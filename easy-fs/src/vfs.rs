use super::{block_cache_sync_all, get_block_cache, BlockDevice, DirEntry, DiskInode, DiskInodeType, EasyFileSystem, DIRENT_SZ, FILE_NOT_EXIST};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use spin::{Mutex, MutexGuard};
/// Virtual filesystem layer over easy-fs: 位于easy-fs五层架构中的第5层
///
/// EasyFileSystem 实现了磁盘布局并能够将磁盘块有效的管理起来。
/// 但是对于文件系统的使用者而言，他们往往不关心磁盘布局是如何实现的，而是更希望能够直接看到目录树结构中逻辑上的文件和目录。
/// 为此需要设计索引节点 Inode 暴露给文件系统的使用者，让他们能够直接对文件和目录进行操作。
/// Inode 和 DiskInode 的区别从它们的名字中就可以看出： DiskInode 放在磁盘块中比较固定的位置，而 Inode 是放在**内存**中的记录文件索引节点信息的数据结构
pub struct Inode {
    /// 该Inode对应的DiskInode所在的块的block_id
    block_id: usize,
    /// 该Inode对应的DiskInode所在的块的内部偏移：block_offset
    block_offset: usize,
    /// fs 是指向 EasyFileSystem 的一个指针，对Inode的种种操作实际上都是要通过底层的文件系统来完成
    fs: Arc<Mutex<EasyFileSystem>>,
    /// 该Inode所在的文件系统所在的块设备
    block_device: Arc<dyn BlockDevice>,
}

/// 包括 find 在内，所有暴露给文件系统的使用者的文件系统操作（还包括接下来将要介绍的几种），全程均需持有 EasyFileSystem 的互斥锁（相对而言，文件系统内部的操作，如之前的 Inode::new 或是上面的 find_inode_id ，都是假定在已持有 efs 锁的情况下才被调用的，因此它们不应尝试获取锁）。这能够保证在多核情况下，同时最多只能有一个核在进行文件系统相关操作。这样也许会带来一些不必要的性能损失，但我们目前暂时先这样做。如果我们在这里加锁的话，其实就能够保证块缓存的互斥访问了
impl Inode {
    /// Create a vfs inode
    pub fn new(
        block_id: u32,
        block_offset: usize,
        fs: Arc<Mutex<EasyFileSystem>>,
        block_device: Arc<dyn BlockDevice>,
    ) -> Self {
        Self {
            block_id: block_id as usize,
            block_offset,
            fs,
            block_device,
        }
    }
    /// Call a function over a disk inode to read it
    ///
    /// **调用者额需要保证持有fs锁**
    fn read_disk_inode<V>(&self, f: impl FnOnce(&DiskInode) -> V) -> V {
        get_block_cache(self.block_id, Arc::clone(&self.block_device))
            .lock()
            .read(self.block_offset, f)
    }
    /// Call a function over a disk inode to modify it
    ///
    /// **调用者额需要保证持有fs锁**
    fn modify_disk_inode<V>(&self, f: impl FnOnce(&mut DiskInode) -> V) -> V {
        get_block_cache(self.block_id, Arc::clone(&self.block_device))
            .lock()
            .modify(self.block_offset, f)
    }
    /// Find inode under a disk inode by name
    ///
    /// 同样由于扁平化的实现，这里直接访问一层（甚至也不需要判断目录，只可能是文件），只需要遍历寻找相等的name。
    /// **调用者额需要保证持有fs锁**
    ///
    /// param disk_inode: 目录文件对应的DiskInode
    fn find_inode_id(&self, name: &str, disk_inode: &DiskInode) -> Option<u32> {
        // assert it is a directory
        assert!(disk_inode.is_dir());
        let file_count = (disk_inode.size as usize) / DIRENT_SZ;
        let mut dirent = DirEntry::empty();
        let mut i = 0;
        let mut cur = 0;
        while cur < file_count {
            assert_eq!(
                disk_inode.read_at(DIRENT_SZ * i, dirent.as_bytes_mut(), &self.block_device,),
                DIRENT_SZ,
            );
            // 为了支持简易标记删除操作，遇到标记为不存在的文件应当不计算在文件数量内，只有0xffffffff的才能算作有效文件（夹）
            if dirent.inode_id() == FILE_NOT_EXIST {
                i += 1;
                continue;
            }
            log::info!("name = {}", dirent.name());
            if dirent.name() == name {
                return Some(dirent.inode_id());
            }
            // move to next
            cur += 1;
            i += 1;
        }
        None
    }
    /// Find inode under current inode by name
    ///
    pub fn find(&self, name: &str) -> Option<Arc<Inode>> {
        let fs = self.fs.lock();
        self.do_find(name, &fs)
    }

