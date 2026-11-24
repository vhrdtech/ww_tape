#![allow(
    clippy::arithmetic_side_effects,
    reason = "tests compute expected values freely"
)]
//! The counts of [Counting] as a test binary's global allocator.

use std::hint::black_box;

use ww_tape_testalloc::{Counting, live_bytes, peak_of, take_largest};

#[global_allocator]
static ALLOC: Counting = Counting::new();

/// A buffer allocated and freed inside `f` shows in its peak, not in what stays live after.
#[test]
fn peak_holds_a_buffer_freed_inside() {
    let before = live_bytes();
    let ((), peak) = peak_of(|| {
        black_box(vec![0u8; 1 << 20]);
    });
    assert!(peak >= 1 << 20, "a 1 MiB buffer in the peak: {peak} bytes");
    assert!(
        live_bytes() < before + (1 << 20),
        "the buffer is no longer live"
    );
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
