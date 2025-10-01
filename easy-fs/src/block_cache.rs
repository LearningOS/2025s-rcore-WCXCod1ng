use super::{BlockDevice, BLOCK_SZ};
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use lazy_static::*;
use spin::Mutex;
/// Cached block inside memory
/// 位于easy-fs五层架构中的第2层，缓存管理的关键就在于对块读写操作进行 合并 。例如，如果一个块已经被读到缓冲区中了，那么我们就没有必要再读一遍，
/// 直接用已有的缓冲区就行了；同时，对于缓冲区中的同一个块的多次修改没有必要每次都写回磁盘，只需等所有的修改都结束之后统一写回磁盘即可
/// 当我们要读写一个块的时候，首先就是去全局管理器中查看这个块是否已被缓存到内存缓冲区中。
/// 如果是这样，则在一段连续时间内对于一个块进行的所有操作均是在同一个固定的缓冲区中进行的，这解决了同步性问题。
/// 此外，通过 read/write_block 进行块实际读写的时机完全交给块缓存层的全局管理器处理，上层子系统无需操心。
/// 全局管理器会尽可能将更多的块操作合并起来，并在必要的时机发起真正的块实际读写
pub struct BlockCache {
    /// cached block data
    /// cache 是一个512字节（与块大小一致）的数组，表示位于内存中的缓冲区
    cache: [u8; BLOCK_SZ],
    /// underlying block id
    block_id: usize,
    /// underlying block device
    block_device: Arc<dyn BlockDevice>,
    /// whether the block is dirty
    modified: bool,
}

impl BlockCache {
    /// Load a new BlockCache from disk.
    /// 当我们创建一个 BlockCache 的时候，这将触发一次 read_block 将一个块上的数据从磁盘读到缓冲区 cache
    pub fn new(block_id: usize, block_device: Arc<dyn BlockDevice>) -> Self {
        let mut cache = [0u8; BLOCK_SZ];
        block_device.read_block(block_id, &mut cache);
        Self {
            cache,
            block_id,
            block_device,
            modified: false,
        }
    }
    /// Get the address of an offset inside the cached block data
    fn addr_of_offset(&self, offset: usize) -> usize {
        &self.cache[offset] as *const _ as usize
    }

    /// 获取缓冲区中的位于偏移量 offset 的一个类型为 T 的磁盘上数据结构的不可变引用
    /// 该泛型方法的 Trait Bound 限制类型 T 必须是一个编译时已知大小的类型，
    /// 我们通过 core::mem::size_of::<T>() 在编译时获取类型T的大小，并确认该数据结构被整个包含在磁盘块及其缓冲区之内
    /// 这里编译器会自动进行生命周期标注，约束返回的引用的生命周期不超过 BlockCache 自身，在使用的时候我们会保证这一点
    pub fn get_ref<T>(&self, offset: usize) -> &T
    where
        T: Sized,
    {
        let type_size = core::mem::size_of::<T>();
        assert!(offset + type_size <= BLOCK_SZ);
        let addr = self.addr_of_offset(offset);
        unsafe { &*(addr as *const T) }
    }

    /// get_mut 会获取磁盘上数据结构的可变引用
    /// 需要将 BlockCache 的 modified 标记为 true 表示该缓冲区已经被修改，之后需要将数据写回磁盘块才能真正将修改同步到磁盘
    pub fn get_mut<T>(&mut self, offset: usize) -> &mut T
    where
        T: Sized,
    {
        let type_size = core::mem::size_of::<T>();
        assert!(offset + type_size <= BLOCK_SZ);
        self.modified = true; // 表示该缓冲区已经被修改（dirty），需要在将来被同步到磁盘上
        let addr = self.addr_of_offset(offset);
        unsafe { &mut *(addr as *mut T) }
    }

    /// 对get_ref的封装，用于对指定偏移的T类型的数据结构进行“read”操作（使用闭包read）
    pub fn read<T, V>(&self, offset: usize, f: impl FnOnce(&T) -> V) -> V {
        f(self.get_ref(offset))
    }

    /// 对get_mut的封装，用于对指定偏移的T类型的数据结构进行“write”操作（使用闭包write）
    /// 这里选择impl FnOnce是为了能够兼顾Fn和FnMut，这里&mut T表明即使推断为FnOnce，原始数据所有权也不会被闭包consume，
    /// 被consume的是这个引用，原始数据的所有权一直都在BlockCache中。要想真正接管，需要将f设计为：
    /// ````rust
    /// pub fn modify<T, V>(&mut self, offset: usize, f: impl FnOnce(T) -> V) -> V {}
    /// ````
    pub fn modify<T, V>(&mut self, offset: usize, f: impl FnOnce(&mut T) -> V) -> V {
        f(self.get_mut(offset))
    }

    pub fn sync(&mut self) {
        if self.modified {
            self.modified = false;
            self.block_device.write_block(self.block_id, &self.cache);
        }
    }
}