    /// 执行实际的find逻辑
    ///
    /// **调用者额需要保证持有fs锁**
    fn do_find(&self, name: &str, fs: &MutexGuard<EasyFileSystem>) -> Option<Arc<Inode>> {
        log::trace!("execute do_find: {}", name);
        // EasyFileSystem 是一个扁平化的文件系统，即在目录树上仅有一个目录——那就是作为根节点的根目录。
        self.read_disk_inode(|disk_inode| {
            // 由于没有子目录的存在，这个过程只会进行一次
            self.find_inode_id(name, disk_inode).map(|inode_id| {
                let (block_id, block_offset) = fs.get_disk_inode_pos(inode_id);
                Arc::new(Self::new(
                    block_id,
                    block_offset,
                    self.fs.clone(),
                    self.block_device.clone(),
                ))
            })
        })
    }

    /// Increase the size of a disk inode
    ///
    /// **调用者额需要保证持有fs锁**
    ///
    /// 分配一些用于扩容的数据块并传给 DiskInode::increase_size
    fn increase_size(
        &self,
        new_size: u32,
        disk_inode: &mut DiskInode,
        fs: &mut MutexGuard<EasyFileSystem>,
    ) {
        if new_size < disk_inode.size {
            return;
        }
        let blocks_needed = disk_inode.blocks_num_needed(new_size);
        let mut v: Vec<u32> = Vec::new();
        for _ in 0..blocks_needed {
            v.push(fs.alloc_data());
        }
        disk_inode.increase_size(new_size, v, &self.block_device);
    }
    /// Create inode under current inode by name
    ///
    /// 需要独占fs。由于扁平化的考虑，这个方法只有根目录的 Inode 才会调用
    pub fn create(&self, name: &str) -> Option<Arc<Inode>> {
        log::trace!("execute create: {}", name);
        let mut fs = self.fs.lock();
        // 只查找一层也是因为扁平化的实现原因
        let op = |root_inode: &DiskInode| {
            // assert it is a directory
            assert!(root_inode.is_dir());
            // has the file been created?
            self.find_inode_id(name, root_inode)
        };
        if self.read_disk_inode(op).is_some() { // 文件名已存在
            return None;
        }
        // create a new file
        // alloc a inode with an indirect block
        let new_inode_id = fs.alloc_inode();
        log::debug!("inode_id = {}", new_inode_id);
        // initialize inode
        let (new_inode_block_id, new_inode_block_offset) = fs.get_disk_inode_pos(new_inode_id);
        get_block_cache(new_inode_block_id as usize, Arc::clone(&self.block_device))
            .lock()
            .modify(new_inode_block_offset, |new_inode: &mut DiskInode| {
                new_inode.initialize(DiskInodeType::File);
            });
        // 修改目录项（根目录），使得之后可以索引到
        self.append_dirent(name, new_inode_id, &mut fs);

        let (block_id, block_offset) = fs.get_disk_inode_pos(new_inode_id);
        block_cache_sync_all();
        // return inode
        Some(Arc::new(Self::new(
            block_id,
            block_offset,
            self.fs.clone(),
            self.block_device.clone(),
        )))
        // release efs lock automatically by compiler
    }

    /// 当前节点被视为目录，为其添加目录项
    ///
    /// 使用顺序扫描+标记添加目录项
    ///
    /// 1. **调用者额需要保证持有fs锁**
    /// 2. **调用者需要保证name不重复**
    fn append_dirent(&self, new_name: &str, inode_id: u32, fs: &mut MutexGuard<EasyFileSystem>) {
        self.modify_disk_inode(|disk_inode| {
            // append file in the dirent
            let file_count = (disk_inode.size as usize) / DIRENT_SZ;
            // 到此说明中间没有hole，file_count实际代表了占据的空间
            let new_size = (file_count + 1) * DIRENT_SZ;
            // increase size
            self.increase_size(new_size as u32, disk_inode, fs);
            // 直接在末尾添加
            let dirent = DirEntry::new(new_name, inode_id);
            disk_inode.write_at(
                file_count * DIRENT_SZ,
                dirent.as_bytes(),
                &self.block_device,
            );
        });
    }

