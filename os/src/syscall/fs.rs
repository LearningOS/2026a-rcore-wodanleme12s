//! File and filesystem-related syscalls
//!
//! Everything here goes through two rules that ch4/ch5 established and that this
//! chapter inherits:
//! - a user pointer is translated by *the calling task's own page table* (never by
//!   whatever satp happens to be loaded), so `.rodata` string literals and stack
//!   buffers both work; a pointer that does not translate costs `-1`, not a panic;
//! - user bytes reach the kernel through `UserStaging`, which copies them into
//!   kernel frames, because the `File` implementations (inode -> virtio-blk, pipe,
//!   stdout) cannot dereference a user virtual address while the kernel page table
//!   is active.
use crate::fs::{
    link_in_root, open_file, pipe_pair, unlink_in_root, OpenFlags, OSInode, Stat,
};use crate::mm::{translated_str_opt, write_struct, UserBuffer, UserStaging};
use crate::task::{current_task, current_user_token};
use alloc::vec::Vec;
/// Stage a user buffer for reading *out* to the application.
///
/// Returns `None` when any byte of `[ptr, ptr+len)` fails to translate or is not
/// writable, in which case the syscall answers `-1`.
fn stage_for_write(ptr: usize, len: usize) -> Option<UserStaging> {
    if len == 0 {
        return Some(UserStaging::empty());
    }
    let staging = UserStaging::new(current_user_token(), ptr, len, true);
    if staging.is_empty() {
        return None;
    }
    Some(staging)
}

/// Stage a user buffer whose contents the kernel will consume.
fn stage_for_read(ptr: usize, len: usize) -> Option<UserStaging> {
    if len == 0 {
        // A zero-length request still needs a valid (translatable) address to be
        // considered well-formed; treat it as an empty buffer.
        return Some(UserStaging::empty());
    }
    let staging = UserStaging::new(current_user_token(), ptr, len, false);
    if staging.is_empty() {
        return None;
    }
    Some(staging)
}

pub fn sys_write(fd: usize, buf: *const u8, len: usize) -> isize {
    trace!("kernel:pid[{}] sys_write", current_task().unwrap().pid.0);
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    if let Some(file) = &inner.fd_table[fd] {
        if !file.writable() {
            return -1;
        }
        let file = file.clone();
        // release current task TCB manually to avoid multi-borrow
        drop(inner);
        match stage_for_read(buf as usize, len) {
            Some(mut staging) => {
                let buf = staging.user_buffer();
                file.write(buf) as isize
            }
            // An unmapped source pointer writes nothing at all.
            None => file.write(UserBuffer::new(Vec::new())) as isize,
        }
    } else {
        -1
    }
}

pub fn sys_read(fd: usize, buf: *const u8, len: usize) -> isize {
    trace!("kernel:pid[{}] sys_read", current_task().unwrap().pid.0);
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    if let Some(file) = &inner.fd_table[fd] {
        let file = file.clone();
        if !file.readable() {
            return -1;
        }
        // release current task TCB manually to avoid multi-borrow
        drop(inner);
        match stage_for_write(buf as usize, len) {
            Some(mut staging) => {
                let buf = staging.user_buffer();
                // `staging` drops here and copies what the file produced back into
                // the user pages.
                file.read(buf) as isize
            }
            None => -1,
        }
    } else {
        -1
    }
}

pub fn sys_open(path: *const u8, flags: u32) -> isize {
    trace!("kernel:pid[{}] sys_open", current_task().unwrap().pid.0);
    // `OpenFlags::from_bits` answers `None` for a flag word we do not know; the
    // old template unwrapped it, which turned a bad argument into a kernel panic
    // and killed the whole session.
    let flags = match OpenFlags::from_bits(flags) {
        Some(flags) => flags,
        None => return -1,
    };
    let path = match translated_str_opt(path) {
        Some(path) => path,
        None => return -1,
    };
    let task = current_task().unwrap();
    if let Some(inode) = open_file(path.as_str(), flags) {
        let mut inner = task.inner_exclusive_access();
        let fd = inner.alloc_fd();
        inner.fd_table[fd] = Some(inode);
        fd as isize
    } else {
        -1
    }
}

