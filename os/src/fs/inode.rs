//! `Arc<Inode>` -> `OSInodeInner`: In order to open files concurrently
//! we need to wrap `Inode` into `Arc`,but `Mutex` in `Inode` prevents
//! file systems from being accessed simultaneously
//!
//! `UPSafeCell<OSInodeInner>` -> `OSInode`: for static `ROOT_INODE`,we
//! need to wrap `OSInodeInner` into `UPSafeCell`
use super::{File, Stat, StatMode};
use crate::drivers::BLOCK_DEVICE;
use crate::mm::UserBuffer;
use crate::sync::UPSafeCell;
use alloc::sync::Arc;
use alloc::vec::Vec;
use bitflags::*;
use easy_fs::{EasyFileSystem, Inode};
use lazy_static::*;

/// inode in memory
/// A wrapper around a filesystem inode
/// to implement File trait atop
///
/// 站在用户的角度看来，在一个进程中可以使用多种不同的标志来打开一个文件，这会影响到打开的这个文件可以用何种方式被访问。此外，在连续调用 sys_read/write 读写一个文件的时候，我们知道进程中也存在着一个文件读写的当前偏移量，它也随着文件读写的进行而被不断更新。这些用户视角中的文件系统抽象特征需要内核来实现，与进程有很大的关系，而 easy-fs 文件系统不必涉及这些与进程结合紧密的属性。因此，我们需要将 easy-fs 提供的 Inode 加上上述信息，进一步封装为 OS 中的索引节点 OSInode
pub struct OSInode {
    readable: bool,
    writable: bool,
    inner: UPSafeCell<OSInodeInner>,
}
/// The OS inode inner in 'UPSafeCell'
pub struct OSInodeInner {
    offset: usize, // sys_read/write期间维护的偏移量offset，表示当前操作到了哪个字节偏移位置
    inode: Arc<Inode>,
}

impl OSInode {
    /// create a new inode in memory
    pub fn new(readable: bool, writable: bool, inode: Arc<Inode>) -> Self {
        Self {
            readable,
            writable,
            inner: unsafe { UPSafeCell::new(OSInodeInner { offset: 0, inode }) },
        }
    }
    /// read all data from the inode
    pub fn read_all(&self) -> Vec<u8> {
        let mut inner = self.inner.exclusive_access();
        let mut buffer: Vec<u8> = Vec::with_capacity(512);
        buffer.resize(512, 0);
        let mut v: Vec<u8> = Vec::new();
        loop {
            let len = inner.inode.read_at(inner.offset, &mut buffer);
            if len == 0 {
                break;
            }
            inner.offset += len;
            v.extend_from_slice(&buffer[..len]);
        }
        v
    }
}

lazy_static! {
    pub static ref ROOT_INODE: Arc<Inode> = {
        // 从块设备 BLOCK_DEVICE 上打开文件系统
        let efs = EasyFileSystem::open(BLOCK_DEVICE.clone());
        // 从文件系统中获取根目录的 inode
        Arc::new(EasyFileSystem::root_inode(&efs))
    };
}

/// List all apps in the root directory
pub fn list_apps() {
    println!("/**** APPS ****");
    for app in ROOT_INODE.ls() {
        println!("{}", app);
    }
    println!("**************/");
}

bitflags! {
    ///  The flags argument to the open() system call is constructed by ORing together zero or more of the following values:
    pub struct OpenFlags: u32 {
        /// readyonly
        const RDONLY = 0;
        /// writeonly
        const WRONLY = 1 << 0;
        /// read and write
        const RDWR = 1 << 1;
        /// create new file
        const CREATE = 1 << 9;
        /// truncate file size to 0
        const TRUNC = 1 << 10;
    }
}

impl OpenFlags {
    /// Do not check validity for simplicity
    /// Return (readable, writable)
    pub fn read_write(&self) -> (bool, bool) {
        if self.is_empty() {
            (true, false)
        } else if self.contains(Self::WRONLY) {
            (false, true)
        } else {
            (true, true)
        }
    }
}

/// Open a file
pub fn open_file(name: &str, flags: OpenFlags) -> Option<Arc<OSInode>> {
    trace!("/*** Opening file {}", name);
    let (readable, writable) = flags.read_write();
    if flags.contains(OpenFlags::CREATE) {
        if let Some(inode) = ROOT_INODE.find(name) {
            // clear size
            inode.clear();
            Some(Arc::new(OSInode::new(readable, writable, inode)))
        } else {
            // create file
            ROOT_INODE
                .create(name)
                .map(|inode| Arc::new(OSInode::new(readable, writable, inode)))
        }
    } else {
        ROOT_INODE.find(name).map(|inode| {
            if flags.contains(OpenFlags::TRUNC) {
                inode.clear();
            }
            Arc::new(OSInode::new(readable, writable, inode))
        })
    }
}

/// sys_linkat的实现：为old_name生成一个新的硬链接，硬链接的路径为new_name
pub fn linkat<'a>(old_path: &'a str, new_path: &'a str) -> isize {
    trace!("/*** Linkat *** {}, {}", old_path, new_path);
    // 1. 搜索old_name对应的路径，找到其inode
    // 由于扁平化的设计，这里只需要在ROOT_INODE中查找即可
    if let Some(old_inode) = ROOT_INODE.find(old_path) {
        // 2. 搜索new_name，同样由于扁平化设计，这里已经可以确认是ROOT_INODE
        let res = ROOT_INODE.add_link(new_path, Arc::clone(&old_inode));
        res
    } else {
        // 搜索不到说明old_name不存在，返回-1
        -1
    }
}

/// sys_unlinkat的实现：将name对应的硬链接unlink
pub fn unlinkat(path: &str) -> isize {
    trace!("/*** Unlinkat *** {}", path);
    // 1. 获取path对应的文件的目录
    // 由于扁平化的设计，这里的目录永远是ROOT_INODE
    let parent_inode = &ROOT_INODE;
    // 2. 查询path对应的文件名name，由于扁平化设计，name就是path
    let name = path;
    // 进行实际的unlinkat
    parent_inode.remove_link(name)
}


/// 在 read/write 的全程需要获取 OSInode 的互斥锁，保证两个进程无法同时访问同个文件
impl File for OSInode {
    fn readable(&self) -> bool {
        self.readable
    }
    fn writable(&self) -> bool {
        self.writable
    }
    fn read(&self, mut buf: UserBuffer) -> usize {
        let mut inner = self.inner.exclusive_access();
        let mut total_read_size = 0usize;
        for slice in buf.buffers.iter_mut() {
            let read_size = inner.inode.read_at(inner.offset, *slice);
            if read_size == 0 {
                break;
            }
            inner.offset += read_size;
            total_read_size += read_size;
        }
        total_read_size
    }
    fn write(&self, buf: UserBuffer) -> usize {
        let mut inner = self.inner.exclusive_access();
        let mut total_write_size = 0usize;
        for slice in buf.buffers.iter() {
            let write_size = inner.inode.write_at(inner.offset, *slice);
            assert_eq!(write_size, slice.len());
            inner.offset += write_size;
            total_write_size += write_size;
        }
        total_write_size
    }

    fn stat(&self) -> Stat {
        let inner = self.inner.exclusive_access();
        let ino =  inner.inode.inode_id();
        trace!("/*** Stat *** inode_id = {}", ino);

        let stat_mode = if inner.inode.is_dir() {
            StatMode::DIR
        } else if inner.inode.is_file() {
            StatMode::FILE
        } else {
            StatMode::NULL
        };

        let nlink = inner.inode.nlink();
        let stat = Stat::new(ino as u64, stat_mode, nlink);

        stat
    }
}
