//! Deadlock detection: a bipartite wait-for graph rebuilt from scratch on demand.
//!
//! This module keeps **no edges of its own**. Every time an acquire would have to
//! park its caller, [`check`] walks the objects that already exist and derives the
//! whole graph from them, then answers one question: starting from the object being
//! asked for, can we get back to the asking task? Because the graph is rebuilt
//! rather than incrementally maintained, there is nothing to invalidate when a grant
//! is handed over, a queue is served, or a thread dies mid-hold — the three places
//! where an incrementally kept graph goes stale and starts inventing cycles.
//!
//! # Where each kind of edge comes from
//!
//! * **resource -> task** ("who stands between this object and its next grant"):
//!   read straight out of the object. A [`MutexBlocking`] names its `owner`; a
//!   [`Semaphore`] has no such field, so the units it handed out are recovered by
//!   counting grants (`initial - current`) across the address space's tasks.
//! * **task -> resource**: the task's own `blocked_in` field plus its grant counts.
//!   Both live in the TCB, written by the primitives themselves.
//!
//! So the only state the detector trusts is (a) the primitives' own queues/owners
//! and (b) per-task grant counts. There is no resource table, no registry keyed by
//! object, and no `Arc` cycle bookkeeping — deliberately unlike the `sync/resource.rs`
//! style of keeping a parallel list of "resources" that must be remembered to update.
//!
//! # Why the *available* count matters (`ch8_deadlock_sem2`)
//!
//! A counting semaphore with `k > 0` units free cannot make anybody wait, so those
//! `k` units are subtracted before the walk continues through its holders. With two
//! philosophers and two forks nobody waits at all; if you instead pretend a
//! semaphore is binary and follow every holder, that same picture looks like a
//! cycle. Reading the remaining capacity first is what turns sem2 from a false
//! positive into a no-op.
//!
//! # Who gets refused (`ch8_deadlock_sem1`)
//!
//! Detection alone is not enough. In sem1 the three threads allocate, keep *running*
//! after each syscall, and only later ask for the next resource, so the request that
//! closes the ring arrives while its author is still holding something else and will
//! go on to release it. Refusing *that* task buys nothing: the ring stays shut behind
//! it and everybody sleeps until the 30s QEMU budget is spent. The rule implemented
//! by [`pick_victim`] is therefore the textbook "abort the transaction that does not
//! release anything else", expressed on this derived graph as: among the cycle
//! members, prefer one whose pending request is queued **behind at least one other
//! waiter** on the same object — aborting it leaves the front waiter satisfiable, so
//! the ring really opens. Only when every member is at the head of its queue (the
//! self-recursion case of mutex1) is the caller itself refused.
//!
//! Condvars contribute no edge: they park a task until somebody signals, not until a
//! resource is granted, so `test_condvar`, `mpsc_sem` and `sync_sem` are untouched.
//! Spin mutexes busy-wait without parking anybody, so they are neither nodes nor
//! edges, and `race_adder_mutex_spin` never appears in this graph.

use super::{Mutex, Semaphore};
use crate::task::{current_task, pid2process, wakeup_task, TaskControlBlock};
use alloc::sync::Arc;
use alloc::vec::Vec;
use lazy_static::*;

/// Error code the ch8 tests expect from an acquire that would deadlock.
pub const DEADLOCK: isize = -0xdead;

/// A resource node: `(pid, index into that process' mutex_list/semaphore_list)`.
/// The index is assigned by `sys_mutex_create` / `sys_semaphore_create`, so it is
/// stable for the life of the address space and needs no object pointer here.
pub type ResKey = (usize, usize);

/// A task node: `(pid, tid)`.
pub type TaskKey = (usize, usize);

/// Is detection armed? Everything here is a no-op while it is not, so the other
/// ch8 tests keep their original behaviour bit for bit.
static mut ENABLED: bool = false;

/// Arm or disarm. Nothing is cached, so disarming cannot leave stale edges behind.
pub fn set_enabled(yes: bool) {
    // SAFETY: single hart, and the kernel cannot trap back into itself while a
    // syscall runs — the same rule that makes `UPSafeCell` sound in `sync/up.rs`.
    unsafe { ENABLED = yes }
}

