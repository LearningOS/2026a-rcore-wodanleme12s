//! Memory-related abstractions that syscalls depend on.
//!
//! ch3 has no page tables — apps live in a fixed physical slot and each task
//! gets a static user stack — so this module only holds the user pointer gate,
//! [`user_ptr`]. From ch4 on it also owns the frame allocator, page tables and
//! per-task address spaces, and the gate grows a page-table backend there while
//! keeping the same public interface.

pub mod user_ptr;

pub use user_ptr::{
    copy_from_user, copy_to_user, read_u8, register_app_slot, register_user_stack, write_u8,
    UserAccess, UserPtr, UserRequest,
};
