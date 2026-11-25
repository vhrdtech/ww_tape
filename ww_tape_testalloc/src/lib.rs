//! Tests and tools only: [Counting], a global allocator over the system's that counts the heap.
//!
//! It counts the heap a call adds on the calling thread and the allocations it makes ([thread_heap_of]), as a heap
//! test measures them; the bytes live and their peak over the whole process ([live_bytes], [baseline],
//! [peak_bytes]), as a tool measuring alone does; and the largest single allocation of each thread ([take_largest]).
//! [Counting::refusing_over] also refuses an allocation past a size, which aborts the process. A `realloc` is an
//! allocation of the new size, a copy and a free, so its peak holds both.

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
    /// Bytes this thread allocated less those it freed, since it started; below 0 once it frees another thread's.
    static NET: Cell<isize> = const { Cell::new(0) };
    /// The highest [NET] since [thread_heap_of] last set it.
    static NET_PEAK: Cell<isize> = const { Cell::new(0) };
    /// Allocations this thread made since it started, a `realloc` counted as one.
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
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

/// Adds `size` bytes to the live count and raises the peak to it, over the process and on this thread.
fn add_live(size: usize) {
    let live = LIVE.fetch_add(size, Ordering::Relaxed).saturating_add(size);
    PEAK.fetch_max(live, Ordering::Relaxed);
    // a layout's size is at most isize::MAX; a thread being torn down has no slots left: its bytes belong to no test
    _ = NET.try_with(|n| {
        let net = n.get().wrapping_add(size.cast_signed());
        n.set(net);
        _ = NET_PEAK.try_with(|p| p.set(p.get().max(net)));
    });
    _ = ALLOCATIONS.try_with(|a| a.set(a.get().wrapping_add(1)));
}

/// Takes `size` freed bytes off the live count, over the process and on this thread.
fn sub_live(size: usize) {
    LIVE.fetch_sub(size, Ordering::Relaxed);
    _ = NET.try_with(|n| n.set(n.get().wrapping_sub(size.cast_signed())));
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
        sub_live(layout.size());
    }
}

/// Bytes live now, over every thread: for a binary measuring alone. A test's harness allocates on threads of its own
/// at any time, so a test measures with [thread_heap_of].
pub fn live_bytes() -> usize {
    LIVE.load(Ordering::Relaxed)
}

/// The most bytes live at once since the last [baseline], over every thread.
pub fn peak_bytes() -> usize {
    PEAK.load(Ordering::Relaxed)
}

/// Starts counting the peak from the bytes live now: returns them, and sets the peak to them.
pub fn baseline() -> usize {
    let live = live_bytes();
    PEAK.store(live, Ordering::Relaxed);
    live
}

/// The heap a call adds on the thread that makes it: see [thread_heap_of].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThreadHeap {
    /// The most bytes the call held at once.
    pub peak_bytes: usize,
    /// Bytes the call left live: those it allocated less those it freed, 0 where it freed more.
    pub held_bytes: usize,
    /// Allocations the call made, a `realloc` counted as one.
    pub allocations: u64,
}

/// The heap `f` adds on this thread: its peak and what it leaves live.
///
/// Only this thread's allocations count. A test's harness allocates on a thread of its own while the test runs
/// (libtest's main thread queues the test's timeout entry just after spawning it), and on a loaded host those bytes
/// would land inside the measurement. So `f` must make its allocations on this thread, not hand them to another.
pub fn thread_heap_of<T>(f: impl FnOnce() -> T) -> (T, ThreadHeap) {
    let start = NET.with(Cell::get);
    let allocations = ALLOCATIONS.with(Cell::get);
    NET_PEAK.with(|p| p.set(start));
    let out = f();
    let above = |net: isize| net.wrapping_sub(start).max(0).cast_unsigned();
    let heap = ThreadHeap {
        peak_bytes: above(NET_PEAK.with(Cell::get)),
        held_bytes: above(NET.with(Cell::get)),
        allocations: ALLOCATIONS.with(Cell::get).wrapping_sub(allocations),
    };
    (out, heap)
}

/// The largest single allocation this thread made since the last call, 0 for none; starts the next span.
pub fn take_largest() -> usize {
    LARGEST.with(|l| l.replace(0))
}
