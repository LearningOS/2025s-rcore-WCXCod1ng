use core::any::Any;
/// Trait for block devices
/// which reads and writes data in the unit of blocks
///
/// 位于easy-fs五层架构中的第1层，定义了以磁盘块大小为单位进行读写的trait
/// 在 easy-fs 中并没有一个实现了 BlockDevice Trait 的具体类型。因为块设备仅支持以块为单位进行随机读写，
/// 所以需要由具体的块设备驱动来实现这两个方法，实际上这是需要由文件系统的使用者（比如操作系统内核 或
/// 直接测试 easy-fs 文件系统的 easy-fs-fuse 应用程序）提供并接入到 easy-fs 库的
///
/// easy-fs 库的块缓存层会调用这两个方法，进行块缓存的管理。这也体现了 easy-fs 的泛用性：它可以访问实现了 BlockDevice Trait 的块设备驱动程序
///
/// 块和扇区是两个不同的概念。 扇区 (Sector) 是块设备随机读写的数据单位，通常每个扇区为 512 字节。
/// 而块是文件系统存储文件时的数据单位，每个块的大小等同于一个或多个扇区。之前提到过 Linux 的Ext4文件系统的单个块大小默认为 4096 字节。
/// 在我们的 easy-fs 实现中一个块和一个扇区同为 512 字节，因此在后面的讲解中我们不再区分扇区和块的概念
pub trait BlockDevice: Send + Sync + Any {
    ///Read data form block to buffer
    fn read_block(&self, block_id: usize, buf: &mut [u8]);
    ///Write data from buffer to block
    fn write_block(&self, block_id: usize, buf: &[u8]);
}
