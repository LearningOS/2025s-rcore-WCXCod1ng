use alloc::string::String;
use super::{get_block_cache, BlockDevice, EasyFileSystem, BLOCK_SZ, FILE_NOT_EXIST};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::{Debug, Formatter, Result};
use spin::MutexGuard;

/// 需要将磁盘上的数据组织为若干种不同的磁盘上数据结构，并合理安排它们在磁盘中的位置，layout.rs的作用就是如此
///
/// - 最开始的区域的长度为一个块，其内容是 easy-fs 超级块 (Super Block)。超级块内以魔数的形式提供了文件系统合法性检查功能，同时还可以定位其他连续区域的位置。
/// - 第二个区域是一个索引节点位图，长度为若干个块。它记录了后面的索引节点区域中有哪些索引节点已经被分配出去使用了，而哪些还尚未被分配出去。
/// - 第三个区域是索引节点区域，长度为若干个块。其中的每个块都存储了若干个索引节点。
/// - 第四个区域是一个数据块位图，长度为若干个块。它记录了后面的数据块区域中有哪些数据块已经被分配出去使用了，而哪些还尚未被分配出去。
/// - 最后的区域则是数据块区域，顾名思义，其中的每一个已经分配出去的块保存了文件或目录中的具体数据内容。

/// Magic number for sanity check
const EFS_MAGIC: u32 = 0x3b800001;
/// The max number of direct inodes
/// 现在不能是28了，要根据增加的元数据适当减少以保证完整的
const INODE_DIRECT_COUNT: usize = 27;
/// The max length of inode name
const NAME_LENGTH_LIMIT: usize = 27;
/// The max number of indirect1 inodes
const INODE_INDIRECT1_COUNT: usize = BLOCK_SZ / 4;
/// The max number of indirect2 inodes
const INODE_INDIRECT2_COUNT: usize = INODE_INDIRECT1_COUNT * INODE_INDIRECT1_COUNT;
/// The upper bound of direct inode index
const DIRECT_BOUND: usize = INODE_DIRECT_COUNT;
/// The upper bound of indirect1 inode index
const INDIRECT1_BOUND: usize = DIRECT_BOUND + INODE_INDIRECT1_COUNT;
/// The upper bound of indirect2 inode indexs
#[allow(unused)]
const INDIRECT2_BOUND: usize = INDIRECT1_BOUND + INODE_INDIRECT2_COUNT;
/// Super block of a filesystem: 位于easy-fs五层架构中的第3层
#[repr(C)]
pub struct SuperBlock {
    magic: u32,
    /// 给出文件系统的总块数，注意不等于所在磁盘的总块数，因为文件系统很可能并没有占据整个磁盘
    pub total_blocks: u32,
    /// inode bitmap所占的块数
    pub inode_bitmap_blocks: u32,
    /// inode所占的块数
    pub inode_area_blocks: u32,
    /// data bitmap所占的块数
    pub data_bitmap_blocks: u32,
    /// data所占的块数
    pub data_area_blocks: u32,
}

impl Debug for SuperBlock {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        f.debug_struct("SuperBlock")
            .field("total_blocks", &self.total_blocks)
            .field("inode_bitmap_blocks", &self.inode_bitmap_blocks)
            .field("inode_area_blocks", &self.inode_area_blocks)
            .field("data_bitmap_blocks", &self.data_bitmap_blocks)
            .field("data_area_blocks", &self.data_area_blocks)
            .finish()
    }
}

impl SuperBlock {
    /// Initialize a super block
    pub fn initialize(
        &mut self,
        total_blocks: u32,
        inode_bitmap_blocks: u32,
        inode_area_blocks: u32,
        data_bitmap_blocks: u32,
        data_area_blocks: u32,
    ) {
        *self = Self {
            magic: EFS_MAGIC,
            total_blocks,
            inode_bitmap_blocks,
            inode_area_blocks,
            data_bitmap_blocks,
            data_area_blocks,
        }
    }
    /// Check if a super block is valid using efs magic
    pub fn is_valid(&self) -> bool {
        self.magic == EFS_MAGIC
    }
}
/// Type of a disk inode
#[derive(PartialEq)]
pub enum DiskInodeType {
    File,
    Directory,
}

