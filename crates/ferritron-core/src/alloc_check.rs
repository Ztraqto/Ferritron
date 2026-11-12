//! Allocation detection for tests and benchmarks (feature `alloc-check`).
//!
//! Install [`AllocGuard`] as the global allocator of a test or bench binary:
//!
//! ```ignore
//! #[global_allocator]
//! static ALLOC: ferritron::testing::AllocGuard = ferritron::testing::AllocGuard;
//! ```
//!
//! While a thread is inside [`audio_scope`], every allocation and free on it
//! is counted. [`Harness`](crate::testing::Harness) wraps `process` in a
//! scope, so `h.assert_no_alloc()` proves the audio path allocates nothing.
//! Only the calling thread is counted (worker threads are not the audio thread).

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    static DEPTH: Cell<u32> = const { Cell::new(0) };
    static COUNT: Cell<u64> = const { Cell::new(0) };
}

/// A global allocator that forwards to the system allocator and counts calls
/// made inside an [`audio_scope`].
pub struct AllocGuard;

fn note() {
    // `try_with`: the thread-locals are gone while a thread shuts down.
    let _ = DEPTH.try_with(|d| {
        if d.get() > 0 {
            let _ = COUNT.try_with(|c| c.set(c.get() + 1));
        }
    });
}

unsafe impl GlobalAlloc for AllocGuard {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        note();
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note();
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

/// Runs `f` with counting on; returns its result and the number of
/// allocations and frees this thread made meanwhile.
pub fn audio_scope<R>(f: impl FnOnce() -> R) -> (R, u64) {
    let before = COUNT.with(Cell::get);
    DEPTH.with(|d| d.set(d.get() + 1));
    let r = f();
    DEPTH.with(|d| d.set(d.get() - 1));
    (r, COUNT.with(Cell::get) - before)
}

/// Whether the current thread is inside an [`audio_scope`].
pub fn in_audio_scope() -> bool {
    DEPTH.with(|d| d.get() > 0)
}
