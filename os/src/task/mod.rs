//! Task management implementation
//!
//! Everything about task management, like starting and switching tasks is
//! implemented here.
//!
//! A single global instance of [`TaskManager`] called `TASK_MANAGER` controls
//! all the tasks in the whole operating system.
//!
//! A single global instance of [`Processor`] called `PROCESSOR` monitors running
//! task(s) for each core.
//!
//! A single global instance of `PID_ALLOCATOR` allocates pid for user apps.
//!
//! Be careful when you see `__switch` ASM function in `switch.S`. Control flow around this function
//! might not be what you expect.
mod context;
mod id;
mod manager;
mod processor;
mod switch;
#[allow(clippy::module_inception)]
mod task;

use crate::fs::{open_file, OpenFlags};
use crate::sync::UPSafeCell;
use alloc::sync::Arc;
use alloc::vec::Vec;
use lazy_static::*;
pub use manager::{add_task, fetch_task, min_runnable_pass, TaskManager};
use switch::__switch;
pub use task::{TaskControlBlock, TaskStatus};

pub use context::TaskContext;
pub use id::{kstack_alloc, pid_alloc, KernelStack, PidHandle};
pub use processor::{
    current_task, current_trap_cx, current_user_token, run_tasks, schedule, take_current_task,
    Processor,
};
/// Suspend the current 'Running' task and run the next task in task list.
pub fn suspend_current_and_run_next() {
    // There must be an application running.
    let task = take_current_task().unwrap();

    // ---- access current TCB exclusively
    let mut task_inner = task.inner_exclusive_access();
    let task_cx_ptr = &mut task_inner.task_cx as *mut TaskContext;
    // Change status to Ready
    task_inner.task_status = TaskStatus::Ready;
    drop(task_inner);
    // ---- release current PCB

    // push back to ready queue.
    add_task(task);
    // jump to scheduling cycle
    schedule(task_cx_ptr);
}

/// Park the current task until `ticks` timer ticks have passed.
///
/// The task deliberately does *not* go back into the ready queue: it is
/// `Blocked`, and [`check_timeout`] re-admits it when its deadline arrives. That
/// keeps a sleeping process from eating stride weight (which would make
/// `set_priority` meaningless) while still letting the clock, not the task,
/// decide when it wakes.
pub fn wait_for_ticks(ticks: usize) {
    let task = take_current_task().unwrap();
    let (task_cx_ptr, deadline) = {
        let mut inner = task.inner_exclusive_access();
        inner.task_status = TaskStatus::Blocked;
        inner.wake_ticks = crate::timer::get_time() + ticks;
        (&mut inner.task_cx as *mut TaskContext, inner.wake_ticks)
    };
    BLOCKED_TASKS.exclusive_access().push((deadline, task));
    schedule(task_cx_ptr);
}

/// Re-admit every task whose sleep deadline has passed. Runs on the tick,
/// before the scheduler picks the next process, so a sleeper resumes exactly on
/// the first tick after its deadline — never because it polled the clock.
pub fn check_timeout() {
    let now = crate::timer::get_time();
    let mut blocked = BLOCKED_TASKS.exclusive_access();
    let mut wakees = Vec::new();
    blocked.retain(|(deadline, task)| {
        if *deadline <= now {
            // Only the owner of a Blocked entry may flip it to Ready; that is
            // what makes this list the single source of truth for sleepers.
            task.inner_exclusive_access().task_status = TaskStatus::Ready;
            wakees.push(task.clone());
            false
        } else {
            true
        }
    });
    drop(blocked);
    for task in wakees {
        add_task(task);
    }
}

/// pid of usertests app in make run TEST=1
pub const IDLE_PID: usize = 0;

/// Exit the current 'Running' task and run the next task in task list.
pub fn exit_current_and_run_next(exit_code: i32) {
    // take from Processor
    let task = take_current_task().unwrap();

    let pid = task.getpid();
    if pid == IDLE_PID {
        println!(
            "[kernel] Idle process exit with exit_code {} ...",
            exit_code
        );
        panic!("All applications completed!");
    }

    // **** access current TCB exclusively
    let mut inner = task.inner_exclusive_access();
    // Change status to Exited: the task no longer runs, but its TCB (and exit
    // code) stays alive until a parent reaps it with wait/waitpid.
    inner.task_status = TaskStatus::Exited;
    // Record exit code
    inner.exit_code = exit_code;
    // do not move to its parent but under initproc

    // ++++++ access initproc TCB exclusively
    {
        let mut initproc_inner = INITPROC.inner_exclusive_access();
        for child in inner.children.iter() {
            child.inner_exclusive_access().parent = Some(Arc::downgrade(&INITPROC));
            initproc_inner.children.push(child.clone());
        }
    }
    // ++++++ release parent PCB

    inner.children.clear();
    // deallocate user space
    inner.memory_set.recycle_data_pages();
    // drop file descriptors: the last Arc going away is what frees an inode's
    // data blocks in `unlinkat`, so an exiting process must not keep them alive.
    inner.fd_table.clear();
    drop(inner);
    // **** release current PCB
    // drop task manually to maintain rc correctly
    drop(task);
    // we do not have to save task context
    let mut _unused = TaskContext::zero_init();
    schedule(&mut _unused as *mut _);
}

lazy_static! {
    /// Tasks parked by `sys_sleep`, as (wake-up tick, TCB).
    ///
    /// A separate list rather than a slot inside the ready heap: an entry in the
    /// heap carries the key it was pushed with, so a task that changed its own
    /// priority while sleeping would be impossible to find without scanning.
    pub static ref BLOCKED_TASKS: UPSafeCell<Vec<(usize, Arc<TaskControlBlock>)>> =
        unsafe { UPSafeCell::new(Vec::new()) };
}

lazy_static! {
    /// Creation of initial process
    ///
    /// the name "initproc" may be changed to any other app name like "usertests",
    /// but we have user_shell, so we don't need to change it.
    ///
    /// From this chapter on the init program is read out of the disk image rather
    /// than baked into the kernel as an ELF blob: `easy-fs` holds every user app as
    /// a file in the root directory, and `open_file` + `read_all` give us its bytes.
    pub static ref INITPROC: Arc<TaskControlBlock> = Arc::new({
        let inode = open_file("ch6b_initproc", OpenFlags::RDONLY).unwrap();
        let v = inode.read_all();
        TaskControlBlock::new(v.as_slice())
    });
}

///Add init process to the manager
pub fn add_initproc() {
    add_task(INITPROC.clone());
}