/// Detection armed?
#[allow(dead_code)]
pub fn enabled() -> bool {
    // SAFETY: as above.
    unsafe { ENABLED }
}

/// One entry of the derived graph.
struct Graph {
    /// For each resource: how many units are free right now (`count > 0` case).
    /// Mutexes contribute 0 or 1.
    available: Vec<(ResKey, isize)>,
    /// For each resource: the tasks that stand between it and its next grant,
    /// i.e. its holders minus those already parked in its own queue.
    owners: Vec<(ResKey, Vec<TaskKey>)>,
    /// For each task: the resource it is parked on, if any.
    blocked_on: Vec<(TaskKey, ResKey)>,
    /// For each task: how many units it holds per resource.
    held: Vec<(TaskKey, Vec<(ResKey, usize)>)>,
}

/// A refusal waiting for the punished thread to resume in its own syscall.
///
/// `ahead` records how many waiters stood *in front of* the victim when it was
/// pulled out of the queue. That number is what tells a re-asking victim whether its
/// denial actually opened the ring: with somebody ahead, that head waiter can now be
/// served by the unit we just freed, so the cycle is broken and asking again must not
/// refuse anyone further. From an empty queue, refusing again is the only progress.
struct Abort {
    task: TaskKey,
    key: ResKey,
    code: isize,
    ahead: usize,
}

lazy_static! {
    /// Refusals queued by victim selection. Small and short-lived: an entry is
    /// dropped as soon as that task re-enters the syscall it was parked in.
    static ref ABORTED: super::UPSafeCell<Vec<Abort>> =
        unsafe { super::UPSafeCell::new(Vec::new()) };
}

// ---------------------------------------------------------------------------
// graph construction
// ---------------------------------------------------------------------------

/// The `(pid, tid)` of a task, used both as the walk's target and as the key a
/// queued refusal is filed under. `None` for a task that already gave up its user
/// resources, i.e. one that is exiting or exited.
fn self_key(task: &Arc<TaskControlBlock>) -> Option<TaskKey> {
    let pid = task.process.upgrade()?.getpid();
    let tid = task.inner_exclusive_access().res.as_ref()?.tid;
    Some((pid, tid))
}

/// Which semaphore-list slot does this resource key name? `None` for mutex keys.
fn semaphore_slot(
    inner: &crate::task::ProcessControlBlockInner,
    key: ResKey,
) -> Option<&Option<Arc<Semaphore>>> {
    if key.1 < SEM_KEY_BASE {
        return None;
    }
    inner.semaphore_list.get(key.1 - SEM_KEY_BASE)
}

/// Which mutex-list slot does this resource key name? `None` for semaphore keys.
fn mutex_slot(
    inner: &crate::task::ProcessControlBlockInner,
    key: ResKey,
) -> Option<&Option<Arc<dyn Mutex>>> {
    if key.1 < MUTEX_KEY_BASE || key.1 >= SEM_KEY_BASE {
        return None;
    }
    inner.mutex_list.get(key.1 - MUTEX_KEY_BASE)
}

/// Look up a task of this address space by tid.
fn task_at(process: &Arc<crate::task::ProcessControlBlock>, tid: usize) -> Option<Arc<TaskControlBlock>> {
    let inner = process.inner_exclusive_access();
    inner.tasks.get(tid)?.as_ref().map(Arc::clone)
}

