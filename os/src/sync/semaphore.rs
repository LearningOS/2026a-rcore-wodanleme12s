//! Semaphore

use super::deadlock::{self, DEADLOCK};
use crate::sync::UPSafeCell;
use crate::task::{block_current_and_run_next, current_task, wakeup_task, TaskControlBlock};
use alloc::{collections::VecDeque, sync::Arc, vec::Vec};

/// Semaphore struct
pub struct Semaphore {
    /// The inner data of Semaphore
    pub inner: UPSafeCell<SemaphoreInner>,
}

/// The inner data of Semaphore
pub struct SemaphoreInner {
    /// Available units. Negative means "these many tasks are parked and still
    /// owed a unit", which is why `wait_queue` has exactly `-count` entries.
    pub count: isize,
    /// Initial value passed to `semaphore_create`. Kept so that the number of
    /// units currently handed out (`initial - count`) can be recovered by the
    /// deadlock detector without maintaining a holder list here.
    pub initial: isize,
    pub wait_queue: VecDeque<Arc<TaskControlBlock>>,
    /// Identity of this semaphore in the wait-for graph:
    /// `(pid, index in semaphore_list)`.
    pub key: (usize, usize),
}

impl Semaphore {
    /// Create a new semaphore known to the deadlock detector as `key`.
    pub fn new(res_count: usize, key: (usize, usize)) -> Self {
        trace!("kernel: Semaphore::new");
        Self {
            inner: unsafe {
                UPSafeCell::new(SemaphoreInner {
                    count: res_count as isize,
                    initial: res_count as isize,
                    wait_queue: VecDeque::new(),
                    key,
                })
            },
        }
    }

    /// Borrow the state, for the detector's graph rebuild.
    pub fn inner_ref(&self) -> core::cell::RefMut<'_, SemaphoreInner> {
        self.inner.exclusive_access()
    }

    /// Tids of a wait queue, without cloning task arcs. A task with no `res`
    /// (impossible for user tasks) is keyed out of range so it never matches.
    pub(crate) fn queued_tids(q: &VecDeque<Arc<TaskControlBlock>>) -> Vec<usize> {
        q.iter()
            .map(|t| {
                t.inner_exclusive_access()
                    .res
                    .as_ref()
                    .map(|r| r.tid)
                    .unwrap_or(usize::MAX)
            })
            .collect()
    }

    /// Remove a specific waiter from the queue, by identity rather than position:
    /// a refused victim can sit anywhere in a FIFO queue.
    pub(crate) fn remove_waiter(&self, task: &Arc<TaskControlBlock>) {
        let mut inner = self.inner.exclusive_access();
        if let Some(pos) = inner
            .wait_queue
            .iter()
            .position(|t| Arc::ptr_eq(t, task))
        {
            inner.wait_queue.remove(pos);
        }
    }

    /// up operation of semaphore
    ///
    /// A released unit either goes straight to the head of the wait queue (when
    /// somebody is already parked, i.e. `count <= 0` after the increment) or
    /// becomes available for the next `down`. Only the bookkeeping differs: the
    /// woken task stops being a waiter and starts being a holder, while the second
    /// case just grows the pool. Nothing else has to be recomputed, because the
    /// detector derives its graph from `count`/`initial` on the next acquire.
    pub fn up(&self) {
        trace!("kernel: Semaphore::up");
        let mut inner = self.inner.exclusive_access();
        inner.count += 1;
        let key = inner.key;
        if inner.count <= 0 {
            if let Some(task) = inner.wait_queue.pop_front() {
                drop(inner);
                // The unit goes straight to the head of the queue: that task stops
                // waiting and starts holding, while we give one of ours back.
                deadlock::release_me(&[key]);
                deadlock::transfer(key, &task);
                wakeup_task(task);
                return;
            }
        }
        drop(inner);
        // Nobody was parked: the unit returns to the pool, so our grant count drops.
        deadlock::release_me(&[key]);
    }

    /// down operation of semaphore
    ///
    /// `0` when a unit was taken, `-0xdead` when taking one would deadlock.
    pub fn down(&self) -> isize {
        trace!("kernel: Semaphore::down");
        let (key, count) = {
            let inner = self.inner.exclusive_access();
            (inner.key, inner.count)
        };
        // A refusal left for us by an earlier detection round wins over anything
        // else: we were picked as a cycle victim while parked on this semaphore.
        if let Some(code) = deadlock::take_abort_for(key) {
            return code;
        }
        if count > 0 {
            // A unit is there for the taking; no wait-for edge is ever created.
            let mut inner = self.inner.exclusive_access();
            inner.count -= 1;
            drop(inner);
            deadlock::grant_me(key);
            return 0;
        }
        // count <= 0: this down has to park somebody, so ask the detector first,
        // before the count or the queue are touched.
        let me = current_task().unwrap();
        let victim = match deadlock::check(key) {
            Some(v) => v,
            None => {
                deadlock::park_me(key);
                {
                    let mut inner = self.inner.exclusive_access();
                    inner.count -= 1;
                    inner.wait_queue.push_back(me);
                    drop(inner);
                    block_current_and_run_next();
                }
                // Resumed with a unit: `up` moved us out of the queue and recorded
                // the grant for us, so only the parked marker is left to clear.
                deadlock::clear_parked_me(key);
                return 0;
            }
        };
        // Somebody on the ring must be refused. When that is another task it is
        // already queued, so `refuse_queued` pulls it out and wakes it; we then park
        // as usual, because the edge the graph keeps is ours. When the victim *is*
        // the caller there is nobody to pull out and joining the queue would be
        // wrong — the object's counter only owes a unit to its queued waiters, so we
        // return without touching either.
        if deadlock::refuse_if_not_me(victim, key) {
            deadlock::park_me(key);
            let mut inner = self.inner.exclusive_access();
            inner.count -= 1;
            inner.wait_queue.push_back(me);
            drop(inner);
            block_current_and_run_next();
            deadlock::clear_parked_me(key);
            return 0;
        }
        DEADLOCK
    }
}
