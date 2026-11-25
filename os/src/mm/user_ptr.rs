//! The user pointer gate: the only place where syscall code turns a user
//! virtual address into something the kernel may read or write.
//!
//! Design (BRIEF3 §4.1): there is no static range whitelist. Every request is
//! answered by walking *the current task's* page table, so the answer is
//! exactly what the hardware would have answered if the user had touched the
//! address itself:
//!
//! - the leaf PTE must be valid and readable, and carry the U bit;
//! - a write additionally requires the W bit, which is what makes
//!   `mmap(PROT_READ)` readable but not writable for [`trace_write`];
//! - an empty PTE never resolves (its ppn field is 0), so a hole in a VMA can
//!   not be mistaken for physical page zero;
//! - the resulting physical address must lie inside RAM, which is how kernel
//!   addresses such as `0x8020_0000` are refused even though they are identity
//!   mapped in the *kernel* space — they simply have no U-mapped PTE here.
//!
//! Because translation happens per page, a multi-page buffer works without any
//! "what if it is split across two pages" special case, and `munmap` of one
//! page cannot leave a stale window for its neighbours.
//!
//! All entry points return `None`/`false` on failure; syscalls turn that into
//! `-1`/`EINVAL`. Nothing here panics, since a user program gets to choose the
//! addresses.

use crate::config::PAGE_SIZE;
use crate::task::current_user_memory_set_ref;

/// Read `len` bytes of user memory into `dst`, page by page.
///
/// `dst` must be exactly as long as the request. Returns `false` when any byte
/// of `[addr, addr + len)` is not legally readable by the current task.
pub fn copy_from_user(addr: usize, dst: &mut [u8]) -> bool {
    let mut ok = true;
    // The address space is reached through a borrow of the task manager's
    // UPSafeCell guard, so the copy happens inside the closure: holding a
    // `&MemorySet` across it would let a caller keep reading a page table that
    // a task switch has since replaced.
    current_user_memory_set_ref(|ms| {
        let mut offset = 0;
        while offset < dst.len() {
            let va = match addr.checked_add(offset) {
                Some(va) => va,
                None => {
                    ok = false;
                    return;
                }
            };
            let page_off = va % PAGE_SIZE;
            let take = core::cmp::min(dst.len() - offset, PAGE_SIZE - page_off);
            let Some(ptr) = ms.translate_user_byte(va, false) else {
                ok = false;
                return;
            };
            // SAFETY: the gate vouched that [ptr, ptr + take) is a readable frame
            // of *this* task and lies inside RAM; `take` stays within that frame.
            unsafe {
                dst[offset..][..take]
                    .copy_from_slice(core::slice::from_raw_parts(ptr as *const u8, take));
            }
            offset += take;
        }
    });
    ok
}

/// Write `src` into user memory, page by page. Returns `false` when any byte of
/// the destination is not legally writable by the current task.
pub fn copy_to_user(addr: usize, src: &[u8]) -> bool {
    let mut ok = true;
    current_user_memory_set_ref(|ms| {
        let mut offset = 0;
        while offset < src.len() {
            let va = match addr.checked_add(offset) {
                Some(va) => va,
                None => {
                    ok = false;
                    return;
                }
            };
            let page_off = va % PAGE_SIZE;
            let take = core::cmp::min(src.len() - offset, PAGE_SIZE - page_off);
            let Some(ptr) = ms.translate_user_byte(va, true) else {
                ok = false;
                return;
            };
            // SAFETY: as in `copy_from_user`, but with the W bit checked.
            unsafe {
                core::slice::from_raw_parts_mut(ptr as *mut u8, take)
                    .copy_from_slice(&src[offset..][..take]);
            }
            offset += take;
        }
    });
    ok
}

/// Read a single user byte. Used by `sys_trace(Read)`, which has no idea
/// whether `addr` is aligned or even mapped.
pub fn read_u8(addr: usize) -> Option<u8> {
    current_user_memory_set_ref(|ms| {
        let ptr = ms.translate_user_byte(addr, false)?;
        // SAFETY: validated readable byte inside a frame of the current task.
        Some(unsafe { (ptr as *const u8).read_volatile() })
    })
}

/// Write a single user byte. Used by `sys_trace(Write)`, which must fail on a
/// read-only mapping.
pub fn write_u8(addr: usize, value: u8) -> Option<()> {
    current_user_memory_set_ref(|ms| {
        let ptr = ms.translate_user_byte(addr, true)?;
        // SAFETY: validated writable byte inside a frame of the current task.
        unsafe { (ptr as *mut u8).write_volatile(value) };
        Some(())
    })
}

/// Copy a plain-old-data value out of user memory.
pub fn read_struct<T: Copy>(addr: usize) -> Option<T> {
    let size = core::mem::size_of::<T>();
    if size == 0 || addr % core::mem::align_of::<T>() != 0 {
        return None;
    }
    // The largest POD the syscalls in this chapter pass around is two words, and
    // `Vec` avoids a generic-sized array (which is not a const expression here).
    let mut buf = alloc::vec![0u8; size];
    if !copy_from_user(addr, &mut buf) {
        return None;
    }
    // SAFETY: `buf` holds size_of::<T>() bytes and T's alignment was checked.
    Some(unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const T) })
}

/// Copy a plain-old-data value into user memory.
pub fn write_struct<T: Copy>(addr: usize, value: &T) -> bool {
    let size = core::mem::size_of::<T>();
    if size == 0 || addr % core::mem::align_of::<T>() != 0 {
        return false;
    }
    // SAFETY: reading a Copy value as its own bytes is always valid.
    let bytes = unsafe { core::slice::from_raw_parts(value as *const T as *const u8, size) };
    copy_to_user(addr, bytes)
}
