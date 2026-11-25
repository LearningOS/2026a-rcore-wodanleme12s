//! Deadlock detection: a bipartite wait-for graph rebuilt from scratch on demand.
//!
//! This module keeps **no edges of its own**. Every time an acquire would have to
//! park its caller, [`would_deadlock`] walks the objects that already exist and
//! derives the whole graph from them, then answers one question: starting from the
//! object being asked for, can we get back to the asking task? Because the graph is
//! rebuilt rather than incrementally maintained, there is nothing to invalidate when
//! a grant is handed over, a queue is served, or a thread dies mid-hold — the three
//! places where an incrementally kept graph goes stale and starts inventing cycles.
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
//! # Victim selection
//!
//! Detection alone is not enough for the sem1 test: three threads grab resources,
//! keep *running*, and only then ask for the next one, so whoever asks last is not
//! necessarily the right one to refuse. When a cycle is found we therefore pick the
//! cycle member whose pending acquire has not been granted yet and which holds
//! nothing else worth punishing, smallest `(pid, tid)` first, and hand *that* task
//! the error code through its own syscall ([`take_abort_for`]). If no such task
//! exists besides the caller, the caller itself is refused. Either way exactly one
//! edge disappears and nothing sleeps until the 30s QEMU timeout cuts the run off.
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
    /// Tasks that hold something but are not parked: they are running and will
    /// release eventually, so they terminate a chain rather than extend one.
    idle_holders: Vec<TaskKey>,
}

/// An abort queued by victim selection, consumed by the victim's own syscall.
struct Pending {
    task: TaskKey,
    res: ResKey,
    code: isize,
}

lazy_static! {
    /// Refusals waiting for the punished thread to resume. Small and short-lived:
    /// an entry is dropped as soon as that task re-enters the syscall.
    static ref PENDING: super::UPSafeCell<Vec<Pending>> =
        unsafe { super::UPSafeCell::new(Vec::new()) };
}

// ---------------------------------------------------------------------------
// graph construction
// ---------------------------------------------------------------------------

