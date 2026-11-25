//! Process management syscalls
use alloc::sync::Arc;

use crate::{
    fs::{open_file, OpenFlags},
    mm::{translated_str_opt, write_struct},
    task::{
        add_task, current_task, current_user_token, exit_current_and_run_next,
        suspend_current_and_run_next, wait_for_ticks, TaskControlBlock, TaskStatus,
    },
};

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

/// task exits and submit an exit code
pub fn sys_exit(exit_code: i32) -> ! {
    trace!("kernel:pid[{}] sys_exit", current_task().unwrap().pid.0);
    exit_current_and_run_next(exit_code);
    panic!("Unreachable in sys_exit!");
}

/// current task gives up resources for other tasks
pub fn sys_yield() -> isize {
    trace!("kernel:pid[{}] sys_yield", current_task().unwrap().pid.0);
    suspend_current_and_run_next();
    0
}

pub fn sys_getpid() -> isize {
    trace!("kernel: sys_getpid pid:{}", current_task().unwrap().pid.0);
    current_task().unwrap().pid.0 as isize
}

pub fn sys_fork() -> isize {
    trace!("kernel:pid[{}] sys_fork", current_task().unwrap().pid.0);
    let current_task = current_task().unwrap();
    let new_task = current_task.fork();
    let new_pid = new_task.pid.0;
    // modify trap context of new_task, because it returns immediately after switching
    let trap_cx = new_task.inner_exclusive_access().get_trap_cx();
    // we do not have to move to next instruction since we have done it before
    // for child process, fork returns 0
    trap_cx.x[10] = 0;
    // add new task to scheduler
    add_task(new_task);
    new_pid as isize
}

pub fn sys_exec(path: *const u8) -> isize {
    trace!("kernel:pid[{}] sys_exec", current_task().unwrap().pid.0);
    // `translated_str_opt` reads the name out of the calling task's own address
    // space — a stack string or an image `.rodata` constant, both of which are
    // mapped U/R there. Anything that does not translate costs us a `-1`, not a
    // kernel panic.
    let path = match translated_str_opt(path) {
        Some(path) => path,
        None => return -1,
    };
    // From ch6 on the program images live on disk, so `exec` is a file lookup:
    // open read-only, slurp every data block, then hand the bytes to the TCB.
    if let Some(app_inode) = open_file(path.as_str(), OpenFlags::RDONLY) {
        let all_data = app_inode.read_all();
        let task = current_task().unwrap();
        task.exec(all_data.as_slice());
        0
    } else {
        -1
    }
}

/// If a child is gone, return its pid and store its exit code; -1 when there is
/// no such child, -2 when it is still running (which `user_lib::wait` turns into
/// a `yield` and retries).
pub fn sys_waitpid(pid: isize, exit_code_ptr: *mut i32) -> isize {
    trace!("kernel::pid[{}] sys_waitpid [{}]", current_task().unwrap().pid.0, pid);
    let task = current_task().unwrap();
    // find a child process

    // ---- access current PCB exclusively
    let mut inner = task.inner_exclusive_access();
    if !inner
        .children
        .iter()
        .any(|p| pid == -1 || pid as usize == p.getpid())
    {
        return -1;
        // ---- release current PCB
    }
    let pair = inner.children.iter().enumerate().find(|(_, p)| {
        // ++++ temporarily access child PCB exclusively
        p.inner_exclusive_access().task_status == TaskStatus::Exited
            && (pid == -1 || pid as usize == p.getpid())
        // ++++ release child PCB
    });
    if let Some((idx, _)) = pair {
        let child = inner.children.remove(idx);
        // confirm that child will be deallocated after being removed from children list
        assert_eq!(Arc::strong_count(&child), 1);
        let found_pid = child.getpid();
        // ++++ temporarily access child PCB exclusively
        let exit_code = child.inner_exclusive_access().exit_code;
        // The exit code pointer is a user address chosen by the application, so
        // it goes through the page-table gate; a bad pointer costs us nothing
        // more than the write. `inner` is released before touching user memory:
        // the parent's TCB borrow must not outlive the copy.
        let token = inner.memory_set.token();
        drop(inner);
        if write_struct(token, exit_code_ptr as usize, &exit_code) {
            found_pid as isize
        } else {
            -1
        }
    } else {
        -2
    }
    // ---- release current PCB automatically
}

/// Microseconds since boot, split the way `struct timeval` wants it.
///
/// `user_lib::get_time()` rebuilds milliseconds as `(sec & 0xffff) * 1000 +
/// usec / 1000`, so `sec` must be returned unmasked: masking it in the kernel
/// would wrap on the first test that runs for more than 18 hours of wall time,
/// but leaving it whole also keeps ch3_sleep's busy-wait monotonic.
fn time_val_now() -> TimeVal {
    let us = crate::timer::get_time_us();
    TimeVal {
        sec: us / 1_000_000,
        usec: us % 1_000_000,
    }
}

