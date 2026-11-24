#![allow(
    clippy::unwrap_used,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "tests fail by panicking and compute expected values freely"
)]
//! The heap of an overview level as it is built: its statistics and nothing per source cell.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use ww_tape_summary::OverviewCells;

/// The system allocator, counting the bytes live and their peak.
struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every call goes to the system allocator with the same layout; the counters only observe.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract for `alloc` holds for `System` too.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from `alloc` with this layout.
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// The peak heap `f` adds to what is live before it.
fn peak_of<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let live = LIVE.load(Ordering::Relaxed);
    PEAK.store(live, Ordering::Relaxed);
    let out = f();
    (out, PEAK.load(Ordering::Relaxed) - live)
}

/// Bytes a level of `cells` cells holds: min, max and sum per scalar of an `elem`-byte element and a `sum`-byte sum,
/// a u32 count per cell, and a little for the axes.
fn level_bytes(cells: usize, elem: usize, sum: usize) -> usize {
    cells * (2 * elem + sum + 4) + 256
}

/// A 512 by 512 tile of u16, halved twice: each level holds its statistics, an i64 sum each, and no table of the
/// source cells; a f32 tile the same with f64 sums.
#[test]
fn a_level_holds_its_statistics_only() {
    let side = 512usize;
    let cells: Vec<u16> = (0..side * side).map(|i| (i * 7919) as u16).collect();
    let (l1, peak) = peak_of(|| OverviewCells::of_cells(&cells, 1, &[512, 512], 1, 0b11).unwrap());
    let want = level_bytes(side * side / 4, 2, 8);
    assert!(
        peak <= want,
        "level 1 of u16: {peak} bytes, want at most {want}"
    );
    let (_l2, peak) = peak_of(|| l1.halve(0b11).unwrap());
    let want = level_bytes(side * side / 16, 2, 8);
    assert!(
        peak <= want,
        "level 2 of u16: {peak} bytes, want at most {want}"
    );

    let floats: Vec<f32> = cells.iter().map(|v| f32::from(*v)).collect();
    let (_f1, peak) =
        peak_of(|| OverviewCells::of_cells(&floats, 1, &[512, 512], 1, 0b11).unwrap());
    let want = level_bytes(side * side / 4, 4, 8);
    assert!(
        peak <= want,
        "level 1 of f32: {peak} bytes, want at most {want}"
    );
}

/// A u32 tile of its largest values sums exactly in i64 at every level; a u64 one in i128.
#[test]
fn narrow_and_wide_sums_are_exact() {
    let l = OverviewCells::of_cells(&[u32::MAX; 64], 1, &[8, 8], 1, 0b11).unwrap();
    let l = l.halve(0b11).unwrap().halve(0b11).unwrap();
    assert_eq!(
        (l.min(), l.max(), l.means()),
        (&[u32::MAX][..], &[u32::MAX][..], vec![u32::MAX])
    );
    let l = OverviewCells::of_cells(&[i32::MIN, i32::MIN, i32::MIN, i32::MIN + 1], 1, &[4], 1, 1)
        .unwrap();
    assert_eq!(l.halve(1).unwrap().means(), [i32::MIN]);
    let l = OverviewCells::of_cells(&[u64::MAX; 16], 1, &[4, 4], 1, 0b11).unwrap();
    assert_eq!(l.halve(0b11).unwrap().means(), [u64::MAX]);
}
