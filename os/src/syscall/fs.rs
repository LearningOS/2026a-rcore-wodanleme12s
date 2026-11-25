//! File and filesystem-related syscalls

use crate::mm::copy_from_user;

const FD_STDOUT: usize = 1;

/// Longest write we stage at once. `buf` and `len` both come from user space,
/// so the request is validated by the pointer gate and copied into this kernel
/// buffer before anything is printed.
const MAX_WRITE_LEN: usize = 4096;

/// write buf of length `len` to a file with `fd`
pub fn sys_write(fd: usize, buf: *const u8, len: usize) -> isize {
    trace!("kernel: sys_write");
    match fd {
        FD_STDOUT => {
            if len > MAX_WRITE_LEN {
                return -1;
            }
            // 4 KiB of stack per write is affordable here, but keep the buffer
            // to the requested size so a small print does not touch it all.
            let mut staging = [0u8; MAX_WRITE_LEN];
            // One gated copy for the whole buffer: either every byte is inside
            // the current task's readable windows or we print nothing at all.
            if !copy_from_user(buf as usize, &mut staging[..len]) {
                return -1;
            }
            // Invalid UTF-8 in a user buffer must not panic the kernel: print
            // the prefix that decodes and drop the rest rather than unwrapping.
            if let Ok(s) = core::str::from_utf8(&staging[..len]) {
                print!("{}", s);
            }
            len as isize
        }
        _ => {
            println!("[kernel] Unsupported fd {} in sys_write!", fd);
            -1
        }
    }
}
