//! RISC-V timer-related functionality

use crate::config::CLOCK_FREQ;
use crate::sbi::set_timer;
use riscv::register::time;

/// The number of ticks per second
pub const TICKS_PER_SEC: usize = 100;
/// Ticks per millisecond, as a fraction: `ticks = ms * TICKS_PER_SEC / 1000`.
/// Kept as the ratio so a slow clock still rounds to at least one tick.
pub const TICKS_PER_MS_NUM: usize = TICKS_PER_SEC;
/// The number of milliseconds per second
pub const MSEC_PER_SEC: usize = 1000;
/// The number of microseconds per second
pub const MICRO_PER_SEC: usize = 1_000_000;

/// Get the current time in ticks
pub fn get_time() -> usize {
    time::read()
}

/// get current time in milliseconds
pub fn get_time_ms() -> usize {
    get_time() * MSEC_PER_SEC / CLOCK_FREQ
}

/// get current time in microseconds
pub fn get_time_us() -> usize {
    get_time() * MICRO_PER_SEC / CLOCK_FREQ
}

/// Set the next timer interrupt
pub fn set_next_trigger() {
    set_timer(get_time() + CLOCK_FREQ / TICKS_PER_SEC);
}

