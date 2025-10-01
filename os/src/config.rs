//! Constants in the kernel

#[allow(unused)]

/// user app's stack size
pub const USER_STACK_SIZE: usize = 4096 * 2;
/// kernel stack size
pub const KERNEL_STACK_SIZE: usize = 4096 * 2;
/// kernel heap size
pub const KERNEL_HEAP_SIZE: usize = 0x200_0000;

/// page size : 4KB
pub const PAGE_SIZE: usize = 0x1000;
/// page size bits: 12
pub const PAGE_SIZE_BITS: usize = 0xc;
/// the virtual addr of trapoline
pub const TRAMPOLINE: usize = usize::MAX - PAGE_SIZE + 1;
/// the virtual addr of trap context
pub const TRAP_CONTEXT_BASE: usize = TRAMPOLINE - PAGE_SIZE;
/// clock frequency
pub const CLOCK_FREQ: usize = 12500000;
/// the physical memory end
pub const MEMORY_END: usize = 0x88000000;
/// The base address of control registers in Virtio_Block device
/// MMIO：将外部设备（如显卡、网卡、磁盘控制器等）的硬件寄存器和内存，“映射”到CPU的物理地址空间中。 这样一来，CPU就不再需要使用特殊的指令来与这些设备通信。它可以像读写普通内存（RAM）一样，使用标准的 load（加载）和 store（存储）指令来访问这些设备的寄存器，从而控制设备或交换数据
/// 对于采用MMIO（内存映射I/O）的设备连接方式，通过查看Qemu for RISC-V 64平台的源码，可以发现MMIO物理地址区间从0x10001000开头的4KB
pub const MMIO: &[(usize, usize)] = &[(0x10001000, 0x1000)];