/// A indirect block
///
/// 抽象成一个u32的数组，数组中的每个元素同样也代表了一个块编号
type IndirectBlock = [u32; BLOCK_SZ / 4];

/// A data block
///
/// 在文件系统看来，数据块没有既定的格式，直接看作是一个字节序列
type DataBlock = [u8; BLOCK_SZ];

/// A disk inode: 位于easy-fs五层架构中的第3层
///
/// 在磁盘上的inode区域，每个块上都保存着若干inode，这里用DiskInode表示
/// 将 DiskInode 的大小设置为 128 字节，每个块正好能够容纳 4 个 DiskInode 。
/// 在后续需要支持更多类型的元数据的时候，可以适当缩减直接索引 direct 的块数，并将节约出来的空间用来存放其他元数据，仍可保证 DiskInode 的总大小为 128 字节
/// 直/间接索引的内容实际上是一个整数，代表了块编号
#[repr(C)]
pub struct DiskInode {
    pub size: u32,
    /// 当文件很小的时候，只需用到直接索引， direct 数组中最多可以指向 INODE_DIRECT_COUNT 个数据块，当取值为 28 的时候，通过直接索引可以找到 14KiB 的内容
    /// 维护硬链接的数量
    pub nlink: u32,
    pub direct: [u32; INODE_DIRECT_COUNT],
    /// 当文件比较大的时候，不仅直接索引的 direct 数组装满，还需要用到一级间接索引 indirect1 。
    /// 它指向一个一级索引块，这个块也位于磁盘布局的数据块区域中。
    /// 这个一级索引块中的每个 u32 都用来指向数据块区域中一个保存该文件内容的数据块，因此，最多能够索引512/4=128个数据块，对应 64KiB 的内容
    pub indirect1: u32,
    /// 当文件大小超过直接索引和一级索引支持的容量上限 78KiB的时候，就需要用到二级间接索引 indirect2 。
    /// 它指向一个位于数据块区域中的二级索引块。二级索引块中的每个 u32 指向一个不同的一级索引块，这些一级索引块也位于数据块区域中。
    /// 因此，通过二级间接索引最多能够索引128 * 64KB = 8MB的内容
    pub indirect2: u32,
    type_: DiskInodeType,
}

