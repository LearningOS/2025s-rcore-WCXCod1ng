//! Types related to task management & Functions for completely changing TCB
use super::{TaskContext};
use super::{kstack_alloc, pid_alloc, KernelStack, PidHandle};
use crate::config::{BIG_STRIDE, TRAP_CONTEXT_BASE};
use crate::mm::{MemorySet, PhysPageNum, VirtAddr, KERNEL_SPACE};
use crate::sync::UPSafeCell;
use crate::trap::{trap_handler, TrapContext};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::cell::RefMut;
use core::cmp::Ordering;

/// Task control block structure
///
/// Directly save the contents that will not change during running
/// 充当进程控制块的功能
pub struct TaskControlBlock {
    // Immutable
    /// Process identifier
    pub pid: PidHandle,

    /// Kernel stack corresponding to PID
    pub kernel_stack: KernelStack,

    /// Mutable
    inner: UPSafeCell<TaskControlBlockInner>,
}

impl TaskControlBlock {
    /// Get the mutable reference of the inner TCB
    pub fn inner_exclusive_access(&self) -> RefMut<'_, TaskControlBlockInner> {
        self.inner.exclusive_access()
    }
    /// Get the address of app's page table
    pub fn get_user_token(&self) -> usize {
        let inner = self.inner_exclusive_access();
        inner.memory_set.token()
    }
}

pub struct TaskControlBlockInner {
    /// The physical page number of the frame where the trap context is placed
    pub trap_cx_ppn: PhysPageNum,

    /// Application data can only appear in areas
    /// where the application address space is lower than base_size
    /// 地址空间从高到低增长（栈）
    pub base_size: usize,

    /// Save task context
    pub task_cx: TaskContext,

    /// Maintain the execution status of the current process
    pub task_status: TaskStatus,

    /// Application address space
    pub memory_set: MemorySet,

    /// Parent process of the current process.
    /// Weak will not affect the reference count of the parent
    pub parent: Option<Weak<TaskControlBlock>>,

    /// A vector containing TCBs of all child processes of the current process
    pub children: Vec<Arc<TaskControlBlock>>,

    /// It is set when active exit or execution error occurs
    pub exit_code: i32,

    /// Heap bottom
    pub heap_bottom: usize,

    /// Program break
    pub program_brk: usize,

    /// 增加控制进程优先级的参数
    pub stride: usize,

    pub pass: usize,

    pub priority: isize,
}

impl TaskControlBlockInner {
    /// get the trap context
    pub fn get_trap_cx(&self) -> &'static mut TrapContext {
        self.trap_cx_ppn.get_mut()
    }
    /// get the user token
    pub fn get_user_token(&self) -> usize {
        self.memory_set.token()
    }
    fn get_status(&self) -> TaskStatus {
        self.task_status
    }
    pub fn is_zombie(&self) -> bool {
        self.get_status() == TaskStatus::Zombie
    }
}


// 1. 实现 PartialEq
// 我们只需要根据 stride 来判断是否“部分相等”，但通常一个完整的进程控制块
// 应该有唯一的标识符来判断相等。这里为了排序，我们仅关注 stride。
// 但更严谨的做法是比较它们的唯一ID（如果有的话）。
// 如果我们认为只要 stride 相同，它们在堆中的顺序就无所谓，可以这样简化实现。
impl PartialEq for TaskControlBlock {
    fn eq(&self, other: &Self) -> bool {
        self.inner_exclusive_access().stride == other.inner_exclusive_access().stride
    }
}

// 2. 实现 Eq
// Eq 是一个标记 trait，表示相等关系是自反、对称和传递的。
// isize 的比较满足这些条件。
impl Eq for TaskControlBlock {}