/// Rebuild the graph for the address space that owns `key`.
///
/// Only one process is ever inspected: the ch8 tests create all their resources in
/// the calling process, and a cross-process resource handle cannot exist because
/// the id lists are per-address-space. Restricting the walk to one pid also bounds
/// it by that process' task count, which is what keeps the check cheap enough to
/// run inside the 30s budget even for `mpsc_sem`-sized thread sets.
fn build_graph(pid: usize) -> Graph {
    let mut g = Graph {
        available: Vec::new(),
        owners: Vec::new(),
        queue_depth: Vec::new(),
        blocked_on: Vec::new(),
        held: Vec::new(),
    };
    let process = match pid2process(pid) {
        Some(p) => p,
        None => return g,
    };

    // Pass 1: collect every task's identity and its own edges out of the TCB.
    // `tasks` is indexed by tid, and a slot is `None` once that thread exited, so
    // a dead holder simply never appears here — that is the whole of our "forget a
    // dying task" logic.
    let tids: Vec<usize> = {
        let inner = process.inner_exclusive_access();
        (0..inner.tasks.len())
            .filter(|i| inner.tasks[*i].is_some())
            .collect()
    };
    for tid in tids {
        let task = match task_at(&process, tid) {
            Some(t) => t,
            None => continue,
        };
        let key = (pid, tid);
        let (blocked_in, grants) = {
            let tcb = task.inner_exclusive_access();
            let res = match tcb.res.as_ref() {
                Some(r) => r,
                None => continue,
            };
            (res.blocked_in, res.grants.clone())
        };
        if !grants.is_empty() {
            g.held.push((key, grants));
        }
        if let Some(res) = blocked_in {
            g.blocked_on.push((key, res));
        }
    }

    // Pass 2: resources. Mutexes answer for themselves; semaphores need the grant
    // counts gathered above, because the object only stores a net counter.
    let mutexes: Vec<Option<Arc<dyn Mutex>>> = {
        let inner = process.inner_exclusive_access();
        inner.mutex_list.clone()
    };
    for (idx, slot) in mutexes.iter().enumerate() {
        let m = match slot.as_ref() {
            Some(m) => Arc::clone(m),
            None => continue,
        };
        // Mutex resource keys are offset so they cannot collide with semaphore
        // keys: both id lists start at 0 but they name different objects.
        let key = mutex_key(pid, idx);
        // A spin mutex parks nobody, so it is not a node at all.
        let blocking = match m.as_blocking() {
            Some(b) => b,
            None => continue,
        };
        let (locked, owner_tid, queued): (bool, Option<usize>, usize) = {
            let inner = blocking.inner_ref();
            (inner.locked, inner.owner_tid(), inner.wait_queue.len())
        };
        // Available capacity: 1 unless somebody owns it.
        g.available.push((key, if locked { 0 } else { 1 }));
        g.queue_depth.push((key, queued));
        // Owners: the holder, unless it is itself parked here (it cannot be, but the
        // subtraction is what makes the rule uniform with semaphores).
        let mut owners: Vec<TaskKey> = Vec::new();
        if let Some(tid) = owner_tid {
            if !g.parked_on(key, tid) {
                owners.push((pid, tid));
            }
        }
        g.owners.push((key, owners));
    }

    let sems: Vec<Option<Arc<Semaphore>>> = {
        let inner = process.inner_exclusive_access();
        inner.semaphore_list.clone()
    };
    for (idx, slot) in sems.iter().enumerate() {
        let sem = match slot.as_ref() {
            Some(s) => Arc::clone(s),
            None => continue,
        };
        let key = sem_key(pid, idx);
        let (count, queued): (isize, usize) = {
            let inner = sem.inner_ref();
            (inner.count, inner.wait_queue.len())
        };
        // Free capacity first: with count > 0 nobody is waiting on this object, so
        // it closes no chain. That is the sem2 lesson.
        g.available.push((key, count));
        g.queue_depth.push((key, queued));
        // Outstanding units are `initial - count`. Which tasks hold them is read
        // off the grant records collected in pass 1, so attribution is exact rather
        // than guessed.
        let mut owners: Vec<TaskKey> = Vec::new();
        for (tkey, grants) in g.held.iter() {
            if let Some(n) = grants.iter().find(|(k, _)| *k == key).map(|(_, n)| *n) {
                if n == 0 {
                    continue;
                }
                // A task parked on this very semaphore holds no unit of it: it is
                // represented by the queue below, not here.
                if g.parked_on(key, tkey.1) {
                    continue;
                }
                owners.push(*tkey);
            }
        }
        g.owners.push((key, owners));
    }
    g
}

impl Graph {
    /// Is the task with this tid currently parked on `res`?
    fn parked_on(&self, res: ResKey, tid: usize) -> bool {
        self.blocked_on
            .iter()
            .any(|(t, r)| *r == res && t.1 == tid)
    }
}

