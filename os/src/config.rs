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
/// the physical base address of RAM (and of the kernel image)
pub const BASE_ADDRESS: usize = 0x8020_0000;
/// The base address of control registers in Virtio_Block device
pub const MMIO: &[(usize, usize)] = &[(0x10001000, 0x1000)];
/// Highest virtual address a fixed `mmap` may reach (exclusive).
///
/// Same ceiling as in ch4/ch5: the application image is position-independent and
/// lives very low, trampoline/trap-frame sit at the top of the SV39 space, and the
/// hard limit is where SV39's 39-bit non-sign-extended window ends.
pub const USER_MMAP_LIMIT: usize = 0x4000_0000_00;
