use crate::sync::{is_safe, Condvar, Mutex, MutexBlocking, MutexSpin, Semaphore};
use crate::task::{block_current_and_run_next, current_process, current_task};
use crate::timer::{add_timer, get_time_ms};
use alloc::sync::Arc;
use alloc::vec;

/// sleep syscall
///
/// 注意这个过程中线程控制块是如何流动的：
/// 它被复制了一份并移动到 TimerCondVar 中，此时在处理器管理结构 PROCESSOR 中还有一份。
/// 而在调用 block_current_and_run_next 阻塞当前线程之后， PROCESSOR 中的那一份就被移除了。
/// 此后直到线程被唤醒之前，线程控制块都只存在于 TimerCondVar 中
pub fn sys_sleep(ms: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_sleep",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    // 超时时间
    let expire_ms = get_time_ms() + ms;
    // 这里是copy了一份task，此时有两份（一份在这里，另一份还在PROCESSOR中）
    let task = current_task().unwrap();
    // 增加一个TimerCondVar并加到全局堆中
    add_timer(expire_ms, task);
    // block执行后PROCESSOR中的那份被回收了，所以函数执行后只有一份TaskControlBlock了（在TimerCondVar中）
    block_current_and_run_next();
    0
}
/// mutex create syscall
///
/// 找到第一个空闲的槽位（如果没有则新增一个）并将指定类型的锁的实例插入进来
pub fn sys_mutex_create(blocking: bool) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_mutex_create",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );

    let process = current_process();
    // 根据参数构建合适的锁实现
    let mutex: Option<Arc<dyn Mutex>> = if !blocking {
        Some(Arc::new(MutexSpin::new()))
    } else {
        Some(Arc::new(MutexBlocking::new()))
    };
    let mut process_inner = process.inner_exclusive_access();

    let n = process_inner.tasks.len();

    if let Some(id) = process_inner
        .mutex_list
        .iter()
        .enumerate()
        .find(|(_, item)| item.is_none())
        .map(|(id, _)| id)
    {
        process_inner.mutex_list[id] = mutex;

        for i in 0..n {
            process_inner.mutex_allocation[i][id] = 0; // 由于是新建了一个锁，所以每个线程都没有在上面分配
        }

        id as isize
    } else {
        process_inner.mutex_list.push(mutex);

        // 维护死锁检测数据结构
        for i in 0..n {
            process_inner.mutex_allocation[i].push(0);
            // process_inner.mutex_need[i].push(0);
        }

        process_inner.mutex_list.len() as isize - 1
    }
}
/// mutex lock syscall
///
/// 参数类似于文件描述符
///
/// 现在对其进行增强，使得能够完成死锁检测
pub fn sys_mutex_lock(mutex_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_mutex_lock",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let process_inner = process.inner_exclusive_access();
    let tid = current_task().unwrap().get_tid();
    // 先检测是否开启了死锁检测，如果开启了，则需要先进行死锁检测
    if process_inner.is_deadlock_detect_enabled() {
        // 死锁检测
        // 1. init
        let n = process_inner.tasks.len();
        let m = process_inner.mutex_list.len();
        let mut available = vec![0usize; n];
        let mut need = vec![vec![0usize; m]; n];
        for (i, item) in process_inner.mutex_list.iter().enumerate() {
            if item.as_ref().is_some_and(|mutex| !mutex.locked()) {
                available[i] = 1; // 只有锁存在，并且没有被上锁，可用数才为1
            } else {
                // available[i] = 0;
            }
        }
        // need
        for j in 0..m {
            let blocked_tids = process_inner.mutex_list[j].as_ref().unwrap().blocked_tids();
            for &blocked_tid in blocked_tids.iter() {
                need[blocked_tid][j] = 1; // 被阻塞住的线程表示还需要这个资源，那么就是1，否则是0
            }
        }
        // 当前线程需要mutex_id
        need[tid][mutex_id] = 1;

        // 打印结果
        debug!("available = {:?}", available);
        debug!("allocation = {:?}", &process_inner.mutex_allocation);
        debug!("need = {:?}", need);

        // 2. 死锁检测
        if !is_safe(&process_inner.mutex_allocation, &available, &need) {
            return -0xdead;
        }
    }

    // 找到这个锁，并且上锁
    let mutex = Arc::clone(process_inner.mutex_list[mutex_id].as_ref().unwrap());
    drop(process_inner);
    drop(process);
    mutex.lock();
    // update deadlock detection structure
    let process = current_process();
    let mut process_inner = process.inner_exclusive_access();
    if process_inner.is_deadlock_detect_enabled() {
        process_inner.mutex_allocation[tid][mutex_id] = 1;
    }
    0
}