    /// 查找并删除指定name的目录项，并返回这个被删除的目录项
    ///
    /// 使用顺序扫描+标记移除目录项
    fn remove_dirent(&self, name: &str, fs: &mut MutexGuard<EasyFileSystem>) -> Option<DirEntry> {
        log::trace!("execute remove_dirent: {}", name);
        self.modify_disk_inode(|disk_inode| {
            // remove file
            let file_count = (disk_inode.size as usize) / DIRENT_SZ;
            let mut dirent = DirEntry::empty();
            for i in 0..file_count {
                assert_eq!(
                        disk_inode.read_at(DIRENT_SZ * i, dirent.as_bytes_mut(), &self.block_device,),
                        DIRENT_SZ,
                    );

                if dirent.name() == name {
                    // 执行删除目录项的逻辑
                    let idx = i;
                    disk_inode.remove_at(DIRENT_SZ * idx, dirent.as_bytes_mut(), &self.block_device);
                    return Some(dirent);
                }
            }

            // 没找到就返回None
            None
        })
    }


    /// List inodes under current inode
    ///
    /// 功能上类似于ls命令，由于扁平化的考虑，这个方法只有根目录的 Inode 才会调用
    ///
    /// return: 文件名组成的Vec
    pub fn ls(&self) -> Vec<String> {
        // 在 ls 操作中，我们虽然获取了 efs 锁，但是这里并不会直接访问 EasyFileSystem 实例，其目的仅仅是锁住该实例避免其他核在同时间的访问造成并发冲突
        let _fs = self.fs.lock();
        self.do_ls(&_fs)
    }

    /// 执行实际的ls操作
    ///
    /// **调用者额需要保证持有fs锁**
    fn do_ls(&self, _fs: &MutexGuard<EasyFileSystem>) -> Vec<String> {
        self.read_disk_inode(|disk_inode| {
            let file_count = (disk_inode.size as usize) / DIRENT_SZ;
            let mut v: Vec<String> = Vec::new();
            for i in 0..file_count {
                let mut dirent = DirEntry::empty();
                assert_eq!(
                    disk_inode.read_at(i * DIRENT_SZ, dirent.as_bytes_mut(), &self.block_device,),
                    DIRENT_SZ,
                );
                v.push(String::from(dirent.name()));
            }
            v
        })
    }


