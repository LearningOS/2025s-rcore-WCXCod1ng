//! Implementation of [`MapArea`] and [`MemorySet`].

use super::{frame_alloc, FrameTracker};
use super::{PTEFlags, PageTable, PageTableEntry};
use super::{PhysAddr, PhysPageNum, VirtAddr, VirtPageNum};
use super::{StepByOne, VPNRange};
use crate::config::{
    KERNEL_STACK_SIZE, MEMORY_END, PAGE_SIZE, TRAMPOLINE, TRAP_CONTEXT_BASE, USER_STACK_SIZE,
};
use crate::sync::UPSafeCell;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::arch::asm;
use lazy_static::*;
use riscv::register::satp;

extern "C" {
    fn stext();
    fn etext();
    fn srodata();
    fn erodata();
    fn sdata();
    fn edata();
    fn sbss_with_stack();
    fn ebss();
    fn ekernel();
    fn strampoline();
}

lazy_static! {
    /// The kernel's initial memory mapping(kernel address space)
    pub static ref KERNEL_SPACE: Arc<UPSafeCell<MemorySet>> =
        Arc::new(unsafe { UPSafeCell::new(MemorySet::new_kernel()) });
}
/// address space
/// 地址空间是一系列有关联的逻辑段，这种关联一般是指这些逻辑段属于一个运行的程序（目前把一个运行的程序称为任务，后续会称为进程）。 用来表明正在运行的应用所在执行环境中的可访问内存空间，在这个内存空间中，包含了一系列的不一定连续的逻辑段。 这样我们就有任务的地址空间、内核的地址空间等说法了
pub struct MemorySet {
    // 多级页表
    page_table: PageTable,
    // 存储属于该地址空间的所有逻辑段
    areas: Vec<MapArea>,
}

impl MemorySet {
    /// Create a new empty `MemorySet`.
    pub fn new_bare() -> Self {
        Self {
            page_table: PageTable::new(),
            areas: Vec::new(),
        }
    }
    /// Get the page table token
    pub fn token(&self) -> usize {
        self.page_table.token()
    }
    /// Assume that no conflicts.
    pub fn insert_framed_area(
        &mut self,
        start_va: VirtAddr,
        end_va: VirtAddr,
        permission: MapPermission,
    ) {
        // 在当前地址空间插入一个 Framed 方式映射到 物理内存的逻辑段。注意该方法的调用者要保证同一地址空间内的任意两个逻辑段不能存在交集
        self.push(
            MapArea::new(start_va, end_va, MapType::Framed, permission),
            None,
        );
    }
    fn push(&mut self, mut map_area: MapArea, data: Option<&[u8]>) { // 在当前地址空间中插入一个新的逻辑段map_area
        map_area.map(&mut self.page_table); // 建立映射关系
        if let Some(data) = data {
            map_area.copy_data(&mut self.page_table, data); // 可选地在那些被映射到的物理页帧上写入一些初始化数据 data
        }
        self.areas.push(map_area);
    }

