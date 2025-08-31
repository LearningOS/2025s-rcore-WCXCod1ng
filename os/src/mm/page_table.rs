//! Implementation of [`PageTableEntry`] and [`PageTable`].

use super::{frame_alloc, FrameTracker, PhysPageNum, StepByOne, VirtAddr, VirtPageNum};
use alloc::vec;
use alloc::vec::Vec;
use bitflags::*;

bitflags! {
    /// page table entry flags
    pub struct PTEFlags: u8 {
        /// Valid
        const V = 1 << 0;
        /// Readable
        const R = 1 << 1;
        /// Writable
        const W = 1 << 2;
        /// eXecutable
        const X = 1 << 3;
        /// User
        const U = 1 << 4;
        /// Global
        const G = 1 << 5;
        /// Accessed
        const A = 1 << 6;
        /// Dirty
        const D = 1 << 7;
    }
}

#[derive(Copy, Clone)]
#[repr(C)]
/// page table entry structure
pub struct PageTableEntry { // 一个页表项，实际上只会存储物理页号（因为虚拟页号以偏移的形式隐式表示）
    /// bits of page table entry
    pub bits: usize,
}

impl PageTableEntry {
    /// Create a new page table entry
    pub fn new(ppn: PhysPageNum, flags: PTEFlags) -> Self {
        PageTableEntry {
            bits: ppn.0 << 10 | flags.bits as usize,
        }
    }
    /// Create an empty page table entry
    pub fn empty() -> Self {
        PageTableEntry { bits: 0 }
    }
    /// Get the physical page number from the page table entry
    pub fn ppn(&self) -> PhysPageNum {
        (self.bits >> 10 & ((1usize << 44) - 1)).into()
    }
    /// Get the flags from the page table entry
    pub fn flags(&self) -> PTEFlags {
        PTEFlags::from_bits(self.bits as u8).unwrap()
    }
    /// The page pointered by page table entry is valid?
    pub fn is_valid(&self) -> bool {
        (self.flags() & PTEFlags::V) != PTEFlags::empty()
    }
    /// The page pointered by page table entry is readable?
    pub fn readable(&self) -> bool {
        (self.flags() & PTEFlags::R) != PTEFlags::empty()
    }
    /// The page pointered by page table entry is writable?
    pub fn writable(&self) -> bool {
        (self.flags() & PTEFlags::W) != PTEFlags::empty()
    }
    /// The page pointered by page table entry is executable?
    pub fn executable(&self) -> bool {
        (self.flags() & PTEFlags::X) != PTEFlags::empty()
    }
    /// 增加功能：是否用户可访问
    pub fn user_accessible(&self) -> bool {
        (self.flags() & PTEFlags::U) != PTEFlags::empty()
    }
}

/// page table structure
pub struct PageTable { // 代表一个页表，使用一棵（字典）树表示，可以只利用一个根节点物理页号，加上该页表所管理的物理页（即frame）集合表示
    root_ppn: PhysPageNum,
    frames: Vec<FrameTracker>, // 这种方式可以将一个页表下的所有frame的声明周期都绑定到该页表上
}

