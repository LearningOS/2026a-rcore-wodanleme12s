//! File and filesystem-related syscalls

use crate::mm::copy_from_user;
use alloc::vec;

const FD_STDOUT: usize = 1;

/// write buf of length `len`  to a file with `fd`
///
/// The buffer usually lives in the application's `.rodata`, but it can be any
/// user address, so it is copied out through the page-table gate rather than
/// dereferenced directly. A buffer that straddles a page boundary or crosses
/// into an unmapped page is handled (or refused) per page.
pub fn sys_write(fd: usize, buf: *const u8, len: usize) -> isize {
    trace!("kernel: sys_write");
    match fd {
        FD_STDOUT => {
            let mut buffer = vec![0u8; len];
            if !copy_from_user(buf as usize, &mut buffer) {
                return -1;
            }
            // The payload comes from user memory, so it need not be UTF-8. Print
            // it byte-wise instead of panicking the whole kernel on one stray
            // 0xFF.
            for &byte in buffer.iter() {
                print!("{}", byte as char);
            }
            len as isize
        }
        _ => {
            panic!("Unsupported fd in sys_write!");
        }
    }
}
