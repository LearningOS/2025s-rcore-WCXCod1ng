//! File and filesystem-related syscalls
use crate::fs::{open_file, linkat, unlinkat, OpenFlags, Stat};
use crate::mm::{translated_byte_buffer, translated_refmut, translated_str, UserBuffer};
use crate::task::{current_task, current_user_token};

pub fn sys_write(fd: usize, buf: *const u8, len: usize) -> isize {
    // 有了fs的支持，sys_write就可以指定需要写入的文件了（通过标识符），而不需要只限于之前特定的标准输入输出了
    trace!("kernel:pid[{}] sys_write", current_task().unwrap().pid.0);
    let token = current_user_token();
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    if let Some(file) = &inner.fd_table[fd] {
        if !file.writable() {
            return -1;
        }
        let file = file.clone();
        // release current task TCB manually to avoid multi-borrow
        drop(inner);
        // 由于传入的是buffer的首地址，因此还需要进行地址翻译
        file.write(UserBuffer::new(translated_byte_buffer(token, buf, len))) as isize
    } else {
        -1
    }
}

pub fn sys_read(fd: usize, buf: *const u8, len: usize) -> isize {
    trace!("kernel:pid[{}] sys_read", current_task().unwrap().pid.0);
    let token = current_user_token();
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    if let Some(file) = &inner.fd_table[fd] {
        let file = file.clone();
        if !file.readable() {
            return -1;
        }
        // release current task TCB manually to avoid multi-borrow
        drop(inner);
        trace!("kernel: sys_read .. file.read");
        // 不用担心这里读完后会因为UserBuffer离开作用域而释放buffers（Vec<&[u8]>），因为Vec的元素是引用，实际的数据是存在堆上的，这里仅仅是相当于这些引用离开了作用域，由于它们**不**拥有数据，所以没有副作用
        file.read(UserBuffer::new(translated_byte_buffer(token, buf, len))) as isize
    } else {
        -1
    }
}

pub fn sys_open(path: *const u8, flags: u32) -> isize {
    trace!("kernel:pid[{}] sys_open", current_task().unwrap().pid.0);
    let task = current_task().unwrap();
    let token = current_user_token();
    let path = translated_str(token, path);
    if let Some(inode) = open_file(path.as_str(), OpenFlags::from_bits(flags).unwrap()) {
        let mut inner = task.inner_exclusive_access();
        let fd = inner.alloc_fd(); // 为该进程打开一个文件，并返回它的文件描述符的下标
        inner.fd_table[fd] = Some(inode);
        fd as isize
    } else {
        -1
    }
}

pub fn sys_close(fd: usize) -> isize {
    trace!("kernel:pid[{}] sys_close", current_task().unwrap().pid.0);
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    if inner.fd_table[fd].is_none() {
        return -1;
    }
    // 我们只需将进程控制块中的文件描述符表对应的一项**改为None**代表它已经空闲即可，同时这也会导致内层的引用计数类型 Arc 被销毁，会减少一个文件的引用计数，当引用计数减少到 0 之后文件所占用的资源就会被自动回收
    inner.fd_table[fd].take();
    0
}

/// YOUR JOB: Implement fstat.
pub fn sys_fstat(_fd: usize, _st: *mut Stat) -> isize {
    trace!("kernel:pid[{}] call sys_fstat, fd = {}", current_task().unwrap().pid.0, _fd);
    // 查询_fd所对应的inode编号
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if _fd >= inner.fd_table.len() {
        // 错误的fd
        return -1;
    }
    if let Some(file) = inner.fd_table[_fd].as_ref() {
        let stat = file.stat();
        // release current task TCB manually to avoid multi-borrow
        // current_user_token()内部会使用inner
        drop(inner);
        // 别忘了要先地址转换
        let st = translated_refmut(current_user_token(), _st);
        // 赋值
        st.dev = 0;
        st.ino = stat.ino;
        st.nlink = stat.nlink;
        st.mode = stat.mode;
        // normal case
        return 0;
    }

    -1 // 未打开，错误
}

/// YOUR JOB: Implement linkat.
pub fn sys_linkat(_old_path: *const u8, _new_path: *const u8) -> isize {
    trace!("kernel:pid[{}] call sys_linkat", current_task().unwrap().pid.0);
    // translate va
    let token = current_user_token();
    let old_name = translated_str(token, _old_path);
    let new_name = translated_str(token, _new_path);
    let res = linkat(&old_name, &new_name);
    res
}

/// YOUR JOB: Implement unlinkat.
pub fn sys_unlinkat(_path: *const u8) -> isize {
    trace!("kernel:pid[{}] call sys_unlinkat", current_task().unwrap().pid.0);
    // translate va
    let path = translated_str(current_user_token(), _path);
    let res = unlinkat(&path);
    res
}
