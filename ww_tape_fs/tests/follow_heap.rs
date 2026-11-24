#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    reason = "tests fail by panicking and compute expected values freely"
)]
//! The heap of a follower over the longest run a writer makes: a group of puts of [GROUP_BYTES_MAX] in as many blocks
//! as it takes. The follower holds the places of the run's blocks, not their bytes, so its heap stays near one block
//! besides the run's definitions.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use shrink_wrap::prelude::{RefVec, UNib32};
use ww_tape::{
    ClockOrigin, ClockSpec, Definitions, EventsDef, FullVersion, GROUP_BYTES_MAX, GroupPut,
    Magnitude, PieceTapes, RUN_BLOCKS_MAX, Segment, StatePolicy, SyncPolicy, TrackKind, TrackNum,
    TrackRepeat, TrackSpec, Version, clock_key, track_key,
};
use ww_tape_fs::follow::{ChainFollower, FollowOptions};

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

/// Bytes of an item: one more than half a block of puts, so each item takes a block of its own.
const ITEM: usize = (32 << 10) + 1;

/// A state track `k` on `clock`.
fn track(k: u32, clock: ww_tape::ClockNum) -> TrackSpec<'static> {
    TrackSpec {
        key: track_key(0x9e, &[Segment::Index(k)]),
        clock,
        dimensions: RefVec::Slice { slice: &[] },
        items: ww_tape::Items::WireWeaver {
            api: FullVersion::new("heap_api", Version::new(0, 1, 0)),
            bundle: RefVec::Slice { slice: &[] },
            path: RefVec::Slice {
                slice: &[UNib32(1)],
            },
            keys: RefVec::Slice { slice: &[] },
            item: None,
        },
        label: "slot",
        repeat: TrackRepeat::State(StatePolicy::DEFAULT),
        kind: TrackKind::Events(EventsDef::INSTANTS),
        relations: RefVec::Slice { slice: &[] },
        defined_twice: false,
        owner: [0; 32],
    }
}

/// A follower over the file `path` hands out every block; the peak heap of handing out those from `from` on. The
/// blocks handed out from `from` and their bytes too.
fn follow_peak(path: &std::path::Path, from: usize) -> (usize, usize, usize) {
    let mut f = ChainFollower::new(
        path,
        FollowOptions {
            tapes: PieceTapes::One,
            ..FollowOptions::default()
        },
    )
    .unwrap();
    let mut at = 0;
    while at < from {
        let b = f.try_next().unwrap().expect("the blocks before `from`");
        at = b.offset as usize + b.block.bytes().len();
    }
    assert_eq!(at, from, "`from` is where a block starts");
    let ((blocks, bytes), peak) = peak_of(|| {
        let (mut blocks, mut bytes) = (0usize, 0usize);
        while let Some(b) = f.try_next().unwrap() {
            blocks += 1;
            bytes += b.block.bytes().len();
        }
        (blocks, bytes)
    });
    (blocks, bytes, peak)
}

/// The heap of a follower over a group of puts of [GROUP_BYTES_MAX] on as many tracks as items: once with the
/// tracks' definitions inside the run, and once with them written before it, which leaves the run's own cost.
#[test]
fn a_follower_holds_a_runs_places_not_its_bytes() {
    let dir = std::env::temp_dir().join(format!("ww_tape_follow_heap_{}", std::process::id()));
    _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut defs = Definitions::new();
    let clock = defs
        .define_clock(ClockSpec {
            key: clock_key(0x9e, &[], 1),
            rate: Magnitude::pow10(9).into(),
            resolution: Magnitude::pow10(-9).into(),
            origin: ClockOrigin::Boot,
            label: "host",
        })
        .unwrap();
    let n = GROUP_BYTES_MAX / ITEM;
    let tracks: Vec<TrackNum> = (0..n as u32)
        .map(|k| defs.define_track(track(k, clock)).unwrap())
        .collect();
    let items: Vec<Vec<u8>> = (0..n).map(|k| vec![k as u8; ITEM]).collect();
    let group = |items: &[Vec<u8>]| -> Vec<(TrackNum, Vec<u8>)> {
        tracks.iter().copied().zip(items.iter().cloned()).collect()
    };
    let put = |w: &mut ww_tape::TapeWriter<_>, g: &[(TrackNum, Vec<u8>)]| {
        let p: Vec<GroupPut<'_>> = g
            .iter()
            .map(|(track, item)| GroupPut {
                track: *track,
                ticks: 1,
                item,
            })
            .collect();
        w.put_group(&defs, &p).unwrap();
    };

    // the definitions inside the run
    let path = dir.join("with_definitions.wwt");
    let mut w = ww_tape_fs::writer::create(&path, SyncPolicy::None).unwrap();
    put(&mut w, &group(&items));
    w.sync().unwrap();
    drop(w);
    let run = std::fs::metadata(&path).unwrap().len() as usize;
    assert!(run > GROUP_BYTES_MAX, "the group is in the file");
    let (blocks, bytes, peak) = follow_peak(&path, 0);
    assert_eq!(bytes, run, "every block handed out");
    assert!(
        blocks > n && blocks < RUN_BLOCKS_MAX,
        "{blocks} blocks, the run of {n} items within the cap"
    );
    // the run's definitions taken at once, a block of puts, the walk's buffer, and a place per held block
    assert!(
        peak < 4 << 20,
        "a follower's peak heap of {peak} bytes over a run of {run} bytes with its definitions"
    );

    // the definitions written before, by a group of one-byte items: the run is the items alone
    let path = dir.join("items_only.wwt");
    let mut w = ww_tape_fs::writer::create(&path, SyncPolicy::None).unwrap();
    put(&mut w, &group(&vec![vec![0xa5]; n]));
    w.sync().unwrap();
    let from = std::fs::metadata(&path).unwrap().len() as usize;
    put(&mut w, &group(&items));
    w.sync().unwrap();
    drop(w);
    drop(items);
    let run = std::fs::metadata(&path).unwrap().len() as usize - from;
    let (blocks, bytes, peak) = follow_peak(&path, from);
    assert_eq!(bytes, run, "every block of the run handed out");
    assert!(
        blocks >= n && blocks < RUN_BLOCKS_MAX,
        "{blocks} blocks, one or more per item"
    );
    // a block of puts, the walk's buffer, and a place per held block: 678444 bytes when measured
    assert!(
        peak < 1 << 20,
        "a follower's peak heap of {peak} bytes over a run of {run} bytes of items alone"
    );
    _ = std::fs::remove_dir_all(&dir);
}
