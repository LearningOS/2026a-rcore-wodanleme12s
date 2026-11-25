//!Implementation of [`TaskManager`]
use super::TaskControlBlock;
use crate::sync::UPSafeCell;
use alloc::collections::BinaryHeap;
use alloc::sync::Arc;
use lazy_static::*;

/// Virtual-time weight a task is charged for one tick at priority 1.
///
/// A task's charge per tick is `WEIGHT / priority`, so priorities 1..N map to
/// charges that stay integral and comparable without floating point. The value
/// is deliberately small enough that even `isize::MAX` priorities cannot make
/// an addition overflow: the pass counter only ever grows by `WEIGHT` per tick,
/// never by anything derived from a user-supplied priority.
const STRIDE_WEIGHT: usize = 1 << 20;

/// Stride key: virtual time first, then arrival order.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct StrideKey {
    /// virtual time already charged (smaller runs first)
    pass: usize,
    /// FIFO tie-break among equal passes
    seq: u64,
}

/// A queued task: its stride key plus the TCB.
///
/// Ordering is spelled out by hand instead of derived from a tuple containing
/// `Arc<TaskControlBlock>` — deriving would demand `Ord` for the TCB itself,
/// which has nothing to do with scheduling and would be an odd thing to define.
struct QueueEntry {
    key: StrideKey,
    task: Arc<TaskControlBlock>,
}

impl PartialEq for QueueEntry {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl Eq for QueueEntry {}

impl PartialOrd for QueueEntry {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for QueueEntry {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        // Reversed: BinaryHeap is a max-heap, and the *least* pass runs first.
        other.key.cmp(&self.key)
    }
}

/// The ready queue, ordered by (pass, sequence number).
pub struct TaskManager {
    ready_queue: BinaryHeap<QueueEntry>,
    /// next tie-breaker; handed out on every enqueue so nothing starves
    next_seq: u64,
}

impl TaskManager {
    ///Creat an empty TaskManager
    pub fn new() -> Self {
        Self {
            ready_queue: BinaryHeap::new(),
            next_seq: 0,
        }
    }
    /// Add process back to ready queue.
    ///
    /// The task's pass is charged here — this is the single place where virtual
    /// time advances, which is what makes preemption (`suspend_current_and_run_
    /// next`, which re-enqueues), blocking wakeups and fresh spawns all share
    /// one accounting rule.
    pub fn add(&mut self, task: Arc<TaskControlBlock>) {
        let mut inner = task.inner_exclusive_access();
        let charge = STRIDE_WEIGHT / inner.priority;
        inner.stride_pass = inner.stride_pass.saturating_add(charge);
        let seq = self.next_seq;
        self.next_seq += 1;
        inner.stride_seq = seq;
        let key = StrideKey {
            pass: inner.stride_pass,
            seq,
        };
        drop(inner);
        self.ready_queue.push(QueueEntry { key, task });
    }
    /// Take the most overdue process out of the ready queue.
    ///
    /// Stale entries can exist: a task whose priority changed while it sat in
    /// the queue was re-based, so the copy in the heap carries an old key. Its
    /// status tells us — a task we hand out is `Ready`, and the stale duplicate
    /// later finds itself already `Running`, so it is skipped rather than run
    /// twice.
    pub fn fetch(&mut self) -> Option<Arc<TaskControlBlock>> {
        loop {
            let entry = self.ready_queue.pop()?;
            let task = entry.task;
            let status = task.inner_exclusive_access().task_status;
            if status == super::TaskStatus::Ready {
                return Some(task);
            }
        }
    }
    /// Smallest pass among tasks waiting right now.
    ///
    /// Used by `sys_set_priority` to re-base a task at the head of the line
    /// instead of at the front of somebody else's imagination. `usize::MAX` when
    /// the queue is empty means "this task is the only one", which is harmless:
    /// its own next charge brings it back down to a normal value.
    pub fn min_pass(&self) -> usize {
        self.ready_queue
            .iter()
            .map(|entry| entry.key.pass)
            .min()
            .unwrap_or(usize::MAX)
    }
}

lazy_static! {
    /// TASK_MANAGER instance through lazy_static!
    pub static ref TASK_MANAGER: UPSafeCell<TaskManager> =
        unsafe { UPSafeCell::new(TaskManager::new()) };
}

/// Add process to ready queue
pub fn add_task(task: Arc<TaskControlBlock>) {
    //trace!("kernel: TaskManager::add_task");
    TASK_MANAGER.exclusive_access().add(task);
}

/// Take a process out of the ready queue
pub fn fetch_task() -> Option<Arc<TaskControlBlock>> {
    //trace!("kernel: TaskManager::fetch_task");
    TASK_MANAGER.exclusive_access().fetch()
}

/// Smallest virtual pass currently waiting in the ready queue.
pub fn min_runnable_pass() -> usize {
    TASK_MANAGER.exclusive_access().min_pass()
}
