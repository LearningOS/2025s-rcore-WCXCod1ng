//! Allocator for pid, task user resource, kernel stack using a simple recycle strategy.

use super::ProcessControlBlock;
use crate::config::{KERNEL_STACK_SIZE, PAGE_SIZE, TRAMPOLINE, TRAP_CONTEXT_BASE, USER_STACK_SIZE};
use crate::mm::{MapPermission, PhysPageNum, VirtAddr, KERNEL_SPACE};
use crate::sync::UPSafeCell;
use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use lazy_static::*;

/// Allocator with a simple recycle strategy
/// 可用来分配任何usize类型的资源。分配与回收的算法与之前的 PidAllocator 一样，不过分配的内容从 PID 变为一个非负整数的通用标识符，可以用来表示多种不同资源。这个通用整数标识符可以直接用作进程的 PID 和进程内一个线程的 TID 。
pub struct RecycleAllocator {
    current: usize,
    recycled: Vec<usize>,
}

impl RecycleAllocator {
    /// Create a new allocator
    pub fn new() -> Self {
        RecycleAllocator {
            current: 0,
            recycled: Vec::new(),
        }
    }
    /// allocate a new item
    pub fn alloc(&mut self) -> usize {
        if let Some(id) = self.recycled.pop() {
            id
        } else {
            self.current += 1;
            self.current - 1
        }
    }
    /// deallocate an item
    pub fn dealloc(&mut self, id: usize) {
        assert!(id < self.current);
        assert!(
            !self.recycled.iter().any(|i| *i == id),
            "id {} has been deallocated!",
            id
        );
        self.recycled.push(id);
    }
}

lazy_static! {
    /// Glocal allocator for pid
    static ref PID_ALLOCATOR: UPSafeCell<RecycleAllocator> =
        unsafe { UPSafeCell::new(RecycleAllocator::new()) };
    /// Global allocator for kernel stack
    static ref KSTACK_ALLOCATOR: UPSafeCell<RecycleAllocator> =
        unsafe { UPSafeCell::new(RecycleAllocator::new()) };
}

/// The idle task's pid is 0
pub const IDLE_PID: usize = 0;

/// A handle to a pid
pub struct PidHandle(pub usize);

/// Allocate a pid for a process
pub fn pid_alloc() -> PidHandle {
    PidHandle(PID_ALLOCATOR.exclusive_access().alloc())
}

impl Drop for PidHandle {
    fn drop(&mut self) {
        // trace!("drop pid {}", self.0);
        // 当PidHandle离开作用域时，会调用allocator的dealloc方法来回收
        PID_ALLOCATOR.exclusive_access().dealloc(self.0);
    }
}

/// Return (bottom, top) of a kernel stack in kernel space.
/// 内核栈是从内核高地址空间的跳板页下面开始分配，每两个内核栈中间用一个保护页隔开
pub fn kernel_stack_position(kstack_id: usize) -> (usize, usize) {
    let top = TRAMPOLINE - kstack_id * (KERNEL_STACK_SIZE + PAGE_SIZE);
    let bottom = top - KERNEL_STACK_SIZE;
    (bottom, top)
}

/// Kernel stack for a task
/// 这里的task也就是thread，这里内核绑定的内容不再是PID（当然也不是TID），而是专门的一个标识符
pub struct KernelStack(pub usize);

/// Allocate a kernel stack for a task
///
/// 内核栈的分配方式见`kernel_stack_position`的定义，与之前相同，但不同的则是它的位置不再与 PID 或者 TID 挂钩，而是与一种新的内核栈标识符有关。因此这里使用了一个单独的KSTACK_ALLOCATOR进行内核栈标识符进行分配
pub fn kstack_alloc() -> KernelStack {
    // 分配内核栈标识符：kstack_id，它是表示内核栈的唯一标识
    let kstack_id = KSTACK_ALLOCATOR.exclusive_access().alloc();
    let (kstack_bottom, kstack_top) = kernel_stack_position(kstack_id);
    // 在内核地址空间中map，这样才真正能够被使用
    KERNEL_SPACE.exclusive_access().insert_framed_area(
        kstack_bottom.into(),
        kstack_top.into(),
        MapPermission::R | MapPermission::W,
    );
    KernelStack(kstack_id)
}

impl Drop for KernelStack {
    fn drop(&mut self) {
        let (kernel_stack_bottom, _) = kernel_stack_position(self.0);
        let kernel_stack_bottom_va: VirtAddr = kernel_stack_bottom.into();
        // unmap地址空间的area
        KERNEL_SPACE
            .exclusive_access()
            .remove_area_with_start_vpn(kernel_stack_bottom_va.into());
        // 回收内核标识符
        KSTACK_ALLOCATOR.exclusive_access().dealloc(self.0);
    }
}

impl KernelStack {
    /// Push a variable of type T into the top of the KernelStack and return its raw pointer
    #[allow(unused)]
    pub fn push_on_top<T>(&self, value: T) -> *mut T
    where
        T: Sized,
    {
        let kernel_stack_top = self.get_top();
        let ptr_mut = (kernel_stack_top - core::mem::size_of::<T>()) as *mut T;
        unsafe {
            *ptr_mut = value;
        }
        ptr_mut
    }
    /// return the top of the kernel stack
    pub fn get_top(&self) -> usize {
        let (_, kernel_stack_top) = kernel_stack_position(self.0);
        kernel_stack_top
    }
}