    /// Mention that trampoline is not collected by areas.
    fn map_trampoline(&mut self) {
        // 无论是内核还是应用的地址空间，跳板的虚拟页均位于同样位置，且它们也将会映射到同一个实际存放这段 汇编代码的物理页帧
        self.page_table.map(
            VirtAddr::from(TRAMPOLINE).into(), // 跳板函数在整个地址空间的最高一页（4K）
            PhysAddr::from(strampoline as usize).into(),
            PTEFlags::R | PTEFlags::X,
        );
    }
    /// Without kernel stacks.
    pub fn new_kernel() -> Self { // SV39规范中，最高的256GB用于内核空间（最低的256GB用于用户空间）
        let mut memory_set = Self::new_bare();
        // map trampoline
        memory_set.map_trampoline();
        // map kernel sections
        info!(".text [{:#x}, {:#x})", stext as usize, etext as usize);
        info!(".rodata [{:#x}, {:#x})", srodata as usize, erodata as usize);
        info!(".data [{:#x}, {:#x})", sdata as usize, edata as usize);
        info!(
            ".bss [{:#x}, {:#x})",
            sbss_with_stack as usize, ebss as usize
        );
        info!("mapping .text section");
        memory_set.push(
            MapArea::new(
                (stext as usize).into(),
                (etext as usize).into(),
                MapType::Identical,
                MapPermission::R | MapPermission::X,
            ),
            None,
        );
        info!("mapping .rodata section");
        memory_set.push(
            MapArea::new(
                (srodata as usize).into(),
                (erodata as usize).into(),
                MapType::Identical,
                MapPermission::R,
            ),
            None,
        );
        info!("mapping .data section");
        memory_set.push(
            MapArea::new(
                (sdata as usize).into(),
                (edata as usize).into(),
                MapType::Identical,
                MapPermission::R | MapPermission::W,
            ),
            None,
        );
        info!("mapping .bss section");
        memory_set.push(
            MapArea::new(
                (sbss_with_stack as usize).into(),
                (ebss as usize).into(),
                MapType::Identical,
                MapPermission::R | MapPermission::W,
            ),
            None,
        );
        info!("mapping physical memory");
        memory_set.push(
            MapArea::new(
                (ekernel as usize).into(),
                MEMORY_END.into(),
                MapType::Identical,
                MapPermission::R | MapPermission::W,
            ),
            None,
        );
        memory_set
    }
    /// Include sections in elf and trampoline and TrapContext and user stack,
    /// also returns user_sp_base and entry point.
    pub fn from_elf(elf_data: &[u8]) -> (Self, usize, usize) {
        let mut memory_set = Self::new_bare();
        // map trampoline
        memory_set.map_trampoline();
        // 使用xmas_elf库解析ELF格式的数据
        // map program headers of elf, with U flag
        let elf = xmas_elf::ElfFile::new(elf_data).unwrap();
        // get elf header
        let elf_header = elf.header;
        let magic = elf_header.pt1.magic;
        assert_eq!(magic, [0x7f, 0x45, 0x4c, 0x46], "invalid elf!");
        // get program header count
        let ph_count = elf_header.pt2.ph_count();
        let mut max_end_vpn = VirtPageNum(0);
        for i in 0..ph_count { // process every program header，程序头描述的就是逻辑段的信息，下面的逻辑也就是根据ph创建逻辑段
            let ph = elf.program_header(i).unwrap();
            if ph.get_type().unwrap() == xmas_elf::program::Type::Load { // 确认 program header 的类型是 LOAD ， 这表明它有被内核加载的必要，此时不必理会其他类型的 program header
                // 计算该program header 所占的地址区域
                let start_va: VirtAddr = (ph.virtual_addr() as usize).into();
                let end_va: VirtAddr = ((ph.virtual_addr() + ph.mem_size()) as usize).into();
                let mut map_perm = MapPermission::U;
                let ph_flags = ph.flags();
                if ph_flags.is_read() {
                    map_perm |= MapPermission::R;
                }
                if ph_flags.is_write() {
                    map_perm |= MapPermission::W;
                }
                if ph_flags.is_execute() {
                    map_perm |= MapPermission::X;
                }
                // 创建对应的逻辑段
                let map_area = MapArea::new(start_va, end_va, MapType::Framed, map_perm);
                max_end_vpn = map_area.vpn_range.get_end();
                memory_set.push(
                    map_area,
                    // 当前 program header 数据被存放的位置可以通过 ph.offset() 和 ph.file_size() 来找到。 注意当 存在一部分零初始化的时候， ph.file_size() 将会小于 ph.mem_size() ，因为这些零出于缩减可执行 文件大小的原因不应该实际出现在 ELF 数据中
                    Some(&elf.input[ph.offset() as usize..(ph.offset() + ph.file_size()) as usize]),
                );
            }
        }
        // map user stack with U flags
        // max_end_vpn 记录目前涉及到的最大的虚拟页号，只需紧接着在它上面再放置一个保护页面和用户栈即可
        let max_end_va: VirtAddr = max_end_vpn.into();
        let mut user_stack_bottom: usize = max_end_va.into();
        // guard page
        user_stack_bottom += PAGE_SIZE;
        let user_stack_top = user_stack_bottom + USER_STACK_SIZE;
        memory_set.push(
            MapArea::new(
                user_stack_bottom.into(),
                user_stack_top.into(),
                MapType::Framed,
                MapPermission::R | MapPermission::W | MapPermission::U,
            ),
            None,
        );
        // used in sbrk
        memory_set.push(
            MapArea::new(
                user_stack_top.into(),
                user_stack_top.into(),
                MapType::Framed,
                MapPermission::R | MapPermission::W | MapPermission::U,
            ),
            None,
        );
        // map TrapContext
        memory_set.push(
            MapArea::new( // TrapContext和trampoline紧挨着放置
                TRAP_CONTEXT_BASE.into(),
                TRAMPOLINE.into(),
                MapType::Framed,
                MapPermission::R | MapPermission::W,
            ),
            None,
        );
        (
            memory_set,
            user_stack_top,
            elf.header.pt2.entry_point() as usize, // 程序入口
        )
    }
    /// Change page table by writing satp CSR Register.
    pub fn activate(&self) {
        // 我们将这个值写入当前 CPU 的 satp CSR ，从这一刻开始 SV39 分页模式就被启用了，而且 MMU 会使用内核地址空间的多级页表进行地址转换
        let satp = self.page_table.token();
        //我们必须注意切换 satp CSR 是否是一个 平滑 的过渡：其含义是指，切换 satp 的指令及其下一条指令这两条相邻的指令的
        // 虚拟地址是相邻的（由于切换 satp 的指令并不是一条跳转指令， pc 只是简单的自增当前指令的字长），
        // 而它们所在的物理地址一般情况下也是相邻的，但是它们所经过的地址转换流程却是不同的——切换 satp 导致 MMU 查的多级页表 是不同的。
        // 这就要求前后两个地址空间在切换 satp 的指令 附近 的映射满足某种意义上的连续性。
        // 幸运的是，我们做到了这一点。这条写入 satp 的指令及其下一条指令都在内核内存布局的代码段中，在切换之后是一个恒等映射，
        // 而在切换之前是视为物理地址直接取指，也可以将其看成一个恒等映射。这完全符合我们的期待：即使切换了地址空间，指令仍应该 能够被连续的执行
        unsafe {
            satp::write(satp);
            // 一旦 我们修改了 satp 切换了地址空间，快表中的键值对就会失效，因为它还表示着上个地址空间的映射关系。
            // 为了 MMU 的地址转换 能够及时与 satp 的修改同步，我们可以选择立即使用 sfence.vma 指令将快表清空，
            // 这样 MMU 就不会看到快表中已经 过期的键值对了
            asm!("sfence.vma");
        }
    }
    /// Translate a virtual page number to a page table entry
    pub fn translate(&self, vpn: VirtPageNum) -> Option<PageTableEntry> {
        self.page_table.translate(vpn)
    }
    /// shrink the area to new_end
    #[allow(unused)]
    pub fn shrink_to(&mut self, start: VirtAddr, new_end: VirtAddr) -> bool {
        if let Some(area) = self
            .areas
            .iter_mut()
            .find(|area| area.vpn_range.get_start() == start.floor())
        {
            area.shrink_to(&mut self.page_table, new_end.ceil());
            true
        } else {
            false
        }
    }