impl DiskInode {
    /// Initialize a disk inode, as well as all direct inodes under it
    /// indirect1 and indirect2 block are allocated only when they are needed
    pub fn initialize(&mut self, type_: DiskInodeType) {
        self.size = 0;
        self.nlink = 1; // 别忘了初始化
        self.direct.iter_mut().for_each(|v| *v = 0);
        // indirect1/2 均被初始化为 0 。因为最开始文件内容的大小为 0 字节，并不会用到一级/二级索引
        self.indirect1 = 0;
        self.indirect2 = 0;
        self.type_ = type_;
    }
    /// Whether this inode is a directory
    pub fn is_dir(&self) -> bool {
        self.type_ == DiskInodeType::Directory
    }
    /// Whether this inode is a file
    #[allow(unused)]
    pub fn is_file(&self) -> bool {
        self.type_ == DiskInodeType::File
    }
    /// Return block number correspond to size.
    pub fn data_blocks(&self) -> u32 {
        Self::_data_blocks(self.size)
    }
    fn _data_blocks(size: u32) -> u32 {
        // 计算为了容纳自身 size 字节的内容需要多少个数据块。计算的过程只需用size对BLOCK_SZ向上取整
        (size + BLOCK_SZ as u32 - 1) / BLOCK_SZ as u32
    }
    /// Return number of blocks needed include indirect1/2.
    ///
    /// total_blocks 不仅包含数据块，还需要统计索引块
    pub fn total_blocks(size: u32) -> u32 {
        let data_blocks = Self::_data_blocks(size) as usize;
        let mut total = data_blocks as usize;
        // indirect1
        if data_blocks > INODE_DIRECT_COUNT {
            total += 1;
        }
        // indirect2
        if data_blocks > INDIRECT1_BOUND {
            total += 1;
            // sub indirect1
            total +=
                (data_blocks - INDIRECT1_BOUND + INODE_INDIRECT1_COUNT - 1) / INODE_INDIRECT1_COUNT;
        }
        total as u32
    }
    /// Get the number of data blocks that have to be allocated given the new size of data
    ///
    /// param new_size 表示容量扩充之后的文件大小
    /// return u32: 需要额外多少个数据和索引块
    pub fn blocks_num_needed(&self, new_size: u32) -> u32 {
        assert!(new_size >= self.size);
        Self::total_blocks(new_size) - Self::total_blocks(self.size)
    }
    /// Get id of block given inner id
    ///
    /// get_block_id 方法体现了 DiskInode 最重要的数据块索引功能，它可以从索引中查到它自身用于保存文件内容的第 block_id个数据块的**块编号**
    pub fn get_block_id(&self, inner_id: u32, block_device: &Arc<dyn BlockDevice>) -> u32 {
        let inner_id = inner_id as usize;
        if inner_id < INODE_DIRECT_COUNT {
            // [0, INODE_DIRECT_COUNT)的数据块由直接索引指向
            self.direct[inner_id]
        } else if inner_id < INDIRECT1_BOUND {
            // [INODE_DIRECT_COUNT, INDIRECT1_BOUND)的数据块由一级间接索引指向
            get_block_cache(self.indirect1 as usize, Arc::clone(block_device))
                .lock()
                .read(0, |indirect_block: &IndirectBlock| {
                    indirect_block[inner_id - INODE_DIRECT_COUNT] // 在indirect1指向的间接块中存放的是一系列块id，利用inner_id - INODE_DIRECT_COUNT可以确定该间接块对应下标的内存单元的值（块id）
                })
        } else {
            // [INDIRECT1_BOUND, )的数据块由二级间接索引指向
            // inner_id = INODE_DIRECT_COUNT  + INODE_INDIRECT1_COUNT + offset1 * INODE_INDIRECT1_COUNT + offset2(<INODE_INDIRECT1_COUNT)
            let last = inner_id - INDIRECT1_BOUND; // last = offset1 * INODE_INDIRECT1_COUNT + offset2
            // 先查询二级索引块挂在哪个一级索引块上（offset1）
            let indirect1 = get_block_cache(self.indirect2 as usize, Arc::clone(block_device))
                .lock()
                .read(0, |indirect2: &IndirectBlock| {
                    indirect2[last / INODE_INDIRECT1_COUNT] // offset1
                });
            // 再通过一级所以块找到数据块（编号）
            get_block_cache(indirect1 as usize, Arc::clone(block_device))
                .lock()
                .read(0, |indirect1: &IndirectBlock| {
                    indirect1[last % INODE_INDIRECT1_COUNT] // offset2
                })
        }
    }
    /// Increase the size of current disk inode
    ///
    /// new_blocks 是一个保存了本次容量扩充所需块编号的向量，这些块都是由上层的磁盘块管理器负责分配的
    pub fn increase_size(
        &mut self,
        new_size: u32,
        new_blocks: Vec<u32>,
        block_device: &Arc<dyn BlockDevice>,
    ) {
        let mut current_blocks = self.data_blocks();
        self.size = new_size;
        let mut total_blocks = self.data_blocks();
        let mut new_blocks = new_blocks.into_iter();
        // fill direct
        while current_blocks < total_blocks.min(INODE_DIRECT_COUNT as u32) {
            self.direct[current_blocks as usize] = new_blocks.next().unwrap();
            current_blocks += 1;
        }
        // alloc indirect1
        if total_blocks > INODE_DIRECT_COUNT as u32 {
            if current_blocks == INODE_DIRECT_COUNT as u32 { // 刚来到一级间接索引，分配一个编号
                self.indirect1 = new_blocks.next().unwrap();
            }
            current_blocks -= INODE_DIRECT_COUNT as u32;
            total_blocks -= INODE_DIRECT_COUNT as u32;
        } else {
            return;
        }
        // fill indirect1
        get_block_cache(self.indirect1 as usize, Arc::clone(block_device))
            .lock()
            .modify(0, |indirect1: &mut IndirectBlock| {
                while current_blocks < total_blocks.min(INODE_INDIRECT1_COUNT as u32) { // 一级索引的数据块分配块编号
                    indirect1[current_blocks as usize] = new_blocks.next().unwrap();
                    current_blocks += 1;
                }
            });
        // alloc indirect2
        if total_blocks > INODE_INDIRECT1_COUNT as u32 {
            if current_blocks == INODE_INDIRECT1_COUNT as u32 { // 刚来到二级间接索引，分配一个编号
                self.indirect2 = new_blocks.next().unwrap();
            }
            current_blocks -= INODE_INDIRECT1_COUNT as u32;
            total_blocks -= INODE_INDIRECT1_COUNT as u32;
        } else {
            return;
        }
        // fill indirect2 from (a0, b0) -> (a1, b1)
        let mut a0 = current_blocks as usize / INODE_INDIRECT1_COUNT;
        let mut b0 = current_blocks as usize % INODE_INDIRECT1_COUNT;
        let a1 = total_blocks as usize / INODE_INDIRECT1_COUNT;
        let b1 = total_blocks as usize % INODE_INDIRECT1_COUNT;
        // alloc low-level indirect1
        get_block_cache(self.indirect2 as usize, Arc::clone(block_device))
            .lock()
            .modify(0, |indirect2: &mut IndirectBlock| {
                while (a0 < a1) || (a0 == a1 && b0 < b1) {
                    if b0 == 0 { // 为一级索引块分配编号
                        indirect2[a0] = new_blocks.next().unwrap();
                    }
                    // fill current
                    // 为一级索引对应的数据块分配编号
                    get_block_cache(indirect2[a0] as usize, Arc::clone(block_device))
                        .lock()
                        .modify(0, |indirect1: &mut IndirectBlock| {
                            indirect1[b0] = new_blocks.next().unwrap();
                        });
                    // move to next
                    b0 += 1;
                    if b0 == INODE_INDIRECT1_COUNT {
                        b0 = 0;
                        a0 += 1;
                    }
                }
            });
    }