/// User Resource for a task
/// 每个线程独占的用户态资源，由于线程TID、用户栈、trap上下文和线程的声明周期相同，可以将其打包到一起进行统一的分配和回收，于是就有了TaskUserRes（注意内核栈不包括在内，它需要单独处理）
pub struct TaskUserRes {
    /// task id
    pub tid: usize,
    /// user stack base
    /// 每个线程都有自己独立的用户栈，它在所属进程的地址空间中的位置可由 TID 计算得到
    pub ustack_base: usize,
    /// process belongs to
    /// 由于线程中的资源（特别是用户栈和trap上下文都需要在地址空间上映射）实际都在PCB中，所以TaskUserRes需要存储PCB的引用
    pub process: Weak<ProcessControlBlock>,
}
/// Return the bottom addr (low addr) of the trap context for a task
/// 线程的trap上下文是从进程的高地址区域（存储trampoline）开始向低增长的，每个trap上下文占据一个页面（4KB）
fn trap_cx_bottom_from_tid(tid: usize) -> usize {
    TRAP_CONTEXT_BASE - tid * PAGE_SIZE
}
/// Return the bottom addr (high addr) of the user stack for a task
/// 用户栈上下文则是从进程的USTACK_BASE开始，逐渐向上增长（每两个线程栈之间使用一个页面间隔——4KB），注意它返回的是一个虚拟地址，因此可以通过into转化为一个VA
fn ustack_bottom_from_tid(ustack_base: usize, tid: usize) -> usize {
    ustack_base + tid * (PAGE_SIZE + USER_STACK_SIZE)
}

impl TaskUserRes {
    /// Create a new TaskUserRes (Task User Resource)
    /// 要求传入的ustack_base必须指定好，可以从PCB中获取到
    pub fn new(
        process: Arc<ProcessControlBlock>,
        ustack_base: usize,
        alloc_user_res: bool,
    ) -> Self {
        // 分配一个tid
        let tid = process.inner_exclusive_access().alloc_tid();
        let task_user_res = Self {
            tid,
            ustack_base,
            process: Arc::downgrade(&process),
        };
        if alloc_user_res {
            // 增加一个flag的原因是：并非所有情况调用TaskUserRes::new就一定要重新分配资源，例如fork子进程的时候，在fork的执行内部子进程所需要的资源已经从父进程复制了一份，而fork还需要创建一个主线程，这个主线程就不再需要分配资源了，直接引用即可
            task_user_res.alloc_user_res();
        }
        task_user_res
    }
    /// Allocate user resource for a task
    pub fn alloc_user_res(&self) {
        let process = self.process.upgrade().unwrap();
        let mut process_inner = process.inner_exclusive_access();
        // alloc user stack
        let ustack_bottom = ustack_bottom_from_tid(self.ustack_base, self.tid);
        let ustack_top = ustack_bottom + USER_STACK_SIZE;
        // 映射用户栈到线程所在的进程的地址空间中
        process_inner.memory_set.insert_framed_area(
            ustack_bottom.into(),
            ustack_top.into(),
            MapPermission::R | MapPermission::W | MapPermission::U,
        );
        // alloc trap_cx
        let trap_cx_bottom = trap_cx_bottom_from_tid(self.tid);
        let trap_cx_top = trap_cx_bottom + PAGE_SIZE;
        // 同样映射trap上下文到线程所在的进程的地址空间中
        process_inner.memory_set.insert_framed_area(
            trap_cx_bottom.into(),
            trap_cx_top.into(),
            MapPermission::R | MapPermission::W,
        );
    }
    /// Deallocate user resource for a task
    fn dealloc_user_res(&self) {
        // dealloc tid
        let process = self.process.upgrade().unwrap();
        let mut process_inner = process.inner_exclusive_access();
        // dealloc ustack manually
        let ustack_bottom_va: VirtAddr = ustack_bottom_from_tid(self.ustack_base, self.tid).into();
        process_inner
            .memory_set
            .remove_area_with_start_vpn(ustack_bottom_va.into());
        // dealloc trap_cx manually
        let trap_cx_bottom_va: VirtAddr = trap_cx_bottom_from_tid(self.tid).into();
        process_inner
            .memory_set
            .remove_area_with_start_vpn(trap_cx_bottom_va.into());
    }

    #[allow(unused)]
    /// alloc task id
    pub fn alloc_tid(&mut self) {
        self.tid = self
            .process
            .upgrade()
            .unwrap()
            .inner_exclusive_access()
            .alloc_tid();
    }
    /// dealloc task id
    pub fn dealloc_tid(&self) {
        let process = self.process.upgrade().unwrap();
        let mut process_inner = process.inner_exclusive_access();
        process_inner.dealloc_tid(self.tid);
    }
    /// The bottom usr vaddr (low addr) of the trap context for a task with tid
    pub fn trap_cx_user_va(&self) -> usize {
        trap_cx_bottom_from_tid(self.tid)
    }
    /// The physical page number(ppn) of the trap context for a task with tid
    pub fn trap_cx_ppn(&self) -> PhysPageNum {
        let process = self.process.upgrade().unwrap();
        let process_inner = process.inner_exclusive_access();
        let trap_cx_bottom_va: VirtAddr = trap_cx_bottom_from_tid(self.tid).into();
        process_inner
            .memory_set
            .translate(trap_cx_bottom_va.into())
            .unwrap()
            .ppn()
    }
    /// the bottom addr (low addr) of the user stack for a task
    pub fn ustack_base(&self) -> usize {
        self.ustack_base
    }
    /// the top addr (high addr) of the user stack for a task
    pub fn ustack_top(&self) -> usize {
        ustack_bottom_from_tid(self.ustack_base, self.tid) + USER_STACK_SIZE
    }
}

impl Drop for TaskUserRes {
    fn drop(&mut self) {
        self.dealloc_tid();
        self.dealloc_user_res();
    }
}