/// Semaphore resource keys are offset so they cannot collide with mutex keys in the
/// flat lists above: both id lists start at 0, but they name different objects.
const SEM_KEY_BASE: usize = 1 << 40;
/// Same for blocking mutexes.
const MUTEX_KEY_BASE: usize = 1 << 20;

/// Tag a mutex slot index as a resource key.
pub fn mutex_key(pid: usize, slot: usize) -> ResKey {
    (pid, slot + MUTEX_KEY_BASE)
}

/// Tag a semaphore slot index as a resource key.
pub fn sem_key(pid: usize, slot: usize) -> ResKey {
    (pid, slot + SEM_KEY_BASE)
}

// ---------------------------------------------------------------------------
// the walk
// ---------------------------------------------------------------------------

impl Graph {
    fn owners_of(&self, res: ResKey) -> &[TaskKey] {
        self.owners
            .iter()
            .find(|(k, _)| *k == res)
            .map(|(_, v)| v.as_slice())
            .unwrap_or(&[])
    }
    fn available_of(&self, res: ResKey) -> isize {
        self.available
            .iter()
            .find(|(k, _)| *k == res)
            .map(|(_, n)| *n)
            .unwrap_or(0)
    }
    fn queue_depth_of(&self, res: ResKey) -> usize {
        self.queue_depth
            .iter()
            .find(|(k, _)| *k == res)
            .map(|(_, n)| *n)
            .unwrap_or(0)
    }
    fn blocked_on(&self, task: TaskKey) -> Option<ResKey> {
        self.blocked_on
            .iter()
            .find(|(t, _)| *t == task)
            .map(|(_, r)| *r)
    }
}