impl Drop for BlockCache {
    /// BlockCache 的设计也体现了 RAII 思想， 它管理着一个缓冲区的生命周期。
    /// 当 BlockCache 的生命周期结束之后缓冲区也会被从内存中回收，这个时候 modified 标记将会决定数据是否需要写回磁盘
    fn drop(&mut self) {
        self.sync()
    }
}
/// Use a block cache of 16 blocks
const BLOCK_CACHE_SIZE: usize = 16;

pub struct BlockCacheManager {
    /// 当我们要对一个磁盘块进行读写时，首先看它是否已经被载入到内存缓存中了，如果已经被载入的话则直接返回，
    /// 否则需要先读取磁盘块的数据到内存缓存中。此时，如果内存中驻留的磁盘块缓冲区的数量已满，
    /// 则需要遵循某种缓存替换算法将某个块的缓存从内存中移除，再将刚刚读到的块数据加入到内存缓存中。
    /// 我们这里使用一种类 FIFO 的简单缓存替换算法，因此在管理器中只需维护一个队列
    ///
    /// 队列中的每个元素是一个(块编号, 块缓存引用)的二元组，块缓存使用了Arc<Mutex<BlockCache>>的形式
    /// 这里的Arch意义在于块缓存既需要在管理器 BlockCacheManager 保留一个引用，还需要以引用的形式返回给块缓存的请求者让它可以对块缓存进行访问。
    /// 而Mutex在单核上的意义在于提供内部可变性通过编译，在多核环境下则可以帮助我们避免可能的并发冲突
    /// > 一般情况下我们需要在更上层提供保护措施避免两个线程同时对一个块缓存进行读写，因此这里只是比较谨慎的留下一层保险
    queue: VecDeque<(usize, Arc<Mutex<BlockCache>>)>,
}

impl BlockCacheManager {
    pub fn new() -> Self {
        Self {
            queue: VecDeque::new(),
        }
    }

    /// 尝试从块缓存管理器中获取一个编号为 block_id 的块的块缓存，如果找不到，会从磁盘读取到内存中，还有可能会发生缓存替换
    pub fn get_block_cache(
        &mut self,
        block_id: usize,
        block_device: Arc<dyn BlockDevice>,
    ) -> Arc<Mutex<BlockCache>> {
        if let Some(pair) = self.queue.iter().find(|pair| pair.0 == block_id) {
            Arc::clone(&pair.1)
        } else {
            // substitute
            if self.queue.len() == BLOCK_CACHE_SIZE {
                // from front to tail
                // 要替换时则从队头弹出。但此时队头对应的块缓存可能仍在使用：判断的标志是其强引用计数`>=2`
                // 即除了块缓存管理器保留的一份副本之外，在外面还有若干份副本正在使用。
                // 因此，我们的做法是从队头遍历到队尾找到第一个强引用计数恰好为 1 的块缓存并将其替换出去
                if let Some((idx, _)) = self
                    .queue
                    .iter()
                    .enumerate()
                    .find(|(_, pair)| Arc::strong_count(&pair.1) == 1)
                {
                    self.queue.drain(idx..=idx);
                } else {
                    // 是否有可能出现队列已满且其中所有的块缓存都正在使用的情形呢？
                    // 事实上，只要我们的上限 BLOCK_CACHE_SIZE 设置的足够大，超过所有应用同时访问的块总数上限，那么这种情况永远不会发生。
                    // 但是，如果我们的上限设置不足，内核将 panic （基于简单内核设计的思路）
                    panic!("Run out of BlockCache!");
                }
            }
            // load block into mem and push back
            let block_cache = Arc::new(Mutex::new(BlockCache::new(
                block_id,
                Arc::clone(&block_device),
            )));
            // 每加入一个块缓存时要从队尾加入
            self.queue.push_back((block_id, Arc::clone(&block_cache)));
            block_cache
        }
    }
}

lazy_static! {
    /// The global block cache manager
    /// 这里本质上也是一个共享+内部可变性的案例，因为队列的元素可以发生变化，这里需要同一时刻只能有一个地方在访问
    pub static ref BLOCK_CACHE_MANAGER: Mutex<BlockCacheManager> =
        Mutex::new(BlockCacheManager::new());
}
/// Get the block cache corresponding to the given block id and block device
pub fn get_block_cache(
    block_id: usize,
    block_device: Arc<dyn BlockDevice>,
) -> Arc<Mutex<BlockCache>> {
    BLOCK_CACHE_MANAGER
        .lock()
        .get_block_cache(block_id, block_device) // 返回的是一个 Arc<Mutex<BlockCache>> ，调用者需要通过 .lock() 获取里层互斥锁 Mutex 才能对最里面的 BlockCache 进行操作
}
/// Sync all block cache to block device
pub fn block_cache_sync_all() {
    let manager = BLOCK_CACHE_MANAGER.lock();
    for (_, cache) in manager.queue.iter() {
        cache.lock().sync();
    }
}
