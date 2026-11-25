//! Deadlock detection: a wait-for *reduction*, rebuilt from scratch on demand.
//!
//! This module keeps **no edges of its own**. Whenever an acquire would have to park
//! its caller, [`would_deadlock`] rebuilds the state of every blocking mutex and
//! counting semaphore in the caller's address space and answers one question: *can
//! the unit this task is queued behind ever come back to it?* If the answer is no,
//! the request is refused with `-0xdead` before the task joins any queue, so the
//! refusing path never touches an object's counter.
//!
//! Because the graph is derived rather than maintained, there is nothing to
//! invalidate when a grant is handed over, a queue is served, or a thread dies
//! mid-hold — the three places where an incrementally kept wait-for graph goes stale
//! and starts inventing cycles.
//!
//! # Where each piece of state comes from
//!
//! * **free units**: the object itself. A [`Semaphore`] answers with `count` clamped
//!   at zero (a negative `count` is not spare capacity, it is an IOU to its queue); a
//!   [`MutexBlocking`] answers `1` while unlocked.
//! * **acquire queues**: the object itself, front to back. Order matters — see rule 2
//!   below — so the detector reads the queue, not just its length.
//! * **who holds what**: each task's own `grants` list in its TCB, written by the
//!   primitives themselves, plus `blocked_in` for the object it is parked on.
//!
//! So the only state the detector trusts is the primitives' own counters/queues and
//! per-task grant counts. There is no resource table, no registry keyed by object
//! pointer, and no `Arc` bookkeeping — deliberately unlike the `sync/resource.rs`
//! style of keeping a parallel list of "resources" that has to be remembered.
//!
//! # The question a cycle search gets wrong (`ch8_deadlock_sem2`)
//!
//! The textbook check — resource → holder → that holder's blocker → ... — reports
//! sem2 as deadlocked. Two forks, four philosophers, and each fork semaphore is
//! created with **two** units: `t1` holds a unit of fork 2 and asks for fork 1, `t3`
//! holds a unit of fork 1 and asks for fork 2, so `t1 -> fork1 -> t3 -> fork2 -> t1`
//! is a graph cycle. It is not a deadlock: `t2` holds the second unit of fork 1 and
//! `t4` the second unit of fork 2, and neither parks on anything (`REQUEST` is `None`
//! for both), so each hands its unit back. A cycle is only fatal when *every* unit of the
//! object at each step is frozen inside the cycle; a search that follows "who holds
//! it" without asking "how many are still loose" cannot tell the two apart.
//!
//! # The reduction
//!
//! So instead of searching for a cycle this module tries to *discharge* the state,
//! the way the banker's algorithm discharges a claim:
//!
//! 1. A task that is not parked on an object is assumed to run to completion, so
//!    every unit it holds is credited back to that object's pool **before** the first
//!    waiter is considered. This is what makes sem2 satisfiable, and what lets a
//!    thread that merely sleeps on a timer (or, in sem1, the `main` thread holding
//!    all three barrier units) feed the queues behind it.
//! 2. A waiter at FIFO position `p` of an object is reachable once `p + 1` units have
//!    come back to that object: one for each waiter ahead of it, plus its own.
//! 3. Repeat rule 2 over all waiters until the discharged set stops growing. A
//!    discharged waiter contributes its own holdings to the pool.
//! 4. The caller — inserted hypothetically at the tail of the object it is asking
//!    for — is refused exactly when it is *not* in the fixed point.
//!
//! Rule 2 is where the "how many are loose" question from the last section actually
//! gets answered, and it is also what makes the answer independent of how the
//! primitives are shaped: a mutex is just the case `p + 1 == 1` with a pool of 0 or 1.
//!
//! # Why refusing the caller is the whole policy (`ch8_deadlock_sem1`)
//!
//! Detection and refusal are the same act here, which is a deliberate simplification
//! worth stating openly. When the fixed point excludes the caller, *no* member of the
//! ring can advance, so the ring can only open if somebody leaves it — and the caller
//! is the only participant not yet inside a queue. Refusing an already-parked task
//! instead (the "abort the transaction that releases nothing" rule) means pulling it
//! out of a FIFO queue, waking it into a syscall it had already left, and remembering
//! a denial until it picks the error up; that is a second protocol bolted onto the
//! first, and sem1 does not need it: the request that closes the ring there is
//! `t3 -> fork 2`, whose author is the only task still holding a unit the ring needs.
//! Failing it is exactly as good as failing `t3` later, and it is checkable without
//! leaving state behind.
//!
//! Two separate reasons keep the rest of ch8 off this code path, and they are worth
//! telling apart. Structurally: arming is per address space (the `ARMED` list below),
//! so a process that never calls `sys_enable_deadlock_detect` is not consulted at all —
//! that covers `test_condvar`, `mpsc_sem`, `sync_sem` and the `race_adder_*` family
//! regardless of what they wait on. Substantively, for the address spaces that *are*
//! armed: condvars contribute nothing to the reduction (they park a task until somebody
//! signals, not until a resource is granted), and spin mutexes busy-wait without parking
//! anybody, so they are neither nodes nor edges.

