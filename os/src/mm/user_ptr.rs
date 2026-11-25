//! Access to user memory from syscall code — the single "user pointer gate".
//!
//! Every syscall that touches a user-supplied address must go through this
//! module; dereferencing a raw user pointer inside `syscall/` is not allowed.
//! The gate answers one question: *may the current task legally access these
//! bytes, and if so, where do they live in the kernel's flat address space?*
//!
//! ## Backend split
//!
//! [`UserAccess`] is the backend trait, and syscall code only ever talks to
//! [`UserPtr`] / [`copy_from_user`] / [`copy_to_user`], so swapping the backend
//! changes nothing outside this module:
//!
//! - [`FlatWindowAccess`] (this chapter): ch3 has no paging. Each app slot and
//!   each task's user stack is recorded once at load time by
//!   [`register_app_slot`] / [`register_user_stack`], and a window hit means
//!   "legally accessible". Note this is still a *per-task* decision — windows
//!   belonging to another task are never visible here — rather than a global
//!   range test repeated inside every syscall.
//! - Page-table backend (ch4+): translate the virtual address through the
//!   current task's page table and require `present` plus the R/W/X bit that
//!   matches the requested operation. That is what makes `mmap(PROT_READ)` let
//!   a read succeed but a write return `-1`, and what makes `munmap`
//!   invalidate an address. Only [`current_user_access`] and the trait impl
//!   change; the syscalls stay as they are.
//!
//! ## Alignment
//!
//! On RV64 the compiler assumes natural alignment for references and for
//! `ptr::read`, so the gate hands out aligned accesses only: misaligned user
//! pointers are rejected with `None` instead of trapping or silently
//! misbehaving. Byte-wise operations have no alignment requirement
//! ([`read_u8`] / [`write_u8`]), and [`copy_from_user`] / [`copy_to_user`] move
//! arbitrary buffers bytewise.

use core::cell::UnsafeCell;

/// Upper bound on simultaneously registered per-task windows. ch3 loads at most
/// `MAX_APP_NUM` apps, so this is room to spare; ch4 keys the same table by
/// task id.
const MAX_TASK_WINDOWS: usize = 64;

/// A request to touch `len` bytes of user memory starting at `addr`.
#[derive(Copy, Clone, Debug)]
pub struct UserRequest {
    /// user-visible address
    pub addr: usize,
    /// number of bytes
    pub len: usize,
    /// `true` when the kernel writes back through this request
    pub writable: bool,
}

impl UserRequest {
    /// Request `count` readable values of type `T` at `addr`.
    pub fn read_slice<T>(addr: usize, count: usize) -> Self {
        Self {
            addr,
            len: count * core::mem::size_of::<T>(),
            writable: false,
        }
    }

    /// Request `count` writable values of type `T` at `addr`.
    pub fn write_slice<T>(addr: usize, count: usize) -> Self {
        Self {
            addr,
            len: count * core::mem::size_of::<T>(),
            writable: true,
        }
    }

    /// Rejects null, wrapped, unaligned or partial-element requests before the
    /// backend is consulted, so a bogus length can never fake a valid range.
    fn well_formed<T>(&self) -> bool {
        let size = core::mem::size_of::<T>();
        size != 0
            && self.len != 0
            && self.len % size == 0
            && self.addr.checked_add(self.len).is_some()
            && self.addr % core::mem::align_of::<T>() == 0
    }
}

/// Backend for the user pointer gate; see the module documentation.
pub trait UserAccess {
    /// Kernel-visible address of `req`, or `None` if the current task may not
    /// access it.
    fn resolve(&self, req: UserRequest) -> Option<*mut u8>;
}

/// One legal user-memory window.
#[derive(Copy, Clone)]
struct FlatWindow {
    /// first byte of the window
    base: usize,
    /// one past the last byte; `base == end` marks an unused slot
    end: usize,
    /// whether the kernel may write into the window
    writable: bool,
}

impl FlatWindow {
    /// A new window `[base, base + size)`.
    const fn new(base: usize, size: usize, writable: bool) -> Self {
        Self {
            base,
            end: base + size,
            writable,
        }
    }

    /// Does this window cover `[addr, end)` completely? A request may not span
    /// two windows, which is what keeps `mmap`-style regions independent in ch4.
    fn covers(&self, addr: usize, end: usize) -> bool {
        self.base <= addr && end <= self.end
    }

    /// Whether the window is usable at all (unused slots are empty).
    const fn is_live(&self) -> bool {
        self.base != self.end
    }
}

/// An unused slot. Const so the static registry can be built without running
/// code at start-up.
const EMPTY_WINDOW: FlatWindow = FlatWindow {
    base: 0,
    end: 0,
    writable: false,
};

/// ch3 backend: static windows belonging to the current task.
pub struct FlatWindowAccess<'a> {
    windows: &'a [FlatWindow],
}