    /// append the area to new_end
    #[allow(unused)]
    pub fn append_to(&mut self, start: VirtAddr, new_end: VirtAddr) -> bool {
        if let Some(area) = self
            .areas
            .iter_mut()
            .find(|area| area.vpn_range.get_start() == start.floor())
        {
            area.append_to(&mut self.page_table, new_end.ceil());
            true
        } else {
            false
        }
    }

    // /// 检查该地址空间的是否存在一个逻辑段包含了某个vpn区间
    // pub fn contains(&self, begin_vpn: VirtPageNum, end_vpn: VirtPageNum) -> bool {
    //     self.areas.iter().any(|area| area.overlaps(begin_vpn, end_vpn))
    // }

    /// 实现mmap
    pub fn mmap(
        &mut self,
        start: VirtAddr,
        end: VirtAddr,
        perm: MapPermission,
    ) -> Result<(), ()> {
        let start_vpn = start.floor();
        let end_vpn = end.ceil();
        if start_vpn > end_vpn {
            return Err(())
        }

        let new_vpn_range = VPNRange::new(start_vpn, end_vpn);

        // 1. Check for conflicts with existing MapAreas ---
        if self.areas.iter().any(|area| area.overlaps(&new_vpn_range)) {
            // warn!("mmap failed: requested area conflicts with an existing MapArea.");
            return Err(());
        }

        // 2. Create and push the new MapArea ---
        self.insert_framed_area(start, end, perm);

        Ok(())
    }