// 3. 实现 PartialOrd
// partial_cmp 必须与 cmp 的逻辑一致。
impl PartialOrd for TaskControlBlock {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

// 4. 实现 Ord (核心)
// 这是 BinaryHeap 用来排序的核心。
impl Ord for TaskControlBlock {
    fn cmp(&self, other: &Self) -> Ordering {
        // --- 最小堆实现 ---
        // 我们希望 stride 最小的元素优先级最高（被视为“最大”）。
        // 因此，我们需要反转比较的顺序。
        // self.stride.cmp(&other.stride) 是升序。
        // other.stride.cmp(&self.stride) 是降序，这会使得 stride 小的元素更大。
        let self_inner = self.inner_exclusive_access();
        let other_inner = other.inner_exclusive_access();
        other_inner.stride.cmp(&self_inner.stride)
    }
}

impl TaskControlBlock {
    /// Create a new process
    ///
    /// At present, it is only used for the creation of initproc
    pub fn new(elf_data: &[u8]) -> Self {
        // memory_set with elf program headers/trampoline/trap context/user stack
        let (memory_set, user_sp, entry_point) = MemorySet::from_elf(elf_data);
        // 手动查页表找到应用地址空间中的trap上下文所在的实际物理页帧
        let trap_cx_ppn = memory_set
            .translate(VirtAddr::from(TRAP_CONTEXT_BASE).into())
            .unwrap()
            .ppn();
        // alloc a pid and a kernel stack in kernel space
        let pid_handle = pid_alloc();
        let kernel_stack = kstack_alloc();
        let kernel_stack_top = kernel_stack.get_top();
        // push a task context which goes to trap_return to the top of kernel stack
        let task_control_block = Self {
            pid: pid_handle,
            kernel_stack,
            inner: unsafe {
                UPSafeCell::new(TaskControlBlockInner {
                    trap_cx_ppn,
                    base_size: user_sp,
                    task_cx: TaskContext::goto_trap_return(kernel_stack_top), // 传入栈指针，构造任务上下文
                    task_status: TaskStatus::Ready,
                    memory_set,
                    parent: None,
                    children: Vec::new(),
                    exit_code: 0,
                    heap_bottom: user_sp,
                    program_brk: user_sp,
                    stride: 0,
                    pass: BIG_STRIDE / 16,
                    priority: 16,
                })
            },
        };
        // 由于trap的流程是：陷入 -> trap_handler -> 实际的trap执行 -> restore，我们会复用restore（它需要读取trap上下文）；所以我们可以手动构造一个trap上下文
        // prepare TrapContext in user space
        let trap_cx = task_control_block.inner_exclusive_access().get_trap_cx();
        *trap_cx = TrapContext::app_init_context(
            entry_point,
            user_sp,
            KERNEL_SPACE.exclusive_access().token(),
            kernel_stack_top,
            trap_handler as usize,
        );
        task_control_block
    }

    /// Load a new elf to replace the original application address space and start execution
    pub fn exec(&self, elf_data: &[u8]) {
        // memory_set with elf program headers/trampoline/trap context/user stack
        let (memory_set, user_sp, entry_point) = MemorySet::from_elf(elf_data);
        let trap_cx_ppn = memory_set
            .translate(VirtAddr::from(TRAP_CONTEXT_BASE).into())
            .unwrap()
            .ppn();

        // **** access current TCB exclusively
        let mut inner = self.inner_exclusive_access();
        // substitute memory_set
        inner.memory_set = memory_set;
        // update trap_cx ppn
        inner.trap_cx_ppn = trap_cx_ppn;
        // initialize base_size
        inner.base_size = user_sp;

        // 注意要更新trap上下文，这里选择直接赋值一个新的trap_cx来覆盖旧的，fork没有更新是因为它可以从父进程中复制过来（只需要修改内核栈指针即可）
        // initialize trap_cx
        let trap_cx = inner.get_trap_cx();
        *trap_cx = TrapContext::app_init_context(
            entry_point,
            user_sp,
            KERNEL_SPACE.exclusive_access().token(),
            self.kernel_stack.get_top(), // 这里内核栈指针不需要变动了，因为只是更改了执行文件，而不是新建一个进程
            trap_handler as usize,
        );
        // **** release inner automatically
    }