    /// Clear size to zero and return blocks that should be deallocated.
    /// We will clear the block contents to zero later.
    ///
    /// 返回回收后的块的编号
    pub fn clear_size(&mut self, block_device: &Arc<dyn BlockDevice>) -> Vec<u32> {
        let mut v: Vec<u32> = Vec::new();
        let mut data_blocks = self.data_blocks() as usize;
        self.size = 0;
        let mut current_blocks = 0usize;
        // direct
        while current_blocks < data_blocks.min(INODE_DIRECT_COUNT) {
            v.push(self.direct[current_blocks]);
            self.direct[current_blocks] = 0;
            current_blocks += 1;
        }
        // indirect1 block
        if data_blocks > INODE_DIRECT_COUNT { // 回收一级间接索引块
            v.push(self.indirect1);
            data_blocks -= INODE_DIRECT_COUNT;
            current_blocks = 0;
        } else {
            return v;
        }
        // indirect1
        get_block_cache(self.indirect1 as usize, Arc::clone(block_device))
            .lock()
            .modify(0, |indirect1: &mut IndirectBlock| {
                while current_blocks < data_blocks.min(INODE_INDIRECT1_COUNT) { // 回收一级间接索引对应的数据块
                    v.push(indirect1[current_blocks]);
                    //indirect1[current_blocks] = 0;
                    current_blocks += 1;
                }
            });
        self.indirect1 = 0;
        // indirect2 block
        if data_blocks > INODE_INDIRECT1_COUNT { // 回收二级间接索引块
            v.push(self.indirect2);
            data_blocks -= INODE_INDIRECT1_COUNT;
        } else {
            return v;
        }
        // indirect2
        assert!(data_blocks <= INODE_INDIRECT2_COUNT);
        let a1 = data_blocks / INODE_INDIRECT1_COUNT;
        let b1 = data_blocks % INODE_INDIRECT1_COUNT;
        get_block_cache(self.indirect2 as usize, Arc::clone(block_device))
            .lock()
            .modify(0, |indirect2: &mut IndirectBlock| {
                // full indirect1 blocks
                for entry in indirect2.iter_mut().take(a1) { // 回收每个一级索引块及其对应的数据块
                    v.push(*entry);
                    get_block_cache(*entry as usize, Arc::clone(block_device))
                        .lock()
                        .modify(0, |indirect1: &mut IndirectBlock| {
                            for entry in indirect1.iter() {
                                v.push(*entry);
                            }
                        });
                }
                // last indirect1 block
                if b1 > 0 { // 回收最后一部分的内容
                    v.push(indirect2[a1]);
                    get_block_cache(indirect2[a1] as usize, Arc::clone(block_device))
                        .lock()
                        .modify(0, |indirect1: &mut IndirectBlock| {
                            for entry in indirect1.iter().take(b1) {
                                v.push(*entry);
                            }
                        });
                    //indirect2[a1] = 0;
                }
            });
        self.indirect2 = 0;
        v
    }
    /// Read data from current disk inode
    ///
    /// 通过 DiskInode 来读它索引的那些数据块中的数据。这些数据可以被视为一个字节序列，而每次都是选取其中的一段连续区间进行操作。
    /// 将文件内容从 offset 字节开始的部分读到内存中的缓冲区 buf 中，并返回实际读到的字节数。如果文件剩下的内容还足够多，那么缓冲区会被填满；否则文件剩下的全部内容都会被读到缓冲区中。
    pub fn read_at(
        &self,
        offset: usize,
        buf: &mut [u8],
        block_device: &Arc<dyn BlockDevice>,
    ) -> usize {
        let mut start = offset;
        let end = (offset + buf.len()).min(self.size as usize);
        if start >= end {
            return 0;
        }
        let mut start_block = start / BLOCK_SZ; // start_block 维护着目前是文件内部第多少个数据块
        let mut read_size = 0usize; // 表示当前已经读取了多少个字节
        loop {
            // calculate end of current block
            let mut end_current_block = (start / BLOCK_SZ + 1) * BLOCK_SZ;
            end_current_block = end_current_block.min(end);
            // read and update read size: 一次（至多）读取一整个块，而且至多读取到该块的结束
            let block_read_size = end_current_block - start;
            let dst = &mut buf[read_size..read_size + block_read_size];
            get_block_cache(
                self.get_block_id(start_block as u32, block_device) as usize,
                Arc::clone(block_device),
            )
            .lock()
            .read(0, |data_block: &DataBlock| {
                let src = &data_block[start % BLOCK_SZ..start % BLOCK_SZ + block_read_size]; // 这里取模就是表示从对应块内部的对应偏移处开始copy
                dst.copy_from_slice(src);
            });
            read_size += block_read_size;
            // move to next block
            if end_current_block == end {
                break;
            }
            start_block += 1; // 移动到下一个块中
            start = end_current_block; // start移动到下一个块的开始，意味着第一次可能不是一整个块，而中间的几次是整个块读取（最后的一次可能也不是一整个块）
        }
        read_size
    }
    /// Write data into current disk inode
    /// size must be adjusted properly beforehand
    ///
    /// 只要 Inode 管理的数据块的大小足够，传入的整个缓冲区的数据都必定会被写入到文件中。
    /// 当从 offset 开始的区间超出了文件范围的时候，就需要调用者在调用 write_at 之前提前调用 increase_size ，将文件大小扩充到区间的右端，保证写入的完整性
    pub fn write_at(
        &mut self,
        offset: usize,
        buf: &[u8],
        block_device: &Arc<dyn BlockDevice>,
    ) -> usize {
        let mut start = offset;
        let end = (offset + buf.len()).min(self.size as usize); // 也是至多写到当前文件的结尾，因此如果超过了文件的现有容量，需要调用者提前increase_size
        assert!(start <= end);
        let mut start_block = start / BLOCK_SZ;
        let mut write_size = 0usize;
        loop {
            // calculate end of current block
            let mut end_current_block = (start / BLOCK_SZ + 1) * BLOCK_SZ;
            end_current_block = end_current_block.min(end);
            // write and update write size
            let block_write_size = end_current_block - start;
            get_block_cache(
                self.get_block_id(start_block as u32, block_device) as usize,
                Arc::clone(block_device),
            )
            .lock()
            .modify(0, |data_block: &mut DataBlock| {
                let src = &buf[write_size..write_size + block_write_size];
                let dst = &mut data_block[start % BLOCK_SZ..start % BLOCK_SZ + block_write_size];
                dst.copy_from_slice(src);
            });
            write_size += block_write_size;
            // move to next block
            if end_current_block == end {
                break;
            }
            start_block += 1;
            start = end_current_block;
        }
        write_size
    }

