#![allow(
    clippy::arithmetic_side_effects,
    reason = "tests compute expected values freely"
)]
//! The counts of [Counting] as a test binary's global allocator.

use std::hint::black_box;
use std::sync::Barrier;

use ww_tape_testalloc::{
    Counting, ThreadHeap, baseline, live_bytes, peak_bytes, take_largest, thread_heap_of,
};

#[global_allocator]
static ALLOC: Counting = Counting::new();

/// A buffer allocated and freed inside `f` shows in its peak, not in what it holds after; one kept shows in both.
#[test]
fn peak_holds_a_buffer_freed_inside() {
    let ((), heap) = thread_heap_of(|| {
        black_box(vec![0u8; 1 << 20]);
    });
    assert!(
        heap.peak_bytes >= 1 << 20,
        "a 1 MiB buffer in the peak: {heap:?}"
    );
    assert_eq!(heap.held_bytes, 0, "the buffer is no longer live");
    let (kept, heap) = thread_heap_of(|| black_box(vec![0u8; 1 << 20]));
    assert_eq!(
        heap,
        ThreadHeap {
            peak_bytes: 1 << 20,
            held_bytes: 1 << 20
        }
    );
    drop(kept);
}

/// Another thread allocating while a call is measured adds nothing to its heap: a test's harness does so at times.
#[test]
fn another_threads_heap_is_not_counted() {
    let barrier = Barrier::new(2);
    std::thread::scope(|s| {
        s.spawn(|| {
            barrier.wait();
            let held = black_box(vec![1u8; 1 << 20]);
            barrier.wait();
            barrier.wait();
            drop(held);
        });
        let ((), heap) = thread_heap_of(|| {
            barrier.wait();
            // the other thread holds its MiB now
            barrier.wait();
        });
        barrier.wait();
        assert_eq!(
            heap,
            ThreadHeap {
                peak_bytes: 0,
                held_bytes: 0
            },
            "another thread's MiB counted"
        );
    });
}

/// The process-wide peak counts every thread, for a binary measuring alone.
#[test]
fn the_process_peak_counts_every_thread() {
    let start = baseline();
    std::thread::spawn(|| black_box(vec![0u8; 1 << 20]))
        .join()
        .unwrap();
    assert!(
        peak_bytes() - start >= 1 << 20,
        "the other thread's MiB in the process peak"
    );
    assert!(live_bytes() < start + (1 << 20), "and no longer live");
}

/// A thread's largest allocation is its own, and taking it starts the next span.
#[test]
fn largest_is_per_thread_and_taken_once() {
    take_largest();
    black_box(vec![0u8; 3 << 20]);
    std::thread::spawn(|| black_box(vec![0u8; 8 << 20]))
        .join()
        .unwrap();
    let largest = take_largest();
    assert!(largest >= 3 << 20, "this thread's 3 MiB: {largest} bytes");
    assert!(
        largest < 8 << 20,
        "not the other thread's 8 MiB: {largest} bytes"
    );
    assert!(take_largest() < 3 << 20, "taken once");
}
