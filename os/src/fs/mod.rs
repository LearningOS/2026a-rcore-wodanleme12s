//! File trait & inode(dir, file, pipe, stdin, stdout)

mod inode;
mod pipe;
mod stdio;

use crate::mm::UserBuffer;
use core::any::Any;

/// trait File for all file types
///
/// `Any` is the price of `sys_fstat`: descriptors arrive as trait objects and only
/// `OSInode` has on-disk metadata, so the syscall has to recover the concrete type
/// instead of guessing from the fd number.
pub trait File: Send + Sync + Any {
    /// the file readable?
    fn readable(&self) -> bool;
    /// the file writable?
    fn writable(&self) -> bool;
    /// read from the file to buf, return the number of bytes read
    fn read(&self, buf: UserBuffer) -> usize;
    /// write to the file from buf, return the number of bytes written
    fn write(&self, buf: UserBuffer) -> usize;
    /// Reveal the concrete type behind `dyn File`.
    fn as_any(&self) -> &dyn Any;
}

/// The stat of a inode
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Stat {
    /// ID of device containing file
    pub dev: u64,
    /// inode number
    pub ino: u64,
    /// file type and mode
    pub mode: StatMode,
    /// number of hard links
    pub nlink: u32,
    /// unused pad
    pad: [u64; 7],
}

bitflags! {
    /// The mode of a inode
    /// whether a directory or a file
    pub struct StatMode: u32 {
        /// null
        const NULL  = 0;
        /// directory
        const DIR   = 0o040000;
        /// ordinary regular file
        const FILE  = 0o100000;
    }
}

pub use inode::{
    link_in_root, list_apps, open_file, unlink_in_root, OSInode, OpenFlags,
};
pub use pipe::{pipe_pair, PipeReader, PipeWriter};
pub use stdio::{Stdin, Stdout};
