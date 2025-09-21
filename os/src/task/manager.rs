//!Implementation of [`TaskManager`]
use super::TaskControlBlock;
use crate::sync::UPSafeCell;
// use alloc::collections::VecDeque;
use alloc::collections::BinaryHeap;
use alloc::sync::Arc;
// use alloc::vec::Vec;
use lazy_static::*;

///A array of `TaskControlBlock` that is thread-safe
pub struct TaskManager {
    // ready_queue: VecDeque<Arc<TaskControlBlock>>, // 存放不能使用stride调度的task
    priority_queue: BinaryHeap<Arc<TaskControlBlock>>, // 存放可以使用stride调度的task
    // ready_queue: VecDeque<Arc<TaskControlBlock>>,
}

/// A simple FIFO scheduler.
impl TaskManager {
    ///Creat an empty TaskManager
    pub fn new() -> Self {
        Self {
            // ready_queue: VecDeque::new(),
            priority_queue: BinaryHeap::new(),
            // ready_queue: VecDeque::new(),
        }
    }
    /// Add process back to ready queue
    pub fn add(&mut self, task: Arc<TaskControlBlock>) {
        // if task.inner_exclusive_access().priority >= 2 { // 只有优先级大于等于2，才能使用stride算法
        //     self.priority_queue.push(task);
        // } else {
        //     self.ready_queue.push_back(task);
        // }

        self.priority_queue.push(task);

        // self.ready_queue.push_back(task);

    }
    /// Take a process out of the ready queue
    pub fn fetch(&mut self) -> Option<Arc<TaskControlBlock>> {
        // 从priority_queue中获取
        let task = self.priority_queue.pop()?;
        // println!("from priority_queue, task = {}", task.pid.0);

        // note 本次task被调度，则要修改其对应的stride——加上步长pass
        let mut inner = task.inner_exclusive_access();
        inner.stride += inner.pass;

        drop(inner); // 要主动drop，因为inner会对task产生借用，而下面的Some(task)则是会移动task

        // // 搜索stride最小的
        // let mut min_idx = 0;
        // let mut min_stride = usize::MAX;
        // for (idx, p) in self.ready_queue.iter().enumerate(){
        //     let inner = p.inner_exclusive_access();
        //     if inner.stride < min_stride {
        //         min_stride = inner.stride;
        //         min_idx = idx;
        //     }
        // }
        //
        // let task = self.ready_queue.remove(min_idx)?;
        //
        // // let task = self.priority_queue.pop()?;
        // // println!("execute an task = {}", task.pid.0);
        //
        // let mut inner = task.inner_exclusive_access();
        // // 适用于stride调度策略，需要更新
        // inner.stride += inner.pass;
        // drop(inner);

        // // 简单的策略，用于debug
        // let task = self.ready_queue.pop_front()?;

        Some(task)
    }
}

lazy_static! {
    /// TASK_MANAGER instance through lazy_static!
    pub static ref TASK_MANAGER: UPSafeCell<TaskManager> =
        unsafe { UPSafeCell::new(TaskManager::new()) };
}

/// Add process to ready queue
pub fn add_task(task: Arc<TaskControlBlock>) {
    //trace!("kernel: TaskManager::add_task");
    TASK_MANAGER.exclusive_access().add(task);
}

/// Take a process out of the ready queue
pub fn fetch_task() -> Option<Arc<TaskControlBlock>> {
    //trace!("kernel: TaskManager::fetch_task");
    TASK_MANAGER.exclusive_access().fetch()
}
