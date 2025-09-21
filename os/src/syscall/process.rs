//! Process management syscalls
use alloc::sync::Arc;
use core::mem;
use crate::{
    loader::get_app_data_by_name,
    mm::{translated_refmut, translated_str},
    task::{
        add_task, current_task, current_user_token, exit_current_and_run_next,
        suspend_current_and_run_next,
    },
};
use crate::config::{BIG_STRIDE, PAGE_SIZE};
use crate::mm::{translated_byte_buffer, MapPermission, VirtAddr};
use crate::task::map_memory_set;
use crate::timer::get_time_us;

#[repr(C)]
#[derive(Debug)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

/// task exits and submit an exit code
pub fn sys_exit(exit_code: i32) -> ! {
    trace!("kernel:pid[{}] sys_exit", current_task().unwrap().pid.0);
    exit_current_and_run_next(exit_code);
    panic!("Unreachable in sys_exit!");
}

/// current task gives up resources for other tasks
pub fn sys_yield() -> isize {
    trace!("kernel:pid[{}] sys_yield", current_task().unwrap().pid.0);
    // println!("suspend_current_task:{:?}", current_task().unwrap().pid.0);
    suspend_current_and_run_next();
    0
}

pub fn sys_getpid() -> isize {
    trace!("kernel: sys_getpid pid:{}", current_task().unwrap().pid.0);
    current_task().unwrap().pid.0 as isize
}

pub fn sys_fork() -> isize {
    trace!("kernel:pid[{}] sys_fork", current_task().unwrap().pid.0);
    let current_task = current_task().unwrap();
    let new_task = current_task.fork(); // 子进程
    let new_pid = new_task.pid.0;
    // modify trap context of new_task, because it returns immediately after switching
    let trap_cx = new_task.inner_exclusive_access().get_trap_cx();
    // we do not have to move to next instruction since we have done it before
    // for child process, fork returns 0
    trap_cx.x[10] = 0; // 修改子进程的系统调用返回值为0（x[10]存储系统调用的返回值）
    // add new task to scheduler
    add_task(new_task);
    new_pid as isize // 而父进程的系统调用返回值设置为子进程的pid（修改此不会影响子进程，因为子进程没有调用过sys_fork）
}

pub fn sys_exec(path: *const u8) -> isize {
    trace!("kernel:pid[{}] sys_exec", current_task().unwrap().pid.0);
    let token = current_user_token();
    let path = translated_str(token, path); // 将一个指定地址开始的字符串读取出来（就是app name）
    if let Some(data) = get_app_data_by_name(path.as_str()) {
        let task = current_task().unwrap();
        task.exec(data);
        0
    } else {
        -1
    }
}

/// If there is not a child process whose pid is same as given, return -1.
/// Else if there is a child process but it is still running, return -2.
/// 这里提供的是非阻塞方式的，user_lib需要自行实现阻塞效果
pub fn sys_waitpid(pid: isize, exit_code_ptr: *mut i32) -> isize {
    trace!("kernel::pid[{}] sys_waitpid [{}]", current_task().unwrap().pid.0, pid);
    let task = current_task().unwrap();
    // find a child process

    // ---- access current PCB exclusively
    let mut inner = task.inner_exclusive_access();
    if !inner
        .children
        .iter()
        .any(|p| pid == -1 || pid as usize == p.getpid())
    { // 如果当前的进程不存在一个符合要求的子进程，则返回 -1
        return -1;
        // ---- release current PCB
    }
    let pair = inner.children.iter().enumerate().find(|(_, p)| {
        // 这里使用的find是选择一个，所以pid为-1表示等待一个已经完整的进程（存在即可，不要求任意）
        // ++++ temporarily access child PCB exclusively
        p.inner_exclusive_access().is_zombie() && (pid == -1 || pid as usize == p.getpid())
        // ++++ release child PCB
    });
    if let Some((idx, _)) = pair {
        let child = inner.children.remove(idx);
        // confirm that child will be deallocated after being removed from children list
        assert_eq!(Arc::strong_count(&child), 1);
        let found_pid = child.getpid();
        // ++++ temporarily access child PCB exclusively
        let exit_code = child.inner_exclusive_access().exit_code; // 写入状态响应码
        // ++++ release child PCB
        *translated_refmut(inner.memory_set.token(), exit_code_ptr) = exit_code;
        found_pid as isize
    } else { // 如果至少存在一个，但是其中没有僵尸进程（也即仍未退出）则返回 -2
        -2
    }
    // ---- release current PCB automatically
}