impl UserAccess for FlatWindowAccess<'_> {
    fn resolve(&self, req: UserRequest) -> Option<*mut u8> {
        // In ch3 user addresses *are* physical addresses, so the window bounds
        // double as the translation. ch4 computes this page by page instead.
        let end = req.addr.checked_add(req.len)?;
        self.windows
            .iter()
            .find(|w| w.is_live() && w.covers(req.addr, end))
            .filter(|w| !req.writable || w.writable)
            .map(|_| req.addr as *mut u8)
    }
}

/// Window registry backing the flat ch3 implementation.
///
/// Plain mutable statics rather than `RefCell`: the gate is consulted from
/// several places inside one syscall (validation, then each access through a
/// `UserPtr`), so a re-entrant borrow would abort, and the snapshot has to be
/// able to outlive the read. Single-threaded plus "windows are only ever added
/// during load" is the same tradeoff the rest of the kernel makes.
struct Windows {
    /// Shared application slot `[base, base + size)`, recorded by
    /// `loader::load_apps()` before the first task runs. Read-only: it holds
    /// the loaded images. `size == 0` means "not recorded yet".
    app_slot: UnsafeCell<(usize, usize)>,
    /// Per-task user stacks, recorded when a task is created. All writable; an
    /// empty range marks an unused slot.
    user_stacks: UnsafeCell<[FlatWindow; MAX_TASK_WINDOWS]>,
}

// SAFETY: the kernel runs on a single hart and every mutation happens in
// interrupt-disabled supervisor context, so no two accesses can race.
unsafe impl Sync for Windows {}

static WINDOWS: Windows = Windows {
    app_slot: UnsafeCell::new((0, 0)),
    user_stacks: UnsafeCell::new([EMPTY_WINDOW; MAX_TASK_WINDOWS]),
};

/// Record the shared application slot `[base, base + size)` as readable.
pub fn register_app_slot(base: usize, size: usize) {
    // SAFETY: called once from `load_apps()`, before any task runs.
    unsafe { *WINDOWS.app_slot.get() = (base, size) };
}

/// Record the writable user stack of the task occupying `slot_index`.
pub fn register_user_stack(slot_index: usize, base: usize, size: usize) {
    if slot_index < MAX_TASK_WINDOWS {
        // SAFETY: called while building each task's initial context, before
        // that task can be scheduled, and never concurrently with a read.
        unsafe {
            (*WINDOWS.user_stacks.get())[slot_index] = FlatWindow::new(base, size, true);
        };
    }
}

/// Snapshot of the windows visible to the current task.
///
/// The view points into the static registry, so no allocation happens per
/// syscall and nothing borrows the task manager — which matters because
/// `sys_trace` reads task state and resolves user pointers in the same call.
/// Sound while the registry only grows; ch4 replaces the body with "the page
/// table of the current task control block", whose lifetime follows the task.
fn current_user_access() -> FlatWindowAccess<'static> {
    // SAFETY: single hart; `register_*` runs while building a task's initial
    // context — before that task can be scheduled and with interrupts disabled
    // in supervisor mode — and readers never write back. The two views below
    // alias the registry on purpose: they are read-only snapshots taken inside
    // one syscall, and ch3 never removes a window once it is recorded.
    let app_slot = unsafe { &*WINDOWS.app_slot.get() };
    let stacks = unsafe { &*WINDOWS.user_stacks.get() };
    if app_slot.1 == 0 {
        return FlatWindowAccess { windows: stacks };
    }
    // One static slot holds the joined view; it is rebuilt from the registry on
    // every call, so it always describes live windows.
    struct Joined(UnsafeCell<[FlatWindow; MAX_TASK_WINDOWS + 1]>);
    // SAFETY: same single-hart argument as `WINDOWS`.
    unsafe impl Sync for Joined {}
    static JOINED: Joined = Joined(UnsafeCell::new([EMPTY_WINDOW; MAX_TASK_WINDOWS + 1]));
    unsafe {
        let joined = &mut *JOINED.0.get();
        joined[0] = FlatWindow::new(app_slot.0, app_slot.1, false);
        joined[1..].copy_from_slice(stacks);
        FlatWindowAccess {
            windows: core::slice::from_raw_parts(joined.as_ptr(), MAX_TASK_WINDOWS + 1),
        }
    }
}

/// A validated reference into user memory.
///
/// Build one with [`UserPtr::new`] / [`UserPtr::new_mut`] /
/// [`UserPtr::with_request`]; the constructors do the whole job, because a
/// successful one means "the current task may perform exactly this access".
/// [`UserPtr::kern`] is then the single place that turns a user address into a
/// kernel-visible pointer, which keeps the `unsafe` dereference in this module
/// instead of at each call site.
pub struct UserPtr<T> {
    req: UserRequest,
    access: FlatWindowAccess<'static>,
    marker: core::marker::PhantomData<T>,
}