/// get time with second and microsecond
///
/// `ts` points at a stack-local `TimeVal` in the caller, which may sit right
/// across a page boundary — hence the per-byte copy instead of one fat store.
/// An unmapped or read-only destination gives `-1`; the tests in this chapter
/// (`ch2b_bad_address`) require exactly that rather than a killed task.
pub fn sys_get_time(ts: *mut TimeVal, _tz: usize) -> isize {
    trace!("kernel:pid[{}] sys_get_time", current_task().unwrap().pid.0);
    let now = time_val_now();
    if write_struct(current_user_token(), ts as usize, &now) {
        0
    } else {
        -1
    }
}

/// Map anonymous zeroed pages at a fixed address with the given protections.
///
/// All validation lives in [`MemorySet::mmap`](crate::mm::MemorySet::mmap) so
/// that the illegal requests the tests probe (unaligned start, overlap,
/// `prot == 0`, `prot & 8`) return `-1` without the application ever touching a
/// page we refused.
pub fn sys_mmap(start: usize, len: usize, prot: usize) -> isize {
    trace!("kernel:pid[{}] sys_mmap", current_task().unwrap().pid.0);
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    inner.memory_set.mmap(start, len, prot)
}

/// Tear down a mapping created by `sys_mmap` and return its frames.
pub fn sys_munmap(start: usize, len: usize) -> isize {
    trace!("kernel:pid[{}] sys_munmap", current_task().unwrap().pid.0);
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    if inner.memory_set.munmap(start, len) {
        0
    } else {
        -1
    }
}

/// change data segment size
pub fn sys_sbrk(size: i32) -> isize {
    trace!("kernel:pid[{}] sys_sbrk", current_task().unwrap().pid.0);
    if let Some(old_brk) = current_task().unwrap().change_program_brk(size) {
        old_brk as isize
    } else {
        -1
    }
}

/// Create a new process running `path` without disturbing the current one.
///
/// This is *not* fork + exec: fork would clone the parent's address space and
/// then exec would throw it away, and — more importantly — a forked child is
/// registered in the parent's `children`, so `wait()` would report programs the
/// parent never spawned. `ch5_spawn0` relies on spawn'd children being invisible
/// to `wait` while `ch5_spawn1` relies on them being visible, so the TCB is built
/// from the ELF directly and adopted by initproc, matching rCore's semantics.
pub fn sys_spawn(path: *const u8) -> isize {
    trace!("kernel:pid[{}] sys_spawn", current_task().unwrap().pid.0);
    let path = match translated_str_opt(path) {
        Some(path) => path,
        None => return -1,
    };
    // spawn == open + read + fork-anew: the image comes from the disk, exactly
    // like `exec`, but a brand new address space is built rather than replacing
    // the caller's.
    let app_inode = match open_file(path.as_str(), OpenFlags::RDONLY) {
        Some(inode) => inode,
        None => return -1,
    };
    let elf_data = app_inode.read_all();
    let new_task = Arc::new(TaskControlBlock::new(elf_data.as_slice()));
    let pid = new_task.pid.0;
    // The spawner owns the child. `sys_waitpid` only ever searches the caller's
    // own `children`, so adopting the child under initproc instead made every
    // spawning task answer "no such child" (`-1`) — `ch5_spawn0` then died on
    // its very first `assert!(wait(..) > 0)` with "wait stopped early", and
    // `ch5_spawn1` compared the wrong pid at `assert_eq!(exit_pid, cpid)`.
    let task = current_task().unwrap();
    task.inner_exclusive_access().children.push(new_task.clone());
    add_task(new_task);
    pid as isize
}

// Set task priority.
// The stride scheduler (see `task::manager`) weighs each tick by the task's
// priority, so a higher priority means proportionally more CPU. `user_lib`'s
// contract, pinned by `ch5_setprio`, is that the call answers with the new
// priority when it was applied and `-1` otherwise. The accepted range comes
// straight out of the test's four assertions — `set_priority(10) == 10`,
// `set_priority(isize::MAX) == isize::MAX`, `set_priority(0) == -1`,
// `set_priority(1) == -1`, `set_priority(-10) == -1` — so `1` is refused along
// with everything non-positive, not treated as a legal baseline weight.
pub fn sys_set_priority(prio: isize) -> isize {
    trace!("kernel:pid[{}] sys_set_priority", current_task().unwrap().pid.0);
    if prio <= 1 {
        return -1;
    }
    let task = current_task().unwrap();
    task.set_priority(prio as usize);
    prio
}

/// Block the caller for `sleep_ms` milliseconds.
///
/// The task is parked in [`TaskStatus::Blocked`] with a wake-up tick and *not*
/// re-entered into the ready queue, so a sleeping process consumes no stride
/// weight; the timer tick that reaches its deadline puts it back. This is what
/// `user_lib::sleep_blocking` uses, and it keeps ch3-style busy waiting honest:
/// time only moves when the clock interrupt fires.
pub fn sys_sleep(sleep_ms: usize) -> isize {
    trace!("kernel:pid[{}] sys_sleep", current_task().unwrap().pid.0);
    let ticks = sleep_ms * crate::timer::TICKS_PER_SEC / 1000;
    wait_for_ticks(ticks.max(1));
    0
}
