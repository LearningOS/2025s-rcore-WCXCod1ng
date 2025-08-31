//! Process management syscalls

use core::mem;
use crate::config::PAGE_SIZE;
use crate::mm::{translated_byte_buffer, MapPermission, PageTable, VirtAddr};
use crate::task::{change_program_brk, current_user_token, exit_current_and_run_next, get_syscall_count, suspend_current_and_run_next, with_map};
use crate::timer::get_time_us;

#[repr(C)]
#[derive(Debug)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

/// task exits and submit an exit code
pub fn sys_exit(_exit_code: i32) -> ! {
    trace!("kernel: sys_exit");
    exit_current_and_run_next();
    panic!("Unreachable in sys_exit!");
}

/// current task gives up resources for other tasks
pub fn sys_yield() -> isize {
    trace!("kernel: sys_yield");
    suspend_current_and_run_next();
    0
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

/// TODO: Finish sys_trace to pass testcases
/// HINT: You might reimplement it with virtual memory management.
pub fn sys_trace(_trace_request: usize, _id: usize, _data: usize) -> isize {
    trace!("kernel: sys_trace");
    match _trace_request {
        0 => {
            // VA -> PA

            let page_table = PageTable::from_token(current_user_token());
            let va = VirtAddr::from(_id);
            let vpn = va.floor();
            let offset = va.page_offset();
            match page_table.translate(vpn) {
                None => -1, // 如果没有对应的表项，说明不是自己的地址空间，则应该返回-1
                Some(page_table_entry) => {
                    if !page_table_entry.user_accessible() || !page_table_entry.is_valid() || !page_table_entry.readable() { // 不可读的直接返回-1
                        return -1;
                    }
                    // 执行真正的读取操作（只读取1个字节）
                    let v = page_table_entry.ppn().get_bytes_array()[offset];
                    v as isize
                }
            }
        },
        1 => {
            // VA -> PA
            let page_table = PageTable::from_token(current_user_token());
            let va = VirtAddr::from(_id);
            let vpn = va.floor();
            let offset = va.page_offset();
            match page_table.translate(vpn) {
                None => -1, // 如果没有对应的表项，说明不是自己的地址空间，则应该返回-1
                Some(page_table_entry) => {
                    if !page_table_entry.user_accessible() || !page_table_entry.is_valid() || !page_table_entry.writable() { // 不可写的直接返回-1
                        return -1;
                    }
                    // 将data转化为字节流（一个字节）
                    let src = &mut [0u8; 1];
                    src[0] = _data as u8;
                    // 执行真正的写入操作（只写入1个字节）
                    let dest = &mut page_table_entry.ppn().get_bytes_array()[offset..(offset + 1)];
                    dest.copy_from_slice(src);
                    0
                }
            }
        },
        2 => get_syscall_count(_id),
        _ => -1,
    }
}

// YOUR JOB: Implement mmap.
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

    with_map(|memory_set| {
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

// YOUR JOB: Implement munmap.
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
    with_map(|memory_set| {
        // 执行实际的映射（可能失败，返回-1）
        match memory_set.munmap(start, end) {
            Ok(_) => 0,
            Err(_) => -1
        }
    })

}
/// change data segment size
pub fn sys_sbrk(size: i32) -> isize {
    trace!("kernel: sys_sbrk");
    if let Some(old_brk) = change_program_brk(size) {
        old_brk as isize
    } else {
        -1
    }
}