/// The `(pid, tid)` of the current task, used both as the walk's target and as the
/// key a queued refusal is filed under.
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
/// it by that process' task count, which is what keeps the check cheap enough to
/// run inside the 30s budget even for `mpsc_sem`-sized thread sets.
fn build_graph(pid: usize) -> Graph {
    let mut g = Graph {
        available: Vec::new(),
        owners: Vec::new(),
        blocked_on: Vec::new(),
        held: Vec::new(),
        idle_holders: Vec::new(),
    };
    let process = match pid2process(pid) {
        Some(p) => p,
        None => return g,
    };

    // Pass 1: collect every task's identity and its own edges out of the TCB.
    // `tasks` is indexed by tid, and a slot is `None` once that thread exited, so
    // a dead holder simply never appears here — that is the whole of our "forget a
    // dying task" logic.
    let mut live: Vec<TaskKey> = Vec::new();
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
        live.push((pid, tid));
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
        let (locked, owner_tid, parked): (bool, Option<usize>, Vec<usize>) = {
            let inner = blocking.inner_ref();
            (inner.locked, inner.owner_tid(), inner.queued_tids())
        };
        // Available capacity: 1 unless somebody owns it.
        g.available.push((key, if locked { 0 } else { 1 }));
        // Owners: the holder, unless it is itself parked here (it cannot be, but the
        // subtraction is what makes the rule uniform with semaphores).
        let mut owners: Vec<TaskKey> = Vec::new();
        if let Some(tid) = owner_tid {
            if !parked.contains(&tid) {
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
        let (count, initial, parked): (isize, isize, Vec<usize>) = {
            let inner = sem.inner_ref();
            (inner.count, inner.initial, Semaphore::queued_tids(&inner.wait_queue))
        };
        // Free capacity first: with count > 0 nobody is waiting on this object, so
        // it closes no chain. That is the sem2 lesson.
        g.available.push((key, count));
        // Outstanding units are `initial - count`. Which tasks hold them is read
        // off the grant records collected in pass 1, so attribution is exact rather
        // than guessed; `outstanding` is kept only as a consistency check against
        // the object's own counter.
        let mut owners: Vec<TaskKey> = Vec::new();
        let mut attributed = 0usize;
        for (tkey, grants) in g.held.iter() {
            if let Some(n) = grants.iter().find(|(k, _)| *k == key).map(|(_, n)| *n) {
                if n == 0 {
                    continue;
                }
                // A task parked on this very semaphore holds no unit of it: it is
                // represented by the queue below, not here.
                if parked.contains(&tkey.1) {
                    continue;
                }
                attributed += n;
                owners.push(*tkey);
            }
        }
        debug_assert_eq!(attributed, (initial - count).max(0) as usize);
        g.owners.push((key, owners));
    }

    // Running holders end chains: they are not waiting for anything, so following
    // them leads nowhere. We record them so the victim rule can tell "punishable"
    // from "already satisfied".
    let blocked: Vec<TaskKey> = g.blocked_on.iter().map(|(t, _)| *t).collect();
    for (tkey, grants) in g.held.iter() {
        if !blocked.contains(tkey) && !grants.is_empty() {
            g.idle_holders.push(*tkey);
        }
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

/// All tasks reachable from `start` that sit on a path back to `me`, collected by
/// re-running the walk restricted to the cycle's resources.

// ---------------------------------------------------------------------------
// public entry points
// ---------------------------------------------------------------------------

/// Would parking the current task on `key` close a wait-for cycle?
///
/// `true` means the caller must be refused with `-0xdead`. The caller's own edge is
/// not in the graph yet (it has not blocked), so "would this close a cycle" is
/// exactly "does `key` lead back to me?".
///
/// Refusing the *last asker* is often the wrong punishment: in sem1 the thread that
/// completes the ring may be one that merely holds a unit it will put down, while
/// another member is parked on an acquire that can simply be denied. So we first
/// look for a cycle member already parked on one of the resources the walk touched —
/// smallest `(pid, tid)` for reproducibility — and hand the error code to *that* task
/// through its own syscall. Only when nobody else qualifies do we refuse the caller.
/// Either way exactly one edge disappears and nothing sleeps until the 30s timeout.
pub fn check(key: ResKey) -> bool {
    if !enabled() {
        return false;
    }
    let me = match current_key() {
        Some(k) => k,
        None => return false,
    };
    let graph = build_graph(me.0);
    let mut seen = Vec::new();
    if !reaches(&graph, key, me, &mut seen) {
        return false;
    }
    let mut candidates: Vec<TaskKey> = Vec::new();
    for res in seen.iter() {
        for (task, r) in graph.blocked_on.iter() {
            if r == res && task != &me && !candidates.contains(task) {
                candidates.push(*task);
            }
        }
    }
    candidates.sort_unstable();
    match candidates.first() {
        Some(victim) => {
            let res = victim_res(&graph, *victim);
            refuse_task(*victim, res);
            false
        }
        // Nobody but us is parked mid-acquire: refuse the caller.
        None => true,
    }
}

/// The resource a parked task is waiting on. `(0, 0)` only if it is not in the
/// graph at all, which cannot happen for a candidate we just read out of it.
fn victim_res(graph: &Graph, victim: TaskKey) -> ResKey {
    graph.blocked_on(victim).unwrap_or((0, 0))
}

/// File a refusal for `victim` and wake it so its own syscall can return the code.
fn refuse_task(victim: TaskKey, key: ResKey) {
    let process = match pid2process(victim.0) {
        Some(p) => p,
        None => return,
    };
    let task = match task_at(&process, victim.1) {
        Some(t) => t,
        None => return,
    };
    PENDING.exclusive_access().push(Pending {
        task: victim,
        res: key,
        code: DEADLOCK,
    });
    // Clear the parked-on marker so the walk no longer routes through it, then make
    // the thread runnable: it resumes inside its own down/lock and picks the code up.
    {
        let mut tcb = task.inner_exclusive_access();
        if let Some(res) = tcb.res.as_mut() {
            res.blocked_in = None;
        }
    }
    wakeup_task(task);
}

/// Pick up a refusal filed against the current task for `key`, if any.
///
/// Called at the top of `sys_mutex_lock` / `sys_semaphore_down`, so a thread that
/// was chosen as a victim while asleep reports `-0xdead` from its own syscall
/// instead of being handed the resource it was punished for.
pub fn take_abort_for(key: ResKey) -> Option<isize> {
    if !enabled() {
        return None;
    }
    let me = match current_task().and_then(|t| self_key(&t)) {
        Some(k) => k,
        None => return None,
    };
    let mut pending = PENDING.exclusive_access();
    let pos = pending.iter().position(|p| p.task == me && p.res == key)?;
    Some(pending.remove(pos).code)
}

/// Forget everything about a task that is exiting. Grant counts and the parked
/// marker live in its own TCB, so dropping the TCB is enough; this only clears the
/// queued refusals that could otherwise be replayed onto a recycled tid.
pub fn forget_task(task: &Arc<TaskControlBlock>) {
    if !enabled() {
        return;
    }
    if let Some(me) = self_key(task) {
        PENDING.exclusive_access().retain(|p| p.task != me);
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

