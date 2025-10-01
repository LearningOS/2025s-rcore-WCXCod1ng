use super::{get_block_cache, BlockDevice, BLOCK_SZ, MAX_INODE_ID};
use alloc::sync::Arc;
/// A bitmap block
/// BitmapBlock 是一个磁盘数据结构，它将位图区域中的一个磁盘块解释为长度为 64 的一个 u64 数组，每个u64打包了一组64bits
/// 于是整个数组包含 64 * 64 bits = 4096 bits = 512 bytes，且可以以组为单位进行操作
type BitmapBlock = [u64; 64];
/// Number of bits in a block
const BLOCK_BITS: usize = BLOCK_SZ * 8;
/// A bitmap: 位于easy-fs五层架构中的第3层
pub struct Bitmap {
    /// 起始块id
    start_block_id: usize,
    /// 该bitmap所占据的块数
    blocks: usize,
}

/// Decompose bits (inode_pos) into (block_pos, bits64_pos, inner_pos)
fn decomposition(mut bit: usize) -> (usize, usize, usize) {
    let block_pos = bit / BLOCK_BITS;
    bit %= BLOCK_BITS;
    (block_pos, bit / 64, bit % 64)
}

impl Bitmap {
    /// A new bitmap from start block id and number of blocks
    pub fn new(start_block_id: usize, blocks: usize) -> Self {
        Self {
            start_block_id,
            blocks,
        }
    }
    /// Allocate a new block from a block device
    /// 主要思路是遍历区域中的每个块，再在每个块中以bit组（每组 64 bits）为单位进行遍历，找到一个尚未被全部分配出去的组，
    /// 最后在里面分配一个bit。它将会返回分配的bit所在的位置，等同于索引节点/数据块的编号。如果所有bit均已经被分配出去了，则返回 None
    pub fn alloc(&self, block_device: &Arc<dyn BlockDevice>) -> Option<usize> {
        for block_id in 0..self.blocks {
            // 调用 get_block_cache 获取块缓存，注意我们传入的块编号是区域起始块编号 start_block_id 加上区域内的块编号 block_id 得到的块设备上的块编号
            let pos = get_block_cache(
                block_id + self.start_block_id as usize,
                Arc::clone(block_device),
            )
            // 通过 .lock() 获取块缓存的互斥锁从而可以对块缓存进行访问
            .lock()
            // 使用到了 BlockCache::modify 接口。它传入的偏移量 offset 为 0，这是因为整个块上只有一个 BitmapBlock ，它的大小恰好为 512 字节。因此我们需要从块的开头开始才能访问到完整的 BitmapBlock
            // 传给它的闭包需要显式声明参数类型为 &mut BitmapBlock ，不然的话， BlockCache 的泛型方法 modify/get_mut 无法得知应该用哪个类型来解析块上的数据
            .modify(0, |bitmap_block: &mut BitmapBlock| {
                // 在闭包内部，我们可以使用这个 BitmapBlock 的可变引用 bitmap_block 对它进行访问
                // 尝试在 bitmap_block 中找到一个空闲的bit并返回其位置，如果不存在的话则返回 None
                // // 如果能够找到的话，bit组的编号将保存在变量 bits64_pos中（也就是下标：第几个64 bits组），而分配的bit在组内的位置将保存在变量 inner_pos 中（由于是按序分配，所以找到的那个就是最终的位置）
                if let Some((bits64_pos, inner_pos)) = bitmap_block
                    .iter()
                    .enumerate() // 遍历每 64 bits构成的组（一个 u64 ）
                    .find(|(_, bits64)| **bits64 != u64::MAX) // 如果它并没有达到 u64::MAX （即 2^64-1），那么说明这个组还有空闲位
                    .map(|(bits64_pos, bits64)| (bits64_pos, bits64.trailing_ones() as usize)) // 通过 u64::trailing_ones 找到最低的一个 0 并置为 1
                {
                    // modify cache
                    bitmap_block[bits64_pos] |= 1u64 << inner_pos;
                    // 在返回分配的bit编号的时候，它的计算方式是 block_id*BLOCK_BITS+bits64_pos*64+inner_pos，也就代表了第几个块
                    Some(block_id * BLOCK_BITS + bits64_pos * 64 + inner_pos as usize)
                } else {
                    None
                }
            });
            if pos.is_some() { // 一旦在某个块中找到一个空闲的bit并成功分配，就不再考虑后续的块。注意要考虑INODE_ID的范围
                return pos;
            }
        }
        None
    }
    /// Deallocate a block
    pub fn dealloc(&self, block_device: &Arc<dyn BlockDevice>, bit: usize) {
        let (block_pos, bits64_pos, inner_pos) = decomposition(bit);
        get_block_cache(block_pos + self.start_block_id, Arc::clone(block_device))
            .lock()
            .modify(0, |bitmap_block: &mut BitmapBlock| {
                assert!(bitmap_block[bits64_pos] & (1u64 << inner_pos) > 0);
                bitmap_block[bits64_pos] -= 1u64 << inner_pos; // 直接减去这一位
            });
    }
    /// Get the max number of allocatable blocks
    pub fn maximum(&self) -> usize {
        self.blocks * BLOCK_BITS
    }
}
