//! Mutex (spin-like and blocking(sleep))

use super::UPSafeCell;
use crate::task::TaskControlBlock;
use crate::task::{block_current_and_run_next, suspend_current_and_run_next};
use crate::task::{current_task, wakeup_task};
use alloc::{collections::VecDeque, sync::Arc};
use alloc::vec::Vec;

/// Mutex trait
pub trait Mutex: Sync + Send {
    /// Lock the mutex
    fn lock(&self);
    /// Unlock the mutex
    fn unlock(&self);

    /// 返回need数据
    fn blocked_tids(&self) -> Vec<usize>;

    /// 是否上锁
    fn locked(&self) -> bool;
}

/// Spinlock Mutex struct
pub struct MutexSpin {
    locked: UPSafeCell<bool>,
}

impl MutexSpin {
    /// Create a new spinlock mutex
    pub fn new() -> Self {
        Self {
            locked: unsafe { UPSafeCell::new(false) },
        }
    }
}

impl Mutex for MutexSpin {
    /// Lock the spinlock mutex
    fn lock(&self) {
        trace!("kernel: MutexSpin::lock");
        loop {
            let mut locked = self.locked.exclusive_access();
            if *locked {
                drop(locked);
                suspend_current_and_run_next();
                continue;
            } else {
                *locked = true;
                return;
            }
        }
    }

    fn unlock(&self) {
        trace!("kernel: MutexSpin::unlock");
        let mut locked = self.locked.exclusive_access();
        *locked = false;
    }

    fn blocked_tids(&self) -> Vec<usize> {
        Vec::new()
    }

    fn locked(&self) -> bool {
        false
    }
}

/// Blocking Mutex struct
///
/// 阻塞机制的锁
pub struct MutexBlocking {
    inner: UPSafeCell<MutexBlockingInner>,
}

/// 和TimerCondVar很像，也是一个队列表示等待该所释放的所有线程
pub struct MutexBlockingInner {
    /// 这里我们仅用到单标记 locked ，为什么无需使用原子指令来保证对于 locked 本身访问的互斥性呢？这其实是因为，RISC-V 架构规定从用户态陷入内核态之后所有（内核态）中断默认被自动屏蔽，也就是说与应用的执行不同， 目前系统调用的执行是不会被中断打断的 。同时，目前我们是在单核上，也 不会有多个 CPU 同时执行系统调用的情况 。在这种情况下，内核态的共享数据访问就仍在 UPSafeCell 的框架之内，只要使用它就能保证互斥访问
    locked: bool,
    wait_queue: VecDeque<Arc<TaskControlBlock>>,
}

impl MutexBlocking {
    /// Create a new blocking mutex
    pub fn new() -> Self {
        trace!("kernel: MutexBlocking::new");
        Self {
            inner: unsafe {
                UPSafeCell::new(MutexBlockingInner {
                    locked: false,
                    wait_queue: VecDeque::new(),
                })
            },
        }
    }
}

impl Mutex for MutexBlocking {
    /// lock the blocking mutex
    ///
    /// 注意这里和用户态的区别，这里如果在阻塞时（阻塞发生在block_current_and_run_next()处）被唤醒，那么会向后执行并结束lock()这个函数。
    /// **只要能够执行，就说明不被阻塞了；换而言之就说明进入了临界区**
    fn lock(&self) {
        trace!("kernel: MutexBlocking::lock");
        let mut mutex_inner = self.inner.exclusive_access();
        // 如果发现已经上锁了就阻塞；否则上锁并进入临界区
        if mutex_inner.locked {
            mutex_inner.wait_queue.push_back(current_task().unwrap());
            drop(mutex_inner);
            block_current_and_run_next();
        } else {
            mutex_inner.locked = true;
        }
    }

    /// unlock the blocking mutex
    ///
    /// 简单起见我们假定当前线程一定持有锁（也就是所有的线程一定将 lock 和 unlock 配对使用），因此断言 locked 为 true 。接下来尝试从阻塞队列中取出一个线程，如果存在的话就将这个线程唤醒
    fn unlock(&self) {
        trace!("kernel: MutexBlocking::unlock");
        let mut mutex_inner = self.inner.exclusive_access();
        assert!(mutex_inner.locked);
        if let Some(waking_task) = mutex_inner.wait_queue.pop_front() {
            // 在此期间 locked 始终为 true ，相当于 释放锁的线程将锁直接移交给这次唤醒的线程。注意这里只能唤醒一个，原因在于lock()的机制，如果唤醒多个就相当于有多个线程都进入了临界区，显然与互斥性矛盾了
            wakeup_task(waking_task);
        } else {
            mutex_inner.locked = false;
        }
    }

    fn blocked_tids(&self) -> Vec<usize> {
        let inner = self.inner.exclusive_access();
        // 将所有还在阻塞的tid都收集起来，这样就能够恢复出need矩阵
        let res = inner.wait_queue.iter()
            .map(|item|{
                item.get_tid()
            })
            .collect();
        res
    }

    fn locked(&self) -> bool {
        self.inner.exclusive_access().locked
    }
}