    /// 实现munmap
    pub fn munmap(&mut self, start: VirtAddr, end: VirtAddr) -> Result<(), ()> {
        let start_vpn = start.floor();
        let end_vpn = end.ceil();

        if start_vpn > end_vpn {
            return Err(());
        }

        let vpn_range = VPNRange::new(start_vpn, end_vpn);

        // 考虑在区间[start_vpn, end_vpn)区间内的是所有逻辑段
        let mut target_areas = self.areas.iter_mut()
            .filter(|area| { area.belongs_to(&vpn_range) })
            .collect::<Vec<_>>();

        // 按照start升序
        target_areas.sort_unstable_by_key(|area| {
            area.vpn_range.get_start()
        });

        // 预运行并检验
        let mut cur_vpn = start_vpn;
        for area in target_areas.iter() {
            if area.vpn_range.get_start() == cur_vpn {
                // 匹配，释放并移动到下一个
                cur_vpn = area.vpn_range.get_end();
                // area.unmap(&mut self.page_table);
            } else if area.vpn_range.get_start() < cur_vpn {
                // 说明存在重复的（遇到了已经unmap过的）
                return Err(());
            } else {
                // 由于已经排序，所以只可能是中间有空隙，也返回Err
                return Err(())
            }
        }
        if cur_vpn != end_vpn { // 最右侧存在空隙的
            return Err(());
        }

        // 到此说明全部通过，可以正常map
        for area in target_areas {
            area.unmap(&mut self.page_table);
        }
        // 从areas中移除
        self.areas.retain(|area|{!area.belongs_to(&vpn_range)});

        Ok(())
    }
}
/// map area structure, controls a contiguous piece of virtual memory
/// 以逻辑段 MapArea 为单位描述一段连续地址的虚拟内存。所谓逻辑段，就是指地址区间中的一段实际可用（即 MMU 通过查多级页表 可以正确完成地址转换）的地址连续的虚拟地址区间，该区间内包含的所有虚拟页面都以一种相同的方式映射到物理页帧，具有可读/可写/可执行等属性
pub struct MapArea {
    // 描述一段“虚拟页号”的连续区间，表示该逻辑段在地址区间中的位置和长度。它是一个迭代器，可以使用 Rust 的语法糖 for-loop 进行迭代
    // 不同逻辑段所覆盖的虚拟页号范围是绝对不会重叠的（如果最后一页不满，则会对齐，而非用于装填另一个逻辑段的数据）
    vpn_range: VPNRange,
    // 当逻辑段采用 MapType::Framed 方式映射到物理内存的时候， data_frames 是一个保存了该逻辑段内的每个虚拟页面 和它被映射到的物理页帧 FrameTracker 的一个键值对容器 BTreeMap 中，这些物理页帧被用来存放实际内存数据而不是 作为多级页表中的中间节点。（中间节点有PageTable管理，并对逻辑段透明，逻辑段只关心实际存储数据的那个页帧）
    data_frames: BTreeMap<VirtPageNum, FrameTracker>,
    // MapType 描述该逻辑段内的所有虚拟页面映射到物理页帧的同一种方式，它是一个枚举类型
    map_type: MapType,
    // MapPermission 表示控制该逻辑段的访问方式，它是页表项标志位 PTEFlags 的一个子集，仅保留 U/R/W/X 四个标志位
    map_perm: MapPermission,
}