use super::{Mutex, Semaphore, UPSafeCell};
use crate::task::{current_task, pid2process, TaskControlBlock};
use alloc::sync::Arc;
use alloc::vec::Vec;
use lazy_static::lazy_static;

/// Error code the ch8 tests expect from an acquire that would deadlock.
pub const DEADLOCK: isize = -0xdead;

/// A resource node: `(pid, index into that process' mutex_list/semaphore_list)`.
/// The index is assigned by `sys_mutex_create` / `sys_semaphore_create`, so it is
/// stable for the life of the address space and needs no object pointer here.
pub type ResKey = (usize, usize);

/// A task node: `(pid, tid)`.
pub type TaskKey = (usize, usize);

// The address spaces that have armed detection.
//
// Per-process rather than one global flag, because `ch8_usertest` forks all 23 cases
// up front and the three deadlock tests never disarm: with a global flag every later
// case (`mpsc_sem`, `sync_sem`, ...) would be held to refusals it never opted into,
// and the reduction does refuse a `down` on a zero-count semaphore whose producer has
// not run yet — legitimate blocking, not a deadlock.
//
// Process ids are allocated monotonically and are not reused inside a session, so an
// entry needs no cleanup when its process exits: that pid is never asked about again,
// and the list stays a few entries long.
lazy_static! {
    static ref ARMED: UPSafeCell<Vec<usize>> = unsafe { UPSafeCell::new(Vec::new()) };
}

/// Arm or disarm detection for the calling address space. Nothing about the objects is
/// cached, so disarming cannot leave stale state behind.
pub fn set_enabled(yes: bool) {
    let Some(pid) = current_pid() else { return };
    let mut armed = ARMED.exclusive_access();
    if yes {
        if !armed.contains(&pid) {
            armed.push(pid);
        }
    } else if let Some(pos) = armed.iter().position(|p| *p == pid) {
        armed.remove(pos);
    }
}

/// Is detection armed for the calling address space? Everything below is a no-op while
/// it is not, so the other ch8 tests keep their original behaviour bit for bit.
fn enabled() -> bool {
    match current_pid() {
        Some(pid) => ARMED.exclusive_access().contains(&pid),
        None => false,
    }
}

/// The calling process' id, read *without* borrowing its inner state: `enabled()` runs
/// inside the primitives, where a `RefMut` on the address space may already be live.
fn current_pid() -> Option<usize> {
    Some(current_task()?.process.upgrade()?.getpid())
}

