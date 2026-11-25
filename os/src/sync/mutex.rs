//! Mutex (spin-like and blocking(sleep))

use super::deadlock::{self, DEADLOCK};
use super::UPSafeCell;
use crate::task::TaskControlBlock;
use crate::task::{block_current_and_run_next, suspend_current_and_run_next};
use crate::task::{current_task, wakeup_task};
use alloc::{collections::VecDeque, sync::Arc, vec::Vec};

/// Mutex trait
pub trait Mutex: Sync + Send {
    /// Lock the mutex. `0` on success, a negative errno when the acquire was
    /// refused (only blocking mutexes with deadlock detection enabled refuse).
    fn lock(&self) -> isize;
    /// Unlock the mutex. `false` means the caller did not own it, which is an
    /// error userspace must see instead of a kernel panic.
    fn unlock(&self) -> bool;
    /// View this object as a blocking mutex, for the detector's graph rebuild.
    /// Spin mutexes park nobody, so they answer `None` and stay out of the graph.
    fn as_blocking(&self) -> Option<&MutexBlocking> {
        None
    }
}

/// Spinlock Mutex struct
pub struct MutexSpin {
    locked: UPSafeCell<bool>,
}

impl MutexSpin {
    /// Create a new spinlock mutex
    pub fn new() -> Self {
        trace!("kernel: MutexSpin::new");
        Self {
            locked: unsafe { UPSafeCell::new(false) },
        }
    }
}

impl Mutex for MutexSpin {
    /// Lock the spinlock mutex
    fn lock(&self) -> isize {
        trace!("kernel: MutexSpin::lock");
        loop {
            let mut locked = self.locked.exclusive_access();
            if *locked {
                // Drop the borrow *before* yielding. `exclusive_access` is a
                // `RefCell::borrow_mut`, so holding it across
                // `suspend_current_and_run_next` would keep the flag borrowed for
                // as long as that task owns the CPU. The next holder of this mutex
                // finishes by calling `unlock`, which borrows the very same cell —
                // and a second `borrow_mut` on top of ours panics inside the kernel,
                // where `trap_from_kernel` (`src/trap/mod.rs`) turns any panic into
                // "the whole QEMU session is over". Whether the victim of that panic
                // is another ch8 test or an unrelated one depends only on who gets
                // scheduled after us, which is why the failure looks intermittent.
                drop(locked);
                suspend_current_and_run_next();
                continue;
            } else {
                *locked = true;
                return 0;
            }
        }
    }

    /// Unlock the spinlock mutex
    fn unlock(&self) -> bool {
        trace!("kernel: MutexSpin::unlock");
        let mut locked = self.locked.exclusive_access();
        if !*locked {
            return false;
        }
        *locked = false;
        true
    }
}

/// Blocking Mutex sturct
pub struct MutexBlocking {
    inner: UPSafeCell<MutexBlockingInner>,
}

/// The state of a blocking mutex.
///
/// `owner` is what makes the whole detection design work without a resource table:
/// the object itself names who holds it, so the detector can ask instead of being
/// told. It mirrors `locked` exactly.
pub struct MutexBlockingInner {
    pub(crate) locked: bool,
    pub(crate) wait_queue: VecDeque<Arc<TaskControlBlock>>,
    pub(crate) owner: Option<Arc<TaskControlBlock>>,
    /// Identity of this mutex in the wait-for graph: `(pid, index in mutex_list)`.
    key: (usize, usize),
}

impl MutexBlocking {
    /// Create a new blocking mutex known to the deadlock detector as `key`.
    pub fn new(key: (usize, usize)) -> Self {
        trace!("kernel: MutexBlocking::new");
        Self {
            inner: unsafe {
                UPSafeCell::new(MutexBlockingInner {
                    locked: false,
                    wait_queue: VecDeque::new(),
                    owner: None,
                    key,
                })
            },
        }
    }

    /// Borrow the state, for the detector's graph rebuild.
    pub fn inner_ref(&self) -> core::cell::RefMut<'_, MutexBlockingInner> {
        self.inner.exclusive_access()
    }

    /// Is the lock currently held by `task`?
    pub fn is_held_by(&self, task: &Arc<TaskControlBlock>) -> bool {
        let inner = self.inner.exclusive_access();
        inner
            .owner
            .as_ref()
            .map(|owner| Arc::ptr_eq(owner, task))
            .unwrap_or(false)
    }

    /// Remove a specific waiter, by identity rather than position: a refused victim
    /// can sit anywhere in a FIFO queue.
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
}

impl MutexBlockingInner {
    /// Tids parked in this mutex's queue.
    pub fn queued_tids(&self) -> Vec<usize> {
        crate::sync::semaphore::Semaphore::queued_tids(&self.wait_queue)
    }

    /// The tid of the current owner, if any.
    pub fn owner_tid(&self) -> Option<usize> {
        self.owner.as_ref().and_then(|t| {
            t.inner_exclusive_access()
                .res
                .as_ref()
                .map(|r| r.tid)
        })
    }
}

impl Mutex for MutexBlocking {
    /// lock the blocking mutex
    ///
    /// Returns `0` once the lock is ours, `-0xdead` when taking it would deadlock
    /// (only possible with deadlock detection enabled).
    fn lock(&self) -> isize {
        trace!("kernel: MutexBlocking::lock");
        let (locked, key) = {
            let inner = self.inner.exclusive_access();
            (inner.locked, inner.key)
        };
        // A refusal left for us by an earlier detection round wins: we were picked
        // as a cycle victim while parked here. Checked before anything else so a
        // punished thread never re-enters the queue it was pulled out of.
        if let Some(code) = deadlock::take_abort_for(key) {
            return code;
        }
        if !locked {
            let mut inner = self.inner.exclusive_access();
            let me = current_task().unwrap();
            inner.locked = true;
            inner.owner = Some(Arc::clone(&me));
            let key = inner.key;
            drop(inner);
            deadlock::grant_me(key);
            return 0;
        }
        // Contended: ask the detector before joining the queue.
        let task = current_task().unwrap();
        if let Some(victim) = deadlock::check(key) {
            if !deadlock::refuse_if_not_me(victim, key) {
                return DEADLOCK;
            }
        }
        deadlock::park_me(key);
        {
            let mut inner = self.inner.exclusive_access();
            inner.wait_queue.push_back(task);
            drop(inner);
            block_current_and_run_next();
        }
        // `unlock` transferred ownership straight to us and recorded the grant on
        // our behalf; all that is left is to drop the parked marker.
        deadlock::clear_parked_me(key);
        0
    }

    /// unlock the blocking mutex
    fn unlock(&self) -> bool {
        trace!("kernel: MutexBlocking::unlock");
        let mut mutex_inner = self.inner.exclusive_access();
        if !mutex_inner.locked {
            return false;
        }
        // The lock belongs to nobody in the instant between here and the hand-off.
        mutex_inner.owner = None;
        if let Some(waking_task) = mutex_inner.wait_queue.pop_front() {
            // Ownership transfers straight to the head of the queue.
            mutex_inner.locked = true;
            mutex_inner.owner = Some(Arc::clone(&waking_task));
            let key = mutex_inner.key;
            drop(mutex_inner);
            deadlock::release_me(&[key]);
            deadlock::transfer(key, &waking_task);
            wakeup_task(waking_task);
        } else {
            let key = mutex_inner.key;
            mutex_inner.locked = false;
            drop(mutex_inner);
            deadlock::release_me(&[key]);
        }
        true
    }

    /// Downcast hook used by the deadlock detector.
    fn as_blocking(&self) -> Option<&MutexBlocking> {
        Some(self)
    }
}
