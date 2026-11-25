//! Process management syscalls
use crate::mm::{read_u8, write_struct, write_u8};
use crate::task::{
    change_program_brk, exit_current_and_run_next, suspend_current_and_run_next, syscall_count,
    with_current_memory_set,
};

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

/// task exits and submit an exit code
pub fn sys_exit(_exit_code: i32) -> ! {
    trace!("kernel: sys_exit");
    exit_current_and_run_next();
    panic!("Unreachable in sys_exit!");
}

/// current task gives up resources for other tasks
pub fn sys_yield() -> isize {
    trace!("kernel: sys_yield");
    suspend_current_and_run_next();
    0
}

/// Microseconds since the machine booted, split the way `struct timeval` wants
/// it. `user_lib::get_time()` rebuilds milliseconds as `sec * 1000 + usec / 1000`
/// in `isize`, so `sec` must be returned unmasked: a 16-bit mask would wrap after
/// 18 hours of wall time and make ch3_sleep's busy-wait loop never terminate.
fn time_val_now() -> TimeVal {
    let us = crate::timer::get_time_us();
    TimeVal {
        sec: us / 1_000_000,
        usec: us % 1_000_000,
    }
}

/// get time with second and microsecond
///
/// `ts` is a user pointer to a stack-local `TimeVal`, so the write goes through
/// the page-table gate; a bad pointer yields `-1` instead of faulting the kernel.
pub fn sys_get_time(ts: *mut TimeVal, _tz: usize) -> isize {
    trace!("kernel: sys_get_time");
    let now = time_val_now();
    if write_struct(ts as usize, &now) {
        0
    } else {
        -1
    }
}

/// The three flavours of `sys_trace`, matching `user_lib::TraceRequest`.
const TRACE_READ: usize = 0;
const TRACE_WRITE: usize = 1;
const TRACE_SYSCALL: usize = 2;

/// Inspect one byte of user memory, poke one byte, or read a syscall counter.
///
/// Read/write legality is decided by the current task's page table (U plus R/W),
/// which is what makes a `PROT_READ` mapping readable but not writable, and a
/// `munmap`'d address unreadable again. Kernel addresses have no U-mapped PTE,
/// so they are refused without a hardcoded range check.
pub fn sys_trace(trace_request: usize, id: usize, data: usize) -> isize {
    trace!("kernel: sys_trace");
    match trace_request {
        TRACE_READ => read_u8(id).map(|byte| byte as isize).unwrap_or(-1),
        TRACE_WRITE => {
            if write_u8(id, data as u8).is_some() {
                0
            } else {
                -1
            }
        }
        TRACE_SYSCALL => match syscall_count(id) {
            Some(count) => count as isize,
            None => -1,
        },
        _ => -1,
    }
}

/// Map anonymous zeroed pages at a fixed address with the given protections.
///
/// Validation happens here so that the illegal requests the tests probe
/// (unaligned length, `prot == 0`, `prot & 8`, overlapping an existing mapping)
/// return `-1`; the application never gets to touch a page we refused.
pub fn sys_mmap(start: usize, len: usize, prot: usize) -> isize {
    trace!("kernel: sys_mmap");
    with_current_memory_set(|ms| ms.mmap(start, len, prot))
}

/// Tear down a mapping created by `sys_mmap` and return its frames.
pub fn sys_munmap(start: usize, len: usize) -> isize {
    trace!("kernel: sys_munmap");
    if with_current_memory_set(|ms| ms.munmap(start, len)) {
        0
    } else {
        -1
    }
}

/// change data segment size
pub fn sys_sbrk(size: i32) -> isize {
    trace!("kernel: sys_sbrk");
    if let Some(old_brk) = change_program_brk(size) {
        old_brk as isize
    } else {
        -1
    }
}