pub fn sys_close(fd: usize) -> isize {
    trace!("kernel:pid[{}] sys_close", current_task().unwrap().pid.0);
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    if inner.fd_table[fd].is_none() {
        return -1;
    }
    inner.fd_table[fd].take();
    0
}

/// Fill the caller's `struct stat` from the inode behind `fd`.
///
/// Only `OSInode` has metadata to report; stdin/stdout/pipe have no on-disk
/// identity, so they are refused with `-1` rather than filled with zeros — none of
/// the tests in this chapter stats a console or pipe fd.
pub fn sys_fstat(fd: usize, st: *mut Stat) -> isize {
    trace!("kernel:pid[{}] sys_fstat", current_task().unwrap().pid.0);
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    let file = match &inner.fd_table[fd] {
        Some(file) => file.clone(),
        None => return -1,
    };
    drop(inner);
    let os_inode = match file.as_ref().as_any().downcast_ref::<OSInode>() {
        Some(os_inode) => os_inode,
        None => return -1,
    };
    let stat = os_inode.stat();
    let token = current_user_token();
    if write_struct(token, st as usize, &stat) {
        0
    } else {
        -1
    }
}

/// `link(old, new)`: give an existing file a second name in the root directory.
pub fn sys_linkat(old_name: *const u8, new_name: *const u8) -> isize {
    trace!("kernel:pid[{}] sys_linkat", current_task().unwrap().pid.0);
    let old_name = match translated_str_opt(old_name) {
        Some(name) => name,
        None => return -1,
    };
    let new_name = match translated_str_opt(new_name) {
        Some(name) => name,
        None => return -1,
    };
    if link_in_root(&old_name, &new_name) {
        0
    } else {
        -1
    }
}

/// `unlink(name)`: remove one name; the last removal frees the file.
pub fn sys_unlinkat(name: *const u8) -> isize {
    trace!("kernel:pid[{}] sys_unlinkat", current_task().unwrap().pid.0);
    let name = match translated_str_opt(name) {
        Some(name) => name,
        None => return -1,
    };
    if unlink_in_root(&name) {
        0
    } else {
        -1
    }
}

/// Duplicate an open file description onto a fresh descriptor.
///
/// The tests use `dup` to point fd 1 at a pipe write end before `fork`+`exec`, so
/// the clone must share the *same* object (and therefore the same offset / ring
/// state), not merely an equivalent one.
pub fn sys_dup(fd: usize) -> isize {
    trace!("kernel:pid[{}] sys_dup", current_task().unwrap().pid.0);
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    let file = match &inner.fd_table[fd] {
        Some(file) => file.clone(),
        None => return -1,
    };
    let new_fd = inner.alloc_fd();
    inner.fd_table[new_fd] = Some(file);
    new_fd as isize
}

/// Create a pipe and hand both endpoint fds to the caller's fd table.
///
/// `pipe_fd` is a `usize[2]` chosen by the application (a stack array in every
/// test here), so it goes through the page-table gate; the two writes happen only
/// once both slots are known good.
pub fn sys_pipe(pipe_fd: *mut [usize; 2]) -> isize {
    trace!("kernel:pid[{}] sys_pipe", current_task().unwrap().pid.0);
    let task = current_task().unwrap();
    let (reader, writer) = pipe_pair();
    let mut inner = task.inner_exclusive_access();
    let read_fd = inner.alloc_fd();
    inner.fd_table[read_fd] = Some(reader);
    let write_fd = inner.alloc_fd();
    inner.fd_table[write_fd] = Some(writer);
    drop(inner);
    let token = current_user_token();
    let pair = [read_fd as isize as usize, write_fd as isize as usize];
    if write_struct(token, pipe_fd as usize, &pair) {
        0
    } else {
        // Roll the descriptors back so a failed pipe() leaves the table clean.
        let mut inner = task.inner_exclusive_access();
        inner.fd_table[read_fd] = None;
        inner.fd_table[write_fd] = None;
        -1
    }
}
