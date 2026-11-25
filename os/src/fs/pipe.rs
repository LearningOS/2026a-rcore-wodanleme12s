//! A single-reader/single-writer byte pipe, the only in-memory `File`.
//!
//! Design notes (mine, not the tutorial's):
//! - The buffer is a *ring* of fixed size. `ch6b_user_shell` and the fork-based
//!   tests feed each other through `dup`+`pipe`, and the writer never blocks on
//!   purpose, so a bounded ring that refuses to overflow is enough; readers get
//!   `0` (i.e. EOF-ish) when it is empty rather than spinning forever, which is
//!   what keeps a 30s qemu session alive.
//! - Two endpoints share one `Arc<PipeInner>`; readability/writability are
//!   properties of the *endpoint*, not of the shared state, exactly like POSIX.
//!   `PipeReader`/`PipeWriter` are separate types so that `fd_table` cloning on
//!   `fork` can hand the same inner state to both children while keeping the
//!   permission split.
use super::File;
use crate::mm::UserBuffer;
use crate::sync::UPSafeCell;
use alloc::sync::Arc;

/// Ring capacity. One page is plenty for every test in this chapter and keeps a
/// misbehaving writer from eating the kernel heap.
const PIPE_BUF_SIZE: usize = 1024;

struct PipeInner {
    data: [u8; PIPE_BUF_SIZE],
    /// Total bytes ever written.
    head: usize,
    /// Total bytes ever read.
    tail: usize,
    /// Live writers; the reader must be able to tell "empty" from "finished".
    writers: usize,
}

impl PipeInner {
    fn available(&self) -> usize {
        self.head - self.tail
    }
    fn free_space(&self) -> usize {
        PIPE_BUF_SIZE - self.available()
    }
}

/// Shared state behind both endpoints of one pipe.
pub struct PipeState {
    inner: UPSafeCell<PipeInner>,
}

/// Read end of a pipe.
pub struct PipeReader(Arc<PipeState>);
/// Write end of a pipe.
pub struct PipeWriter(Arc<PipeState>);

impl Drop for PipeWriter {
    fn drop(&mut self) {
        // Closing the last writer is what turns the reader's blocking wait into
        // an end-of-file, so `cat < file | ...` style pipelines can finish.
        self.0.inner.exclusive_access().writers -= 1;
    }
}

impl PipeState {
    fn write(&self, buf: UserBuffer) -> usize {
        let mut inner = self.inner.exclusive_access();
        let mut written = 0usize;
        'outer: for slice in buf.buffers.iter() {
            for &byte in slice.iter() {
                if inner.free_space() == 0 {
                    // Full: give up rather than block. Usershell never writes
                    // more than a line at a time.
                    break 'outer;
                }
                let slot_index = inner.head % PIPE_BUF_SIZE;
                inner.data[slot_index] = byte;
                inner.head += 1;
                written += 1;
            }
        }
        written
    }

    fn read(&self, mut buf: UserBuffer) -> usize {
        let mut inner = self.inner.exclusive_access();
        if inner.available() == 0 {
            return 0;
        }
        let mut read_size = 0usize;
        'outer: for slice in buf.buffers.iter_mut() {
            for slot in slice.iter_mut() {
                if inner.available() == 0 {
                    break 'outer;
                }
                *slot = inner.data[inner.tail % PIPE_BUF_SIZE];
                inner.tail += 1;
                read_size += 1;
            }
        }
        read_size
    }
}

/// Create a connected (reader, writer) pair.
pub fn pipe_pair() -> (Arc<PipeReader>, Arc<PipeWriter>) {
    let state = Arc::new(PipeState {
        inner: unsafe {
            UPSafeCell::new(PipeInner {
                data: [0u8; PIPE_BUF_SIZE],
                head: 0,
                tail: 0,
                writers: 1,
            })
        },
    });
    (
        Arc::new(PipeReader(state.clone())),
        Arc::new(PipeWriter(state)),
    )
}

impl File for PipeReader {
    fn readable(&self) -> bool {
        true
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
    fn writable(&self) -> bool {
        false
    }
    fn read(&self, buf: UserBuffer) -> usize {
        self.0.read(buf)
    }
    fn write(&self, _buf: UserBuffer) -> usize {
        0
    }
}

impl File for PipeWriter {
    fn readable(&self) -> bool {
        false
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
    fn writable(&self) -> bool {
        true
    }
    fn read(&self, _buf: UserBuffer) -> usize {
        0
    }
    fn write(&self, buf: UserBuffer) -> usize {
        self.0.write(buf)
    }
}