/// Can `start` (a resource) lead back to `target` (a task)?
///
/// Alternating depth-first search over resource -> holder -> its blocker -> ... .
/// `seen` prevents exponential blow-up and also stops us at a resource we already
/// expanded, which bounds the walk by the number of objects in the address space.
fn reaches(graph: &Graph, start: ResKey, target: TaskKey, seen: &mut Vec<ResKey>) -> bool {
    if seen.contains(&start) {
        return false;
    }
    seen.push(start);
    // Capacity still free: the next acquirer is served immediately, no chain.
    if graph.available_of(start) > 0 {
        return false;
    }
    for owner in graph.owners_of(start).to_vec() {
        if owner == target {
            return true;
        }
        if let Some(next) = graph.blocked_on(owner) {
            if reaches(graph, next, target, seen) {
                return true;
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// public entry points
// ---------------------------------------------------------------------------

/// Would parking the current task on `key` close a wait-for cycle?
///
/// The caller's own edge is not in the graph yet (it has not blocked), so "would this
/// close a cycle" is exactly "does `key` lead back to me?". When it does, somebody on
/// the ring has to be refused — and it is usually *not* the asker, see [`pick_victim`].
/// This function therefore returns the chosen victim rather than a yes/no answer:
/// `Some(me)` means "the primitive must fail this acquire itself", `Some(other)`
/// means "`refuse_queued` the other one and park normally".
pub fn check(key: ResKey) -> Option<TaskKey> {
    if !enabled() {
        return None;
    }
    let me = match current_key() {
        Some(k) => k,
        None => return None,
    };
    let graph = build_graph(me.0);
    let mut seen = Vec::new();
    if !reaches(&graph, key, me, &mut seen) {
        return None;
    }
    Some(pick_victim(&graph, &seen, me, key))
}

/// Choose which cycle member gets the error code.
///
/// Candidates are the members already parked mid-acquire on one of the resources the
/// walk touched, plus the caller (whose pending request is `key`). A denial only
/// helps if the ring actually opens afterwards, and on a FIFO acquire queue it opens
/// exactly when the victim was **not** the first waiter: then the head-of-queue task
/// stays queued for the same unit the victim just gave up, and the holder that feeds
/// it is still running. So we prefer a candidate whose queue has somebody ahead of it
/// and break ties by smallest `(pid, tid)` for reproducibility; only when every
/// candidate would be served next anyway do we refuse the caller.
fn pick_victim(graph: &Graph, seen: &[ResKey], me: TaskKey, my_key: ResKey) -> TaskKey {
    // (waiters ahead of the victim, victim). Fewer is better: refusing a task that
    // stands behind others leaves a satisfiable head of queue, whereas refusing the
    // head just moves the blockage one step along. Smallest tid breaks ties.
    let mut candidates: Vec<(usize, TaskKey)> = Vec::new();
    for res in seen.iter() {
        let depth = graph.queue_depth_of(*res);
        for (task, r) in graph.blocked_on.iter() {
            if r == res && task != &me && !candidates.iter().any(|(_, t)| t == task) {
                candidates.push((depth, *task));
            }
        }
    }
    // The caller is a candidate too: its queue depth counts the waiters that would
    // sit ahead of it once it joins.
    if !candidates.iter().any(|(_, t)| *t == me) {
        candidates.push((graph.queue_depth_of(my_key), me));
    }
    candidates.sort_unstable();
    candidates[0].1
}

/// Refuse `victim`, which is parked in `key`'s queue.
///
/// The entry has to come out of the queue, otherwise the next `up`/`unlock` would
/// hand a unit to a thread that has been told to fail and the grant counts would
/// drift apart from the object's counter. [`unqueue`] drops it, then the task resumes
/// inside its own syscall and reports the code through [`take_abort_for`].
pub fn refuse_queued(victim: TaskKey, key: ResKey) {
    let process = match pid2process(victim.0) {
        Some(p) => p,
        None => return,
    };
    let task = match task_at(&process, victim.1) {
        Some(t) => t,
        None => return,
    };
    {
        let mut tcb = task.inner_exclusive_access();
        if let Some(res) = tcb.res.as_mut() {
            res.blocked_in = None;
        }
    }
    unqueue(&task, key);
    ABORTED.exclusive_access().push(Abort {
        task: victim,
        key,
        code: DEADLOCK,
    });
    wakeup_task(task);
}

/// Hand the refusal to the chosen cycle member.
///
/// Returns `true` when somebody *else* was refused — the caller then joins the queue
/// as normal, because the edge the graph keeps is the caller's. Returns `false` when
/// the victim is the caller itself: there is nobody to pull out of the queue, and the
/// primitive must fail its own acquire instead (a queued waiter implies the object
/// owes that waiter a unit, so parking here would desynchronise counter and queue).
pub fn refuse_if_not_me(victim: TaskKey, key: ResKey) -> bool {
    let me = match current_key() {
        Some(k) => k,
        // No usable identity means we cannot be the victim; refuse whoever was named
        // rather than letting the ring stay shut.
        None => {
            refuse_queued(victim, key);
            return true;
        }
    };
    if victim == me {
        return false;
    }
    refuse_queued(victim, key);
    true
}

/// Remove a task from an object's wait queue without granting it anything.
///
/// Queues are FIFO, but a refused victim can sit anywhere in one, so removal is by
/// identity (`Arc::ptr_eq`) rather than position — the same reason grant counts are
/// carried by the task instead of indexed by tid here.
fn unqueue(task: &Arc<TaskControlBlock>, key: ResKey) {
    let process = match pid2process(key.0) {
        Some(p) => p,
        None => return,
    };
    let (sem, mutex) = {
        let inner = process.inner_exclusive_access();
        (
            semaphore_slot(&inner, key).and_then(|s| s.as_ref()).cloned(),
            mutex_slot(&inner, key).and_then(|m| m.as_ref()).cloned(),
        )
    };
    if let Some(sem) = sem {
        sem.remove_waiter(task);
        return;
    }
    if let Some(m) = mutex {
        if let Some(b) = m.as_blocking() {
            b.remove_waiter(task);
        }
    }
}

/// Pick up a refusal filed against the current task for `key`, if any.
///
/// Called at the top of the blocking acquire paths, so a thread chosen as a cycle
/// victim while asleep reports `-0xdead` from its own syscall instead of being handed
/// the resource it was punished for.
pub fn take_abort_for(key: ResKey) -> Option<isize> {
    if !enabled() {
        return None;
    }
    let me = match current_key() {
        Some(k) => k,
        None => return None,
    };
    let mut aborted = ABORTED.exclusive_access();
    let pos = aborted.iter().position(|a| a.task == me && a.key == key)?;
    Some(aborted.remove(pos).code)
}

/// Forget everything about a task that is exiting. Grant counts and the parked
/// marker live in its own TCB, so dropping the TCB is enough; this only clears
/// queued refusals that could otherwise be replayed onto a recycled tid.
pub fn forget_task(task: &Arc<TaskControlBlock>) {
    if !enabled() {
        return;
    }
    if let Some(me) = self_key(task) {
        ABORTED.exclusive_access().retain(|a| a.task != me);
    }
}

// ---------------------------------------------------------------------------
// bookkeeping called by the primitives
//
// All of it writes into the *calling* task's own TCB (or, for a hand-off, into the
// receiver's), which is why there is no registry to keep in sync: an object never
// has to remember who it gave a unit to, because that task carries the number.
// Every function returns immediately while detection is disarmed, so the other ch8
// tests pay nothing beyond one branch.
// ---------------------------------------------------------------------------

/// The `(pid, tid)` of the current task.
fn current_key() -> Option<TaskKey> {
    current_task().and_then(|t| self_key(&t))
}

/// Run `f` on the current task's `TaskUserRes`.
fn with_me(mut f: impl FnMut(&mut crate::task::TaskUserRes)) {
    if !enabled() {
        return;
    }
    if let Some(task) = current_task() {
        let mut tcb = task.inner_exclusive_access();
        if let Some(res) = tcb.res.as_mut() {
            f(res);
        }
    }
}

/// Run `f` on some other task's `TaskUserRes` (used for a grant handed over by an
/// unlock/up, where the receiver is not the running task).
fn with_task(task: &Arc<TaskControlBlock>, mut f: impl FnMut(&mut crate::task::TaskUserRes)) {
    if !enabled() {
        return;
    }
    let mut tcb = task.inner_exclusive_access();
    if let Some(res) = tcb.res.as_mut() {
        f(res);
    }
}

/// Add one outstanding grant of `key` to the current task.
pub fn grant_me(key: ResKey) {
    with_me(|res| add_grant(res, key));
}

/// Add one outstanding grant of `key` to `task`, chosen by identity rather than by
/// tid so a recycled tid cannot pick up a previous tenant's count.
pub fn grant_to_task(task: &Arc<TaskControlBlock>, key: ResKey) {
    with_task(task, |res| add_grant(res, key));
}

fn add_grant(res: &mut crate::task::TaskUserRes, key: ResKey) {
    match res.grants.iter_mut().find(|(k, _)| *k == key) {
        Some(entry) => entry.1 += 1,
        None => res.grants.push((key, 1)),
    }
}

/// The current task parks on `key`: record the edge before it blocks.
pub fn park_me(key: ResKey) {
    with_me(|res| res.blocked_in = Some(key));
}

/// The current task left the queue of `key` and was granted the resource by
/// whoever released it — that release already recorded the grant on its behalf, so
/// all that is left here is to drop the parked marker.
pub fn clear_parked_me(key: ResKey) {
    with_me(|res| {
        if res.blocked_in == Some(key) {
            res.blocked_in = None;
        }
    });
}

/// Give back **one** unit of every resource named in `key`s held by the caller.
///
/// A mutex unlock releases its single lock; `semaphore_up` releases exactly one
/// unit, so the two paths differ only in how many keys they pass.
pub fn release_me(keys: &[ResKey]) {
    if !enabled() {
        return;
    }
    with_me(|res| {
        for key in keys {
            take_one_grant(res, *key);
        }
    });
}

/// Hand the resource of `key` from the releasing task straight to `receiver`: the
/// receiver stops waiting and gains one grant.
pub fn transfer(key: ResKey, receiver: &Arc<TaskControlBlock>) {
    with_task(receiver, |res| {
        if res.blocked_in == Some(key) {
            res.blocked_in = None;
        }
        add_grant(res, key);
    });
}

fn take_one_grant(res: &mut crate::task::TaskUserRes, key: ResKey) {
    if let Some(pos) = res.grants.iter().position(|(k, _)| *k == key) {
        let left = res.grants[pos].1 - 1;
        if left == 0 {
            res.grants.remove(pos);
        } else {
            res.grants[pos].1 = left;
        }
    }
}