/// The derived state of one address space, in exactly the shape [`served`] reads.
struct Graph {
    /// Units of each object not handed out right now. Mutexes contribute 0 or 1.
    free: Vec<(ResKey, isize)>,
    /// Each object's acquire queue, front to back.
    queues: Vec<(ResKey, Vec<TaskKey>)>,
    /// For each task: the object it is parked on, if any.
    blocked_on: Vec<(TaskKey, ResKey)>,
    /// For each task: how many units it holds per object.
    held: Vec<(TaskKey, Vec<(ResKey, usize)>)>,
}

impl Graph {
    fn queue_of(&self, res: ResKey) -> &[TaskKey] {
        self.queues
            .iter()
            .find(|(k, _)| *k == res)
            .map(|(_, q)| q.as_slice())
            .unwrap_or(&[])
    }
    /// The object this task is parked on, or `None` when it is running/ready.
    fn parked_on(&self, task: TaskKey) -> Option<ResKey> {
        self.blocked_on
            .iter()
            .find(|(t, _)| *t == task)
            .map(|(_, r)| *r)
    }
    fn held_by(&self, task: TaskKey) -> &[(ResKey, usize)] {
        self.held
            .iter()
            .find(|(t, _)| *t == task)
            .map(|(_, g)| g.as_slice())
            .unwrap_or(&[])
    }
    /// Every task that carries state. A task with neither a grant nor a park cannot
    /// move the reduction either way, so leaving it out of the walk is exact.
    fn tasks(&self) -> Vec<TaskKey> {
        let mut out: Vec<TaskKey> = self.held.iter().map(|(t, _)| *t).collect();
        for (task, _) in self.blocked_on.iter() {
            if !out.contains(task) {
                out.push(*task);
            }
        }
        out
    }
}

/// Credit `task`'s holdings onto the pool of returned units.
///
/// Called when a task is discharged, and once more for every task that was never
/// parked to begin with — that second use is rule 1 of the reduction, and it is what
/// a running holder contributes to the threads waiting on its units.
fn credit(pool: &mut Vec<(ResKey, isize)>, grants: &[(ResKey, usize)]) {
    for (res, n) in grants {
        match pool.iter_mut().find(|(k, _)| *k == *res) {
            Some(entry) => entry.1 += *n as isize,
            None => pool.push((*res, *n as isize)),
        }
    }
}

/// Units of `res` that have come back, either still loose in the object or credited
/// to it by a task the reduction already discharged.
fn pool_of(pool: &[(ResKey, isize)], res: ResKey) -> isize {
    pool.iter()
        .find(|(k, _)| *k == res)
        .map(|(_, n)| *n)
        .unwrap_or(0)
}

/// Can the caller's hypothetical request for `key` eventually be served?
///
/// `me` is deliberately absent from the graph (it has not blocked yet), so the
/// request is appended to `key`'s queue here at the tail — acquires are FIFO — and
/// `me` is excluded from the "already finished" set even though it is running: it is
/// the one task whose units may *not* be counted before its own request is served.
fn served(graph: &Graph, me: TaskKey, key: ResKey) -> bool {
    let mut pool: Vec<(ResKey, isize)> = graph.free.clone();
    let mut done: Vec<TaskKey> = Vec::new();
    // (waiter, object it waits on, its position in that object's queue)
    let mut need: Vec<(TaskKey, ResKey, usize)> = Vec::new();

    for task in graph.tasks() {
        if task != me && graph.parked_on(task).is_none() {
            done.push(task);
        }
    }
    for task in done.clone() {
        credit(&mut pool, graph.held_by(task));
    }
    for (res, queue) in graph.queues.iter() {
        for (pos, task) in queue.iter().enumerate() {
            need.push((*task, *res, pos));
        }
    }
    need.push((me, key, graph.queue_of(key).len()));

    loop {
        let mut changed = false;
        for (task, res, pos) in need.clone() {
            if done.contains(&task) {
                continue;
            }
            // `pos + 1` because the waiters ahead of it take one unit each before its
            // own turn arrives; see rule 2 in the module header.
            if pool_of(&pool, res) >= (pos + 1) as isize {
                done.push(task);
                credit(&mut pool, graph.held_by(task));
                changed = true;
            }
        }
        if !changed {
            return done.contains(&me);
        }
    }
}

