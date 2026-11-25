//! Process management syscalls
use crate::{
    mm::{copy_to_user, read_u8, write_u8},
    task::{current_syscall_count, exit_current_and_run_next, suspend_current_and_run_next},
    timer::get_time_us,
};

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

/// Encoding of `user_lib::TraceRequest`.
const TRACE_READ: usize = 0;
const TRACE_WRITE: usize = 1;
const TRACE_SYSCALL: usize = 2;

/// task exits and submit an exit code
pub fn sys_exit(exit_code: i32) -> ! {
    trace!("[kernel] Application exited with code {}", exit_code);
    exit_current_and_run_next();
    panic!("Unreachable in sys_exit!");
}

/// current task gives up resources for other tasks
pub fn sys_yield() -> isize {
    trace!("kernel: sys_yield");
    suspend_current_and_run_next();
    0
}

/// get time with second and microsecond
pub fn sys_get_time(ts: *mut TimeVal, _tz: usize) -> isize {
    trace!("kernel: sys_get_time");
    let us = get_time_us();
    let timeval = TimeVal {
        sec: us / 1_000_000,
        usec: us % 1_000_000,
    };
    // One gated copy instead of a raw dereference: a bogus address now returns
    // -1 rather than faulting inside the kernel or scribbling over memory the
    // caller does not own.
    if copy_to_user(ts as usize, core::slice::from_ref(&timeval)) {
        0
    } else {
        -1
    }
}

/// Single-step tracing facility used by the ch3 test `ch3_trace`.
///
/// `request` selects one of three operations (see the `TRACE_*` constants):
///
/// - read/write a single byte of the *calling task's* memory, validated by the
///   same user pointer gate every other syscall uses, so `trace` cannot be
///   used to peek at kernel memory or at another task's stack;
/// - report how often the calling task has invoked syscall `id`.
///
/// Counting happens in the trap entry before dispatch (see
/// [`crate::trap`]), which is why asking about `SYSCALL_TRACE` itself already
/// includes the question being answered.
pub fn sys_trace(trace_request: usize, id: usize, data: usize) -> isize {
    trace!("kernel: sys_trace");
    match trace_request {
        // One byte each, resolved by the same gate every other syscall uses.
        TRACE_READ => read_u8(id).map(|v| v as isize).unwrap_or(-1),
        TRACE_WRITE => write_u8(id, data as u8).map(|_| 0).unwrap_or(-1),
        TRACE_SYSCALL => match current_syscall_count(id) {
            Some(count) => count as isize,
            // untracked syscall id
            None => -1,
        },
        _ => -1,
    }
}
