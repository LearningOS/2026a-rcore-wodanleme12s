//! Per-task syscall accounting, the data behind `sys_trace(Syscall)`.
//!
//! The contract (BRIEF3 §4.5) is that a task can ask "how many times have I
//! issued syscall number N so far", and that the answer *includes* the
//! `sys_trace` call being answered. That only works if the counter is bumped
//! before dispatch, which is what [`record`] is for; `trap_handler` calls it
//! for every environment call.
//!
//! `MAX_SYSCALL_NUM` comes from `config.rs`, so the table size is a kernel
//! constant rather than something a user-supplied `id` can influence.

use crate::config::MAX_SYSCALL_NUM;

/// One task's syscall counters.
pub struct SyscallAccounting {
    /// calls issued so far, indexed by syscall number
    counts: [usize; MAX_SYSCALL_NUM],
}

impl SyscallAccounting {
    /// A fresh table with every counter at zero.
    pub fn new() -> Self {
        Self {
            counts: [0; MAX_SYSCALL_NUM],
        }
    }

    /// Count one invocation of `syscall_id`. Out-of-range ids are ignored:
    /// they cannot index the table, and the user does not get to choose its
    /// size.
    pub fn record(&mut self, syscall_id: usize) {
        if let Some(slot) = self.counts.get_mut(syscall_id) {
            *slot += 1;
        }
    }

    /// How many times has this task issued `syscall_id`?
    pub fn count(&self, syscall_id: usize) -> Option<usize> {
        self.counts.get(syscall_id).copied()
    }
}