// ---------------------------------------------------------------------------
// graph construction
// ---------------------------------------------------------------------------

/// The `(pid, tid)` of a task, used as the walk's node identity. `None` for a task
/// that already gave up its user resources, i.e. one that is exiting or exited.
fn self_key(task: &Arc<TaskControlBlock>) -> Option<TaskKey> {
    let pid = task.process.upgrade()?.getpid();
    let tid = task.inner_exclusive_access().res.as_ref()?.tid;
    Some((pid, tid))
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
/// it by that process' task count, which is what keeps the reduction cheap enough to
/// run inside the 30s budget even for `mpsc_sem`-sized thread sets.
fn build_graph(pid: usize) -> Graph {
    let mut g = Graph {
        free: Vec::new(),
        queues: Vec::new(),
        blocked_on: Vec::new(),
        held: Vec::new(),
    };
    let process = match pid2process(pid) {
        Some(p) => p,
        None => return g,
    };

    // Pass 1: collect every task's identity and its own edges out of the TCB.
    // `tasks` is indexed by tid, and a slot is `None` once that thread exited, so
    // a dead holder simply never appears here. Its units are gone from the pool as
    // well, which is the conservative direction: an exited thread that never
    // released anything cannot be credited with feeding the queues.
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

    // Pass 2: the objects. Each one reports its own loose capacity and its queue;
    // attribution of the handed-out units is pass 1's job, because the objects
    // themselves only keep a net counter.
    let mutexes: Vec<Option<Arc<dyn Mutex>>> = {
        let inner = process.inner_exclusive_access();
        inner.mutex_list.clone()
    };
    for (idx, slot) in mutexes.iter().enumerate() {
        let m = match slot.as_ref() {
            Some(m) => Arc::clone(m),
            None => continue,
        };
        // A spin mutex parks nobody, so it is not a node at all.
        let blocking = match m.as_blocking() {
            Some(b) => b,
            None => continue,
        };
        let (locked, queue) = {
            let inner = blocking.inner_ref();
            (inner.locked, inner.queued_tids())
        };
        // Mutex resource keys are offset so they cannot collide with semaphore
        // keys: both id lists start at 0 but they name different objects.
        let key = mutex_key(pid, idx);
        g.free.push((key, if locked { 0 } else { 1 }));
        g.queues.push((key, queue.into_iter().map(|tid| (pid, tid)).collect()));
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
        let (count, queue) = {
            let inner = sem.inner_ref();
            (inner.count, Semaphore::queued_tids(&inner.wait_queue))
        };
        let key = sem_key(pid, idx);
        // A negative counter is not spare capacity, it is the number of units this
        // object still owes its queue — which the queue itself already accounts for.
        g.free.push((key, count.max(0)));
        g.queues.push((key, queue.into_iter().map(|tid| (pid, tid)).collect()));
    }
    g
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
// public entry points
// ---------------------------------------------------------------------------

/// Must the current task's request for `key` be refused?
///
/// `true` means: even if every task that is not parked runs to completion and hands
/// all of its units back, the unit this request would join the queue for never
/// arrives. The primitive then returns [`DEADLOCK`] itself, without parking anybody or
/// editing the object's counter — see the policy discussion in the module header.
pub fn would_deadlock(key: ResKey) -> bool {
    if !enabled() {
        return false;
    }
    let me = match current_key() {
        Some(k) => k,
        // No usable identity: assume no deadlock rather than invent a refusal.
        None => return false,
    };
    let graph = build_graph(me.0);
    !served(&graph, me, key)
}

/// The `(pid, tid)` of the current task.
fn current_key() -> Option<TaskKey> {
    current_task().and_then(|t| self_key(&t))
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