/// mutex unlock syscall
pub fn sys_mutex_unlock(mutex_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_mutex_unlock",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let process_inner = process.inner_exclusive_access();
    let tid = current_task().unwrap().get_tid();
    let mutex = Arc::clone(process_inner.mutex_list[mutex_id].as_ref().unwrap());
    drop(process_inner);
    drop(process);
    mutex.unlock();
    // update deadlock detection structure
    let process = current_process();
    let mut process_inner = process.inner_exclusive_access();
    if process_inner.is_deadlock_detect_enabled() {
        process_inner.mutex_allocation[tid][mutex_id] = 0;
    }
    0
}
/// semaphore create syscall
pub fn sys_semaphore_create(res_count: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_semaphore_create",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let mut process_inner = process.inner_exclusive_access();

    let n = process_inner.tasks.len();

    let id = if let Some(id) = process_inner
        .semaphore_list
        .iter()
        .enumerate()
        .find(|(_, item)| item.is_none())
        .map(|(id, _)| id)
    {
        process_inner.semaphore_list[id] = Some(Arc::new(Semaphore::new(res_count)));

        for i in 0..n {
            process_inner.semaphore_allocation[i][id] = 0; // 由于是新建了一个sem，每个线程都没有在上面分配
        }

        id
    } else {
        process_inner
            .semaphore_list
            .push(Some(Arc::new(Semaphore::new(res_count))));

        // 维护死锁检测数据结构
        for i in 0..n {
            process_inner.semaphore_allocation[i].push(0);
            // process_inner.mutex_need[i].push(0);
        }

        process_inner.semaphore_list.len() - 1
    };
    id as isize
}
/// semaphore up syscall
pub fn sys_semaphore_up(sem_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_semaphore_up",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let process_inner = process.inner_exclusive_access();
    let tid = current_task().unwrap().get_tid();
    let sem = Arc::clone(process_inner.semaphore_list[sem_id].as_ref().unwrap());
    drop(process_inner);
    sem.up();
    // update deadlock detection structure
    let process = current_process();
    let mut process_inner = process.inner_exclusive_access();
    if process_inner.is_deadlock_detect_enabled() {
        process_inner.semaphore_allocation[tid][sem_id] -= 1;
    }
    0
}
/// semaphore down syscall
pub fn sys_semaphore_down(sem_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_semaphore_down",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let mut process_inner = process.inner_exclusive_access();

    let tid = current_task().unwrap().get_tid();
    // 先检测是否开启了死锁检测，如果开启了，则需要先进行死锁检测
    if process_inner.is_deadlock_detect_enabled() {
        // 死锁检测
        // 1. init
        let n = process_inner.tasks.len();
        let m = process_inner.semaphore_list.len();
        let mut available = vec![0usize; n];
        let mut need = vec![vec![0usize; m]; n];
        for (i, item) in process_inner.semaphore_list.iter().enumerate() {
            if let Some(sem) = item {
                if sem.count() > 0 {
                    available[i] = sem.count() as usize; // 只有count大于0，才表示可用的资源数量
                }
            } else {
                // available[i] = 0;
            }
        }
        // need
        for j in 0..m {
            let blocked_tids = process_inner.semaphore_list[j].as_ref().unwrap().blocked_tids();
            for &blocked_tid in blocked_tids.iter() {
                need[blocked_tid][j] += 1; // 这些被阻塞的线程都还需要对应的资源
            }
        }
        // 当前线程需要sem_id
        need[tid][sem_id] += 1;
        // 为allocation补充缺失的位置，主要是为了防止下标越界
        for i in 0..n {
            let additional = m - process_inner.semaphore_allocation[i].len();
            process_inner.semaphore_allocation[i].reserve(additional);
            while process_inner.semaphore_allocation[i].len() < m {
                process_inner.semaphore_allocation[i].push(0);
            }
        }

        // 打印结果
        debug!("available = {:?}", available);
        debug!("allocation = {:?}", &process_inner.semaphore_allocation);
        debug!("need = {:?}", need);

        // 2. 死锁检测
        if !is_safe(&process_inner.semaphore_allocation, &available, &need) {
            return -0xdead;
        }
    }
    let sem = Arc::clone(process_inner.semaphore_list[sem_id].as_ref().unwrap());
    drop(process_inner);
    sem.down();
    debug!("{:?} down {:?} success", tid, sem_id);
    // update deadlock detection structure
    let process = current_process();
    let mut process_inner = process.inner_exclusive_access();
    if process_inner.is_deadlock_detect_enabled() {
        process_inner.semaphore_allocation[tid][sem_id] += 1;
    }
    0
}
/// condvar create syscall
pub fn sys_condvar_create() -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_condvar_create",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let mut process_inner = process.inner_exclusive_access();
    let id = if let Some(id) = process_inner
        .condvar_list
        .iter()
        .enumerate()
        .find(|(_, item)| item.is_none())
        .map(|(id, _)| id)
    {
        process_inner.condvar_list[id] = Some(Arc::new(Condvar::new()));
        id
    } else {
        process_inner
            .condvar_list
            .push(Some(Arc::new(Condvar::new())));
        process_inner.condvar_list.len() - 1
    };
    id as isize
}
/// condvar signal syscall
pub fn sys_condvar_signal(condvar_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_condvar_signal",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let process_inner = process.inner_exclusive_access();
    let condvar = Arc::clone(process_inner.condvar_list[condvar_id].as_ref().unwrap());
    drop(process_inner);
    condvar.signal();
    0
}
/// condvar wait syscall
pub fn sys_condvar_wait(condvar_id: usize, mutex_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_condvar_wait",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let process_inner = process.inner_exclusive_access();
    let condvar = Arc::clone(process_inner.condvar_list[condvar_id].as_ref().unwrap());
    let mutex = Arc::clone(process_inner.mutex_list[mutex_id].as_ref().unwrap());
    drop(process_inner);
    condvar.wait(mutex);
    0
}
/// enable deadlock detection syscall
///
/// YOUR JOB: Implement deadlock detection, but might not all in this syscall
pub fn sys_enable_deadlock_detect(_enabled: usize) -> isize {
    trace!("kernel: sys_enable_deadlock_detect NOT IMPLEMENTED");
    let process = current_process();
    let mut process_inner = process.inner_exclusive_access();
    process_inner.enable_deadlock_detect(_enabled)
}
