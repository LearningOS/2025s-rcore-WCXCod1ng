//! Process management syscalls
use crate::{
    task::{exit_current_and_run_next, suspend_current_and_run_next, get_syscall_count},
    timer::get_time_us,
};

#[repr(C)]
#[derive(Debug)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

/// task exits and submit an exit code
pub fn sys_exit(exit_code: i32) -> ! {
    trace!("[kernel] Application exited with code {}", exit_code);
    exit_current_and_run_next();
    panic!("Unreachable in sys_exit!");
}

/// current task gives up resources for other tasks
pub fn sys_yield() -> isize {
    trace!("kernel: sys_yield");
    suspend_current_and_run_next();
    0
}

/// get time with second and microsecond
pub fn sys_get_time(ts: *mut TimeVal, _tz: usize) -> isize {
    trace!("kernel: sys_get_time");
    let us = get_time_us();
    unsafe {
        *ts = TimeVal {
            sec: us / 1_000_000,
            usec: us % 1_000_000,
        };
    }
    0
}

// TODO: implement the syscall
pub fn sys_trace(_trace_request: usize, _id: usize, _data: usize) -> isize {
    trace!("kernel: sys_trace {}", _id);
    match _trace_request {
        0 => {
            let ptr = _id as *const u8;
            let value: isize;
            unsafe {
                value = core::ptr::read_volatile(ptr) as isize;
                // value = *ptr as isize;
            }
            value
        },
        1 => {
            let ptr = _id as *mut u8;
            let value = _data as u8;
            unsafe {
                // 使用write_volatile而非简单赋值，flush缓存
                // *ptr = value;
                core::ptr::write_volatile(ptr, value);
            }
            0
        },
        2 => get_syscall_count(_id),
        _ => -1,
    }
}