/// Assume that it won't oom when creating/mapping.
impl PageTable {
    /// Create a new page table
    pub fn new() -> Self {
        let frame = frame_alloc().unwrap();
        PageTable {
            root_ppn: frame.ppn,
            frames: vec![frame], // 新建时，一定会分配一个frame，存储根节点
        }
    }
    /// Temporarily used to get arguments from user space.
    pub fn from_token(satp: usize) -> Self { // from_token 可以临时创建一个专用来手动查页表的 PageTable ，它仅有一个从传入的 satp token 中得到的多级页表根节点的物理页号，它的 frames 字段为空，也即不实际控制任何资源
        Self {
            root_ppn: PhysPageNum::from(satp & ((1usize << 44) - 1)), // 只提取它的ppn部分
            frames: Vec::new(),
        }
    }
    /// Find PageTableEntry by VirtPageNum, create a frame for a 4KB page table if not exist
    fn find_pte_create(&mut self, vpn: VirtPageNum) -> Option<&mut PageTableEntry> {
        let idxs = vpn.indexes();
        let mut ppn = self.root_ppn;
        let mut result: Option<&mut PageTableEntry> = None;
        for (i, idx) in idxs.iter().enumerate() {
            let pte = &mut ppn.get_pte_array()[*idx]; // 根据虚拟地址的第i级的值确定页表项
            if i == 2 {
                result = Some(pte); // 三级页表结束后一定能够找到一个页，返回页表项即可（它就可以唯一确定目标frame，因为存储了物理页号）
                break;
            }
            if !pte.is_valid() { // 说明其实这一个表项不存在，需要重新分配一个frame，并将虚拟地址和frame的ppn填入这里
                let frame = frame_alloc().unwrap();
                *pte = PageTableEntry::new(frame.ppn, PTEFlags::V); // 写入，注意要将Valid置为1，表示这一个页表项还存在
                self.frames.push(frame); // 新加入的frame存入vec中
            }
            ppn = pte.ppn(); // 来到下一级页表
        }
        result
    }
    /// Find PageTableEntry by VirtPageNum
    fn find_pte(&self, vpn: VirtPageNum) -> Option<&mut PageTableEntry> {
        let idxs = vpn.indexes();
        let mut ppn = self.root_ppn;
        let mut result: Option<&mut PageTableEntry> = None;
        for (i, idx) in idxs.iter().enumerate() {
            let pte = &mut ppn.get_pte_array()[*idx];
            if i == 2 {
                result = Some(pte);
                break;
            }
            if !pte.is_valid() { // 区别仅在于遇到无效的entry就返回None
                return None;
            }
            ppn = pte.ppn();
        }
        result
    }
    // 多级页表并不是被创建出来之后就不再变化的，为了 MMU 能够通过地址转换正确找到应用地址空间中的数据实际被内核放在内存中 位置，操作系统需要动态维护一个虚拟页号到页表项的映射，支持插入/删除键值对，也即下面的map和unmap
    /// set the map between virtual page number and physical page number
    #[allow(unused)]
    pub fn map(&mut self, vpn: VirtPageNum, ppn: PhysPageNum, flags: PTEFlags) {
        let pte = self.find_pte_create(vpn).unwrap();
        assert!(!pte.is_valid(), "vpn {:?} is mapped before mapping", vpn);
        *pte = PageTableEntry::new(ppn, flags | PTEFlags::V);
    }
    /// remove the map between virtual page number and physical page number
    #[allow(unused)]
    pub fn unmap(&mut self, vpn: VirtPageNum) {
        let pte = self.find_pte(vpn).unwrap();
        assert!(pte.is_valid(), "vpn {:?} is invalid before unmapping", vpn);
        *pte = PageTableEntry::empty();
    }
    /// get the page table entry from the virtual page number
    pub fn translate(&self, vpn: VirtPageNum) -> Option<PageTableEntry> {
        self.find_pte(vpn).map(|pte| *pte)
    }
    /// get the token from the page table
    /// PageTable::token 会按照 satp CSR 格式要求 构造一个无符号 64 位无符号整数，使得其 分页模式为 SV39 ，且将当前多级页表的根节点所在的物理页号填充进去
    pub fn token(&self) -> usize { // 用于标明是否开启分页
        8usize << 60 | self.root_ppn.0
    }
}

/// Translate&Copy a ptr[u8] array with LENGTH len to a mutable u8 Vec through page table
pub fn translated_byte_buffer(token: usize, ptr: *const u8, len: usize) -> Vec<&'static mut [u8]> {
    // 它以一个代表用户地址空间的 token 和一个用户虚拟地址 ptr 为输入，通过软件模拟硬件的页表查询过程，找到数据对应的真实物理内存位置，然后安全地从这些物理内存中提取出数据切片（切len个字节）
    let page_table = PageTable::from_token(token);
    let mut start = ptr as usize;
    // 内核不能直接解引用 ptr！因为 ptr 是一个用户态的虚拟地址。内核运行时，CPU 的 satp 寄存器指向的是内核的页表。如果直接访问 ptr，要么会访问到内核地址空间中一个完全无关的位置，要么（更可能地）会因为该地址在内核页表中无效而触发缺页异常 (Page Fault)。
    // 因此，内核必须使用用户进程的页表（由 token，也就是 satp 的值代表）来手动翻译这个地址
    let end = start + len;
    let mut v = Vec::new();
    while start < end {
        let start_va = VirtAddr::from(start);
        let mut vpn = start_va.floor();
        let ppn = page_table.translate(vpn).unwrap().ppn();
        vpn.step();
        let mut end_va: VirtAddr = vpn.into();
        end_va = end_va.min(VirtAddr::from(end));
        if end_va.page_offset() == 0 { // 这个条件判断数据块是否跨越了页边界
            v.push(&mut ppn.get_bytes_array()[start_va.page_offset()..]);
        } else {
            v.push(&mut ppn.get_bytes_array()[start_va.page_offset()..end_va.page_offset()]); // 将指定区域切给v
        }
        start = end_va.into();
    }
    v
}