impl MapArea {
    pub fn new(
        start_va: VirtAddr,
        end_va: VirtAddr,
        map_type: MapType,
        map_perm: MapPermission,
    ) -> Self {
        let start_vpn: VirtPageNum = start_va.floor();
        let end_vpn: VirtPageNum = end_va.ceil();
        Self {
            vpn_range: VPNRange::new(start_vpn, end_vpn),
            data_frames: BTreeMap::new(),
            map_type,
            map_perm,
        }
    }
    pub fn map_one(&mut self, page_table: &mut PageTable, vpn: VirtPageNum) {
        let ppn: PhysPageNum;
        match self.map_type { // 根据指定的类型确定ppn
            MapType::Identical => { // 当以恒等映射 Identical 方式映射的时候，物理页号就等于虚拟页号
                ppn = PhysPageNum(vpn.0);
            }
            MapType::Framed => { // 当以 Framed 方式映射的时候，需要分配一个物理页帧让当前的虚拟页面可以映射过去，此时页表项中的物理页号自然就是 这个被分配的物理页帧的物理页号。此时还需要将这个物理页帧挂在逻辑段的 data_frames 字段下
                let frame = frame_alloc().unwrap();
                ppn = frame.ppn;
                self.data_frames.insert(vpn, frame); // 如果是Framed模式，需要将其加入集合中
            }
        }
        let pte_flags = PTEFlags::from_bits(self.map_perm.bits).unwrap(); // 该逻辑段具有的权限都会被属于该逻辑段的页面继承
        page_table.map(vpn, ppn, pte_flags); // 在页表中建立实际的映射关系
    }
    #[allow(unused)]
    pub fn unmap_one(&mut self, page_table: &mut PageTable, vpn: VirtPageNum) {
        if self.map_type == MapType::Framed {
            self.data_frames.remove(&vpn); // 删除vpn对应的entry
        }
        page_table.unmap(vpn); // 从页表中删除对应的映射关系
    }
    pub fn map(&mut self, page_table: &mut PageTable) { // 对该逻辑段的每个虚拟页号都进行映射
        for vpn in self.vpn_range {
            self.map_one(page_table, vpn);
        }
    }
    #[allow(unused)]
    pub fn unmap(&mut self, page_table: &mut PageTable) {
        for vpn in self.vpn_range {
            self.unmap_one(page_table, vpn);
        }
    }
    #[allow(unused)]
    pub fn shrink_to(&mut self, page_table: &mut PageTable, new_end: VirtPageNum) {
        for vpn in VPNRange::new(new_end, self.vpn_range.get_end()) {
            self.unmap_one(page_table, vpn)
        }
        self.vpn_range = VPNRange::new(self.vpn_range.get_start(), new_end);
    }
    #[allow(unused)]
    pub fn append_to(&mut self, page_table: &mut PageTable, new_end: VirtPageNum) {
        for vpn in VPNRange::new(self.vpn_range.get_end(), new_end) {
            self.map_one(page_table, vpn)
        }
        self.vpn_range = VPNRange::new(self.vpn_range.get_start(), new_end);
    }
    /// data: start-aligned but maybe with shorter length
    /// assume that all frames were cleared before
    pub fn copy_data(&mut self, page_table: &mut PageTable, data: &[u8]) {
        // 将切片 data 中的数据拷贝到当前逻辑段实际被内核放置在的各物理页帧上，从而 在地址空间中通过该逻辑段就能访问这些数据
        // 切片 data 中的数据大小不超过当前逻辑段的 总大小，且切片中的数据会被对齐到逻辑段的开头，然后逐页拷贝到实际的物理页帧
        assert_eq!(self.map_type, MapType::Framed);
        let mut start: usize = 0;
        let mut current_vpn = self.vpn_range.get_start();
        let len = data.len();
        loop {
            let src = &data[start..len.min(start + PAGE_SIZE)];
            // 第 39 行从传入的当前逻辑段所属的地址空间的多级页表中手动查找迭代到的虚拟页号被映射 到的物理页帧，并通过 get_bytes_array 方法获取能够真正改写该物理页帧上内容的字节数组型可变引用，最后再获取它 的切片用于数据拷贝
            let dst = &mut page_table
                .translate(current_vpn)
                .unwrap()
                .ppn()
                .get_bytes_array()[..src.len()]; // 根据current_vpn得到对应的ppn，并读取len个内容
            dst.copy_from_slice(src); // 执行实际的copy
            start += PAGE_SIZE;
            if start >= len {
                break;
            }
            current_vpn.step(); // 处理下一页（虚拟）
        }
    }

