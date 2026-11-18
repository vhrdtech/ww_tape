//! The case runner: an allocation over the cap and a panic are outcomes of the case, not of the test.

use ww_tape_fuzz::case::{ALLOC_MAX, Outcome, run};

fn allocates_over_the_cap(_: &[u8]) {
    let v: Vec<u8> = Vec::with_capacity(ALLOC_MAX + 1);
    std::hint::black_box(&v);
}

fn allocates_at_the_cap(_: &[u8]) {
    let v: Vec<u8> = Vec::with_capacity(ALLOC_MAX);
    std::hint::black_box(&v);
}

fn panics(data: &[u8]) {
    assert!(data.is_empty(), "a case that panics");
}

#[test]
fn an_allocation_over_the_cap_is_the_case_outcome() {
    assert_eq!(
        run(allocates_over_the_cap, &[]).0,
        Outcome::Alloc(ALLOC_MAX + 1)
    );
    assert_eq!(run(allocates_at_the_cap, &[]).0, Outcome::Ok);
    // the next case starts clean
    assert_eq!(run(panics, &[]).0, Outcome::Ok);
}

/// Barriers of [waits_while_another_thread_allocates]: the case has started, the other thread has allocated.
static STARTED: std::sync::Barrier = std::sync::Barrier::new(2);
static ALLOCATED: std::sync::Barrier = std::sync::Barrier::new(2);

fn waits_while_another_thread_allocates(_: &[u8]) {
    STARTED.wait();
    ALLOCATED.wait();
}

/// An allocation over the cap on another thread, made while a case runs, is not that case's outcome: test harnesses
/// run cases on parallel threads.
#[test]
fn an_allocation_on_another_thread_is_not_the_case_outcome() {
    let other = std::thread::spawn(|| {
        STARTED.wait();
        allocates_over_the_cap(&[]);
        ALLOCATED.wait();
    });
    assert_eq!(
        run(waits_while_another_thread_allocates, &[]).0,
        Outcome::Ok
    );
    other.join().expect("the other thread ends");
}

#[test]
fn a_panic_is_the_case_outcome_with_its_place() {
    let Outcome::Panic(at, msg) = run(panics, &[1]).0 else {
        panic!("the case panics");
    };
    assert!(at.starts_with("ww_tape_fuzz/tests/it/limits.rs:"), "{at}");
    assert_eq!(msg, "a case that panics");
}