/// YOUR JOB: get time with second and microsecond
/// HINT: You might reimplement it with virtual memory management.
/// HINT: What if [`TimeVal`] is splitted by two pages ?
pub fn sys_get_time(_ts: *mut TimeVal, _tz: usize) -> isize {
    trace!("kernel: sys_get_time");
    let size = mem::size_of::<TimeVal>();
    // 获取时间戳，构造TimeVal
    let ts = get_time_us();
    let kernel_tv = TimeVal {
        sec: ts / 1_000_000,
        usec: ts % 1_000_000,
    };
    // 将TimeVal其转化为字节数组
    let src_buffer: &[u8] = unsafe {
        // 这个unsafe是安全的，因为我们只是创建了一个临时的、只读的
        // 字节视图来查看栈上有效的`kernel_tv`变量。
        core::slice::from_raw_parts(&kernel_tv as *const _ as *const u8, size)
    };
    // let page_table = PageTable::from_token(current_user_token());
    // let ptr = _ts as usize;
    // let vpn = VirtAddr::from(ptr).floor();
    // let ppn = page_table.translate(vpn).unwrap().ppn();
    // 将虚拟地址解析为物理地址（实际上可以写入的切片数组）
    let dest_buffers = translated_byte_buffer(current_user_token(), _ts as *const u8, size);

    // 执行写入操作
    let mut current_pos = 0;
    for dest in dest_buffers {
        let slice_len = dest.len();
        dest.copy_from_slice(&src_buffer[current_pos..(current_pos + slice_len)]);
        current_pos += slice_len;
    }

    0
}

/// YOUR JOB: Implement mmap.
pub fn sys_mmap(_start: usize, _len: usize, _port: usize) -> isize {
    // print!("_start = {}", _start);
    // 没有按页大小对齐则直接返回-1（对齐的含义是地址必须是某个页的起始地址）
    if _start % PAGE_SIZE != 0 {
        return -1;
    }
    // 判断port的有效性
    if (_port & (!0x7)) != 0 || (_port & 0x7) == 0 {
        return -1;
    }

    // 如果len为0，则直接返回
    if _len == 0 {
        return 0;
    }

    let start = VirtAddr::from(_start);
    let end = VirtAddr::from((_start + _len + PAGE_SIZE - 1) / PAGE_SIZE * PAGE_SIZE);

    map_memory_set(|memory_set| {
        // 构造perm：_port X W R
        let mut perm = MapPermission::empty();
        if (_port & 1) != 0 {
            perm |= MapPermission::R;
        }
        if (_port & 2) != 0 {
            perm |= MapPermission::W;
        }
        if (_port & 4) != 0 {
            perm |= MapPermission::X;
        }
        // 增加用户权限
        perm |= MapPermission::U;

        // 执行实际的映射（可能失败，返回-1）
        match memory_set.mmap(start, end, perm) {
            Ok(_) => 0,
            Err(_) => -1
        }
    })
}

/// YOUR JOB: Implement munmap.
/// NOTE 在rCore的课程实验中，正确执行的sys_munmap只考虑唯一且完整的mmap区间，不考虑交叉、截断mmap区间的情况（但是考虑覆盖多个area的情况，而且多个area之间可能存在间隙）
/// NOTE 也不考虑发生错误时内存的恢复和回收
pub fn sys_munmap(_start: usize, _len: usize) -> isize {
    if _start % PAGE_SIZE != 0 {
        return -1;
    }

    if _len == 0 {
        return 0;
    }

    let start = VirtAddr::from(_start);
    let end = VirtAddr::from((_start + _len + PAGE_SIZE - 1) / PAGE_SIZE * PAGE_SIZE);

    // 执行实际的 unmap
    // 执行实际的映射（可能失败，返回-1）
    map_memory_set(|memory_set| {
        // 执行实际的映射（可能失败，返回-1）
        match memory_set.munmap(start, end) {
            Ok(_) => 0,
            Err(_) => -1
        }
    })

}

/// change data segment size
pub fn sys_sbrk(size: i32) -> isize {
    trace!("kernel:pid[{}] sys_sbrk", current_task().unwrap().pid.0);
    if let Some(old_brk) = current_task().unwrap().change_program_brk(size) {
        old_brk as isize
    } else {
        -1
    }
}

/// YOUR JOB: Implement spawn.
/// HINT: fork + exec =/= spawn
pub fn sys_spawn(_path: *const u8) -> isize {
    // trace!(
    //     "kernel:pid[{}] sys_spawn NOT IMPLEMENTED",
    //     current_task().unwrap().pid.0
    // );
    trace!("kernel:pid[{}] sys_spawn", current_task().unwrap().pid.0);

    // check path
    let token = current_user_token();
    let path = translated_str(token, _path);
    if let Some(data) = get_app_data_by_name(path.as_str()) {
        let task = current_task().unwrap();
        let new_task = task.spawn(data);
        let new_pid = new_task.getpid() as isize;
        // 别忘了添加到队列中
        add_task(new_task);

        // 返回新pid
        new_pid
    } else {
        -1
    }
}

// YOUR JOB: Set task priority.
// 设置当前进程优先级为 prio
// 参数：prio 进程优先级，要求 prio >= 2
// 返回值：如果输入合法则返回 prio，否则返回 -1
pub fn sys_set_priority(_prio: isize) -> isize {
    trace!("kernel:pid[{}] sys_set_priority", current_task().unwrap().pid.0);

    if _prio >= 2 {
        // 合法的进程优先级，进行实际的优先级设置
        let task = current_task().unwrap();
        let mut inner = task.inner_exclusive_access();
        inner.priority = _prio;

        // 更新pass
        inner.pass = BIG_STRIDE / _prio as usize;


        drop(inner); // 主动drop

        // // fixme 设置成功时，还需要将其添加到新的任务中
        // suspend_current_and_run_next();


        // // 加入调度队列中
        // add_task(task);

        _prio
    } else {
        // 不合法的优先级，直接返回-1
        -1
    }
}