    /// 检查当前逻辑段是否与指定虚拟页号区间存在交集
    pub fn overlaps(&self, vpnrange: &VPNRange) -> bool {
        self.vpn_range.get_start() < vpnrange.get_end() && vpnrange.get_start() < self.vpn_range.get_end()
    }

    /// 检查当前逻辑段是否属于指定的虚拟页号区间
    pub fn belongs_to(&self, other: &VPNRange) -> bool {
        self.vpn_range.get_start() >= other.get_start() && self.vpn_range.get_end() <= other.get_end()
    }
}

#[derive(Copy, Clone, PartialEq, Debug)]
/// map type for memory set: identical or framed
pub enum MapType {
    // VA = PA，对于给定的vpn，它对应的ppn==vpn。通常用于内核地址空间
    Identical,
    // VA != PA，每个虚拟页面都对应了一个物理页帧，但是对应关系是相对随机的（需要通过页表记录）
    Framed,
}

bitflags! {
    /// map permission corresponding to that in pte: `R W X U`
    pub struct MapPermission: u8 {
        ///Readable
        const R = 1 << 1;
        ///Writable
        const W = 1 << 2;
        ///Excutable
        const X = 1 << 3;
        ///Accessible in U mode
        const U = 1 << 4;
    }
}

/// Return (bottom, top) of a kernel stack in kernel space.
pub fn kernel_stack_position(app_id: usize) -> (usize, usize) {
    // 定义指定用户程序的内核栈的位置
    let top = TRAMPOLINE - app_id * (KERNEL_STACK_SIZE + PAGE_SIZE);
    let bottom = top - KERNEL_STACK_SIZE;
    (bottom, top)
}

/// remap test in kernel space
#[allow(unused)]
pub fn remap_test() {
    let mut kernel_space = KERNEL_SPACE.exclusive_access();
    let mid_text: VirtAddr = ((stext as usize + etext as usize) / 2).into();
    let mid_rodata: VirtAddr = ((srodata as usize + erodata as usize) / 2).into();
    let mid_data: VirtAddr = ((sdata as usize + edata as usize) / 2).into();
    assert!(!kernel_space
        .page_table
        .translate(mid_text.floor())
        .unwrap()
        .writable(),);
    assert!(!kernel_space
        .page_table
        .translate(mid_rodata.floor())
        .unwrap()
        .writable(),);
    assert!(!kernel_space
        .page_table
        .translate(mid_data.floor())
        .unwrap()
        .executable(),);
    println!("remap_test passed!");
}