    /// 将大小减少到new_size
    pub fn decrease_size(&mut self, new_size: u32) {
        if new_size >= self.size {
            return;
        }
        self.size = new_size;
        // todo 考虑回收数据块信息？
    }

    /// 删除指定偏移的内容
    ///
    /// 这里实现为一个线性表（将后续元素覆盖到前面）
    pub fn remove_at(&mut self, offset: usize, buf: &mut [u8], block_device: &Arc<dyn BlockDevice>) {
        // 1. 先读取需要删除的元素，存入buf中返回
        let len = self.read_at(offset, buf, block_device);
        // 2. 再将后续的内容移动，需要申请足够长的缓冲区（这里只需要超过剩余元素的个数就行了，注意buf的元素不需要处理）
        let mut v: Vec<u8> = vec![0; self.size as usize - offset - len];
        let tmp_buf = v.as_mut_slice();
        // 3. 读取后续元素
        self.read_at(offset + len, tmp_buf, block_device);
        // 4. 写入offset位置
        self.write_at(offset, tmp_buf, block_device);
        // 5. 修改size
        let new_size = self.size - len as u32;
        self.decrease_size(new_size);
    }

}
/// A directory entry: 位于easy-fs五层架构中的第3层，32字节
///
/// 目录的内容必须满足特殊的格式，在本次实现中看成是一个“目录项”的序列，每个目录项是一个二元组：(name, inode_id)
#[repr(C)]
pub struct DirEntry {
    /// 这里简单实现为：name的最大长度为27（末尾一个字节留给\0）
    name: [u8; NAME_LENGTH_LIMIT + 1],
    /// 4个字节的inode id
    inode_id: u32,
}
/// Size of a directory entry
pub const DIRENT_SZ: usize = 32;