    /// Read data from current inode
    pub fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        let _fs = self.fs.lock();
        self.read_disk_inode(|disk_inode| disk_inode.read_at(offset, buf, &self.block_device))
    }
    /// Write data to current inode
    ///
    /// 这里的语义是：从offset开始，将后续的内容覆盖
    pub fn write_at(&self, offset: usize, buf: &[u8]) -> usize {
        let mut fs = self.fs.lock();
        let size = self.modify_disk_inode(|disk_inode| {
            // 注意在执行 DiskInode::write_at 之前先调用 increase_size 对自身进行扩容
            self.increase_size((offset + buf.len()) as u32, disk_inode, &mut fs);
            disk_inode.write_at(offset, buf, &self.block_device)
        });
        block_cache_sync_all();
        size
    }
    /// Clear the data in current inode
    pub fn clear(&self) {
        let mut fs = self.fs.lock();
        self.do_clear(&mut fs);
        block_cache_sync_all();
    }

    /// 执行实际的清除操作
    ///
    /// 1. **调用者额需要保证持有fs锁**
    /// 2. **调用者负责调用sync将缓冲区数据刷新到磁盘**
    fn do_clear(&self, fs: &mut MutexGuard<EasyFileSystem>) {
        log::trace!("execute do_clear");
        self.modify_disk_inode(|disk_inode| {
            let size = disk_inode.size;
            let data_blocks_dealloc = disk_inode.clear_size(&self.block_device);
            assert!(data_blocks_dealloc.len() == DiskInode::total_blocks(size) as usize);
            // 回收的动作由调用clear_size的函数完成
            for data_block in data_blocks_dealloc.into_iter() {
                fs.dealloc_data(data_block);
            }
        });
    }

    /// inode id of this inode
    pub fn inode_id(&self) -> u32 {
        let fs = self.fs.lock();
        fs.compute_inode_id(self.block_id as u32, self.block_offset as u32)
    }

    /// is Dir
    pub fn is_dir(&self) -> bool {
        let _fs = self.fs.lock();
        self.read_disk_inode(|disk_inode| disk_inode.is_dir())
    }

    /// is File
    pub fn is_file(&self) -> bool {
        let _fs = self.fs.lock();
        self.read_disk_inode(|disk_inode| disk_inode.is_file())
    }

    /// 获取硬链接数量
    pub fn nlink(&self) -> u32 {
        let _fs = self.fs.lock();
        self.do_nlink(&_fs)
    }

    /// 执行实际的逻辑：获取硬链接数量
    pub fn do_nlink(&self, _fs: &MutexGuard<EasyFileSystem>) -> u32 {
        self.read_disk_inode(|disk_inode| {disk_inode.nlink})
    }

    /// 用于在当前Inode所表示的目录下创建一个硬链接，也即添加一个目录项（name, ino of old_inode），同时增加old_inode的硬链接数。
    ///
    /// 正常返回0
    ///
    /// 错误：
    /// 1. 如果name已经存在在当前目录下，那么直接返回-1。
    /// 2. 如果old_inode是一个目录，那么返回-2
    pub fn add_link(&self, new_name: &str, old_inode: Arc<Inode>) -> isize {
        log::trace!("execute add_link: {}", new_name);
        let mut fs = self.fs.lock();
        // 不允许对目录创建硬链接
        if old_inode.read_disk_inode(|disk_inode| {disk_inode.is_dir()}) {
            panic!("not allowed to create hard link to directory, old_name=");
        }

        // 1. 为当前inode增加目录项
        // 文件已经存在在当前目录下，直接返回-1
        if self.do_find(new_name, &mut fs).is_some() {
            return -1;
        }

        // 获取old_inode的ino
        let old_inode_id = fs.compute_inode_id(old_inode.block_id as u32, old_inode.block_offset as u32);

        // 为当前目录增加目录项
        self.append_dirent(new_name, old_inode_id, &mut fs);

        // 2. 增加old_inode硬链接数量
        old_inode.modify_disk_inode(|disk_inode| {
            // fixme 不考虑nlink overflow的情况
            disk_inode.nlink += 1;
        });


        // flash回磁盘
        block_cache_sync_all();

        // let v = self.do_ls(&fs);
        // log::debug!("ls to check");
        // for s in v.iter() {
        //     log::debug!("{}", s);
        // }

        // 正常返回
        0
    }

    /// 用于在当前Inode所表示的目录下移除一个硬链接，也即删除一个目录项（name , ino of inode），同时减少inode的硬链接数，如果减少到0，还应该删除该inode
    ///
    /// 参数：
    /// - name: &str，表示当前目录项下的文件名
    ///
    /// 正常返回0
    ///
    /// 错误：
    /// 1. 如果name不存在在在当前目录下，那么直接返回-1。
    pub fn remove_link(&self, name: &str) -> isize {
        log::trace!("execute remove_link: {}", name);
        let mut fs = self.fs.lock();
        // 1. 在当前目录下寻找指定文件名的inode
        if let Some(inode) = self.do_find(name, &fs) {
            let inode_id = fs.compute_inode_id(inode.block_id as u32, inode.block_offset as u32);
            let mut to_delete = false;
            // 2. 减少inode的nlink数量
            inode.modify_disk_inode(|disk_inode| {
                disk_inode.nlink -= 1;
                if disk_inode.nlink == 0 {// 如果刚好删除完毕，则直接删除
                    to_delete = true;
                }
            });

            // let v = self.do_ls(&fs);
            // log::debug!("before remove directory entry:");
            // for s in v.iter() {
            //     log::debug!("{}", s);
            // }
            // 3. 移除目录项
            let _dirent = self.remove_dirent(name, &mut fs);

            // 4. 如果是最后一个链接，还需要清空文件的数据、释放inode
            if to_delete {
                log::trace!("clear data and inode, inode_id = {}", inode_id);
                // 清空数据
                inode.do_clear(&mut fs);
                // fixme 回收disk_inode还未完成
                fs.dealloc_inode(inode_id);
            }

            block_cache_sync_all();
            // let v = self.do_ls(&fs);
            // log::debug!("remove directory entry, now ls to check");
            // for s in v.iter() {
            //     log::debug!("{}", s);
            // }
            // 正常返回
            0
        } else {
            // 不存在则返回-1
            log::debug!("name not exist");
            -1
        }
    }
}
