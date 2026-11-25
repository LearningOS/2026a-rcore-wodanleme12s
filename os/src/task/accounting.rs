//! Per-task bookkeeping that syscalls other than the scheduler need.
//!
//! [`SyscallCounter`] is the record `sys_trace` reads: how many times the task
//! has invoked each syscall number. It lives in the task control block rather
//! than in a global table so that it dies with the task and cannot be read or
//! forged through another task's window.

/// Number of distinct syscall ids we keep counters for. The course test suite
/// uses ids up to 473 (`SYSCALL_CONDVAR_WAIT`); anything at or above this
/// bound is reported as "not tracked" instead of panicking, because a syscall
/// id is user input.
pub const SYSCALL_ID_TRACKED: usize = 512;

/// Invocation counters for one task.
#[derive(Clone)]
pub struct SyscallCounter {
    /// calls so far, indexed by syscall id
    counts: [usize; SYSCALL_ID_TRACKED],
}

impl SyscallCounter {
    /// A fresh counter: nothing called yet.
    pub fn zero_init() -> Self {
        Self {
            counts: [0; SYSCALL_ID_TRACKED],
        }
    }

    /// Record one invocation of `id`. Ids outside the tracked range are
    /// ignored rather than fatal — the kernel must survive a bogus `a7`.
    pub fn record(&mut self, id: usize) {
        if let Some(slot) = self.counts.get_mut(id) {
            *slot += 1;
        }
    }

    /// How many times `id` was invoked so far, or `None` if it is not tracked.
    pub fn count(&self, id: usize) -> Option<usize> {
        self.counts.get(id).copied()
    }
}
