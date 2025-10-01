//!An easy file system isolated from the kernel
#![no_std]
#![deny(missing_docs)]
extern crate alloc;
extern crate log;
mod bitmap;
mod block_cache;
mod block_dev;
mod efs;
mod layout;
mod vfs;
/// Use a block size of 512 bytes
pub const BLOCK_SZ: usize = 512;
use bitmap::Bitmap;
use block_cache::{block_cache_sync_all, get_block_cache};
pub use block_dev::BlockDevice;
pub use efs::EasyFileSystem;
use layout::*;
pub use vfs::Inode;
/// inode_id的最大允许值，代表了文件（夹）的最大数量。预留128个位置
pub const MAX_INODE_ID: u32 = u32::MAX - 128;
/// 不存在的INODE_ID
pub const FILE_NOT_EXIST: u32 = u32::MAX - 1;
