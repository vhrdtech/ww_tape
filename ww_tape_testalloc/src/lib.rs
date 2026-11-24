//! Tests and tools only: [Counting], a global allocator over the system's that counts the heap.
//!
//! It counts the bytes live and their peak over the whole process ([live_bytes], [peak_of]), and the largest single
//! allocation of each thread ([take_largest]). [Counting::refusing_over] also refuses an allocation past a size, which
//! aborts the process. A `realloc` is an allocation of the new size, a copy and a free, so its peak holds both.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Bytes live now, over every thread.
static LIVE: AtomicUsize = AtomicUsize::new(0);
/// The most bytes live at once since [baseline] last set it.
static PEAK: AtomicUsize = AtomicUsize::new(0);

// spirit-level: allow(thread-local-state) an allocator gets no state passed; per thread, tests side by side keep apart
std::thread_local! {
    /// The largest single allocation this thread made since [take_largest], 0 for none.
    static LARGEST: Cell<usize> = const { Cell::new(0) };
}

/// The system allocator, counting the heap. Install it as a binary's `#[global_allocator]`.
#[derive(Clone, Copy, Debug)]
pub struct Counting {
    /// Single allocations past this many bytes are refused.
    refused_over_bytes: usize,
}

impl Counting {
    /// Counts and refuses nothing.
    pub const fn new() -> Self {
        Self {
            refused_over_bytes: usize::MAX,
        }
    }

    /// Counts, and refuses single allocations past `bytes`: the allocation fails, which aborts the process.
    pub const fn refusing_over(bytes: usize) -> Self {
        Self {
            refused_over_bytes: bytes,
        }
    }

    /// Notes an allocation of `size` bytes this thread asks for; false when it is refused.
    fn note(self, size: usize) -> bool {
        // a thread being torn down has no slot left: its allocations belong to no test
        _ = LARGEST.try_with(|l| l.set(l.get().max(size)));
        size <= self.refused_over_bytes
    }
}

impl Default for Counting {
    fn default() -> Self {
        Self::new()
    }
}

/// Adds `size` bytes to the live count and raises the peak to it.
fn add_live(size: usize) {
    let live = LIVE.fetch_add(size, Ordering::Relaxed).saturating_add(size);
    PEAK.fetch_max(live, Ordering::Relaxed);
}

#[expect(
    unsafe_code,
    reason = "a GlobalAlloc is an unsafe trait: the count wraps System and changes no pointer"
)]
// SAFETY: every call goes to the system allocator with the caller's layout and pointer unchanged; a refused
// allocation returns null, which GlobalAlloc allows; the counters only observe.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if !self.note(layout.size()) {
            return std::ptr::null_mut();
        }
        // SAFETY: the caller's layout, as GlobalAlloc::alloc requires.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            add_live(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if !self.note(layout.size()) {
            return std::ptr::null_mut();
        }
        // SAFETY: the caller's layout, as GlobalAlloc::alloc_zeroed requires.
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            add_live(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's pointer and layout, as GlobalAlloc::dealloc requires.
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

/// Bytes live now, over every thread.
pub fn live_bytes() -> usize {
    LIVE.load(Ordering::Relaxed)
}

/// The most bytes live at once since the last [baseline].
pub fn peak_bytes() -> usize {
    PEAK.load(Ordering::Relaxed)
}

/// Starts counting the peak from the bytes live now: returns them, and sets the peak to them.
pub fn baseline() -> usize {
    let live = live_bytes();
    PEAK.store(live, Ordering::Relaxed);
    live
}

/// The peak heap `f` adds to what is live before it.
pub fn peak_of<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let live = baseline();
    let out = f();
    (out, peak_bytes().saturating_sub(live))
}

/// The largest single allocation this thread made since the last call, 0 for none; starts the next span.
pub fn take_largest() -> usize {
    LARGEST.with(|l| l.replace(0))
}