impl DirEntry {
    /// Create an empty directory entry
    pub fn empty() -> Self {
        Self {
            name: [0u8; NAME_LENGTH_LIMIT + 1],
            inode_id: 0,
        }
    }
    /// Crate a directory entry from name and inode number
    pub fn new(name: &str, inode_id: u32) -> Self {
        let mut bytes = [0u8; NAME_LENGTH_LIMIT + 1];
        bytes[..name.len()].copy_from_slice(name.as_bytes());
        Self {
            name: bytes,
            inode_id,
        }
    }
    /// Serialize into bytes: 以符合write_at方法接口的要求
    pub fn as_bytes(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self as *const _ as usize as *const u8, DIRENT_SZ) }
    }
    /// Serialize into mutable bytes: 以符合read_at方法接口的要求
    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self as *mut _ as usize as *mut u8, DIRENT_SZ) }
    }
    /// Get name of the entry
    pub fn name(&self) -> &str {
        let len = (0usize..).find(|i| self.name[*i] == 0).unwrap(); // 按照C语言字符串的方式读取
        core::str::from_utf8(&self.name[..len]).unwrap()
    }
    /// Get inode number of the entry
    pub fn inode_id(&self) -> u32 {
        self.inode_id
    }

    /// 删除当前目录项
    ///
    /// 这里简单实现为：赋予一个特定的inode_id
    pub fn mark_as_deleted(&mut self) {
        self.inode_id = FILE_NOT_EXIST;
    }
}