    /// parent process fork the child process
    pub fn fork(self: &Arc<Self>) -> Arc<Self> {
        // ---- access parent PCB exclusively
        let mut parent_inner = self.inner_exclusive_access();
        // copy user space(include trap context)
        let memory_set = MemorySet::from_existed_user(&parent_inner.memory_set);
        // 由于MemorySet::from_existed_user只会拷贝进程的地址空间，所以还需要单独处理其他内容的拷贝，包括：trap_cx, task_cx, kernal_stack
        let trap_cx_ppn = memory_set
            .translate(VirtAddr::from(TRAP_CONTEXT_BASE).into())
            .unwrap()
            .ppn();
        // alloc a pid and a kernel stack in kernel space
        let pid_handle = pid_alloc();
        let kernel_stack = kstack_alloc();
        let kernel_stack_top = kernel_stack.get_top();
        let task_control_block = Arc::new(TaskControlBlock {
            pid: pid_handle,
            kernel_stack,
            inner: unsafe {
                UPSafeCell::new(TaskControlBlockInner {
                    trap_cx_ppn,
                    base_size: parent_inner.base_size,
                    task_cx: TaskContext::goto_trap_return(kernel_stack_top),
                    task_status: TaskStatus::Ready,
                    memory_set,
                    parent: Some(Arc::downgrade(self)), // 维护父子进程之间的关系
                    children: Vec::new(),
                    exit_code: 0,
                    heap_bottom: parent_inner.heap_bottom,
                    program_brk: parent_inner.program_brk,
                    stride: 0,
                    pass: BIG_STRIDE / 16,
                    priority: 16,
                })
            },
        });
        // 维护父子进程之间的关系
        // add child
        parent_inner.children.push(task_control_block.clone());

        // 由于每一个进程都有一个独立的内核栈（同属于同一个内核空间），所以我们需要将子进程的内核栈修改为正确的位置（新分配的）
        // modify kernel_sp in trap_cx
        // **** access child PCB exclusively
        let trap_cx = task_control_block.inner_exclusive_access().get_trap_cx();
        trap_cx.kernel_sp = kernel_stack_top;
        // return
        task_control_block
        // **** release child PCB
        // ---- release parent PCB
    }

    /// 新增spawn方法，完成fork+exec的功能，返回新创建的进程的pid
    pub fn spawn(self: &Arc<Self>, elf_data: &[u8]) -> Arc<TaskControlBlock> {
        // 1. 创建一个新进程，让其执行elf_data
        let (memory_set, user_sp, entry_point) = MemorySet::from_elf(elf_data);
        // get ppn of trap_cx
        let trap_cx_ppn = memory_set
            .translate(VirtAddr::from(TRAP_CONTEXT_BASE).into())
            .unwrap()
            .ppn();
        // alloc a pid and a kernel stack
        let pid_handle = pid_alloc();
        let kernel_stack = kstack_alloc();
        let kernel_stack_top = kernel_stack.get_top();
        let task_control_block = Arc::new(TaskControlBlock {
            pid: pid_handle,
            kernel_stack,
            inner: unsafe {
                UPSafeCell::new(TaskControlBlockInner {
                    trap_cx_ppn,
                    base_size: user_sp,
                    task_cx: TaskContext::goto_trap_return(kernel_stack_top),
                    task_status: TaskStatus::Ready,
                    memory_set,
                    parent: Some(Arc::downgrade(self)),
                    children: Vec::new(),
                    exit_code: 0,
                    heap_bottom: user_sp,
                    program_brk: user_sp,
                    stride: 0,
                    pass: BIG_STRIDE / 16,
                    priority: 16,
                })
            }
        });

        // 手动填入一个trap_cx
        let trap_cx = task_control_block.inner_exclusive_access().get_trap_cx();
        *trap_cx = TrapContext::app_init_context(
            entry_point,
            user_sp,
            KERNEL_SPACE.exclusive_access().token(),
            kernel_stack_top,
            trap_handler as usize,
        );

        // 2. 维护父子关系
        // 在创建TCB的时候已经维护过了parent，这里只需要维护self的children
        let mut inner = self.inner_exclusive_access();
        inner.children.push(task_control_block.clone());

        // 返回创建的进程
        task_control_block
    }

    /// get pid of process
    pub fn getpid(&self) -> usize {
        self.pid.0
    }

    /// change the location of the program break. return None if failed.
    pub fn change_program_brk(&self, size: i32) -> Option<usize> {
        let mut inner = self.inner_exclusive_access();
        let heap_bottom = inner.heap_bottom;
        let old_break = inner.program_brk;
        let new_brk = inner.program_brk as isize + size as isize;
        if new_brk < heap_bottom as isize {
            return None;
        }
        let result = if size < 0 {
            inner
                .memory_set
                .shrink_to(VirtAddr(heap_bottom), VirtAddr(new_brk as usize))
        } else {
            inner
                .memory_set
                .append_to(VirtAddr(heap_bottom), VirtAddr(new_brk as usize))
        };
        if result {
            inner.program_brk = new_brk as usize;
            Some(old_break)
        } else {
            None
        }
    }
}

#[derive(Copy, Clone, PartialEq)]
/// task status: UnInit, Ready, Running, Exited
pub enum TaskStatus {
    /// uninitialized
    UnInit,
    /// ready to run
    Ready,
    /// running
    Running,
    /// exited
    Zombie,
}
