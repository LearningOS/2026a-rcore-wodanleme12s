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
/// the max number of syscall
pub const MAX_SYSCALL_NUM: usize = 500;
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
/// Highest virtual address a fixed `mmap` may reach (exclusive).
///
/// The application image is position-independent and lives very low, while the
/// shared trampoline and trap-frame pages sit at the top of the SV39 space, so
/// everything in between is fair game. The real boundary is where SV39's 39-bit
/// non-sign-extended window ends (`0x40_0000_0000`): above that a virtual page
/// number would need more than three levels of indexes and would alias onto the
/// sign-extended kernel half. A generous ceiling keeps the policy "only what is
/// actually owned blocks you", instead of a guessed floor or mid-range cap.
pub const USER_MMAP_LIMIT: usize = 0x4000_0000_00;