impl<T> UserPtr<T> {
    /// Validate `addr` as a readable `T`.
    pub fn new(addr: usize) -> Option<Self> {
        Self::with_request(UserRequest::read_slice::<T>(addr, 1))
    }

    /// Validate `addr` as a writable `T`.
    pub fn new_mut(addr: usize) -> Option<Self> {
        Self::with_request(UserRequest::write_slice::<T>(addr, 1))
    }

    /// Validate an arbitrary request with element type `T`.
    pub fn with_request(req: UserRequest) -> Option<Self> {
        if !req.well_formed::<T>() {
            return None;
        }
        let access = current_user_access();
        access.resolve(req).map(|_| Self {
            req,
            access,
            marker: core::marker::PhantomData,
        })
    }

    /// The user-visible address this handle covers.
    pub const fn addr(&self) -> usize {
        self.req.addr
    }

    /// Number of `T`s covered.
    pub const fn count(&self) -> usize {
        self.req.len / core::mem::size_of::<T>()
    }

    /// Kernel-side pointer for this request, checked against the windows this
    /// handle was built from.
    pub fn kern(&self) -> Option<*mut T> {
        self.access.resolve(self.req).map(|p| p as *mut T)
    }

}

/// Read one user byte; no alignment required.
pub fn read_u8(addr: usize) -> Option<u8> {
    current_user_access()
        .resolve(UserRequest {
            addr,
            len: 1,
            writable: false,
        })
        .map(|p| unsafe { p.read_volatile() })
}

/// Write one user byte; `None` if the address is not writable user memory of
/// the current task.
pub fn write_u8(addr: usize, value: u8) -> Option<()> {
    current_user_access()
        .resolve(UserRequest {
            addr,
            len: 1,
            writable: true,
        })
        .map(|p| unsafe { p.write_volatile(value) })
}

/// Copy `src` into a user buffer of the same length. Returns `false` if the
/// destination is not writable user memory of the current task.
pub fn copy_to_user<T: Copy>(dst_addr: usize, src: &[T]) -> bool {
    let bytes = unsafe {
        // SAFETY: `src` is a kernel-local slice; describing its bytes keeps
        // the gate free of alignment rules for user buffers.
        core::slice::from_raw_parts(src.as_ptr() as *const u8, core::mem::size_of_val(src))
    };
    write_user_bytes(dst_addr, bytes)
}

/// Copy a user buffer of `dst.len()` values into `dst`. Returns `false` if the
/// source is not readable user memory of the current task.
pub fn copy_from_user<T: Copy>(src_addr: usize, dst: &mut [T]) -> bool {
    read_user_bytes(src_addr, unsafe {
        // SAFETY: as above, for the kernel-side destination.
        core::slice::from_raw_parts_mut(dst.as_mut_ptr() as *mut u8, core::mem::size_of_val(dst))
    })
}

/// The one byte-moving primitive behind both copies: validate the request
/// through the gate, then move bytes between the resolved user address and the
/// kernel-side buffer. `kernel_side` is read when the user side is being
/// written and written when the user side is being read, so it is passed as a
/// `&mut` even in the read direction.
fn transfer_user_bytes(req: UserRequest, kernel_side: &mut [u8]) -> bool {
    if !req.well_formed::<u8>() {
        return false;
    }
    match current_user_access().resolve(req) {
        Some(user_ptr) => {
            // SAFETY: the gate vouched that [user_ptr, user_ptr + len) lies
            // inside a single window of the current task with the requested
            // access mode, and `kernel_side` is kernel memory, so the two
            // regions cannot overlap.
            let user_bytes =
                unsafe { core::slice::from_raw_parts_mut(user_ptr, kernel_side.len()) };
            if req.writable {
                user_bytes.copy_from_slice(kernel_side);
            } else {
                kernel_side.copy_from_slice(user_bytes);
            }
            true
        }
        None => false,
    }
}

/// Read `dst.len()` bytes of user memory starting at `addr`.
pub fn read_user_bytes(addr: usize, dst: &mut [u8]) -> bool {
    transfer_user_bytes(
        UserRequest {
            addr,
            len: dst.len(),
            writable: false,
        },
        dst,
    )
}

/// Write `src.len()` bytes into user memory starting at `addr`.
pub fn write_user_bytes(addr: usize, src: &[u8]) -> bool {
    let kernel_side = unsafe {
        // SAFETY: only reborrows the kernel-side source mutably so both
        // directions can share one signature; the copy reads from it.
        core::slice::from_raw_parts_mut(src.as_ptr() as *mut u8, src.len())
    };
    transfer_user_bytes(
        UserRequest {
            addr,
            len: src.len(),
            writable: true,
        },
        kernel_side,
    )
}
