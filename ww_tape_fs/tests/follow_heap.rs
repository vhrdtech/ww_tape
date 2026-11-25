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

use shrink_wrap::prelude::{RefVec, UNib32};
use ww_tape::{
    ClockOrigin, ClockSpec, Definitions, EventsDef, FLAG_GROUP, FileBlocks, FullVersion,
    GROUP_BYTES_MAX, GroupPut, Magnitude, MemSink, PieceTapes, RELAY_HELD_BYTES_MAX,
    RUN_BLOCKS_MAX, Recovery, Segment, StatePolicy, SyncPolicy, TapeWriter, TrackKind, TrackNum,
    TrackRepeat, TrackSpec, Version, clock_key, track_key,
};
use ww_tape_fs::follow::{ChainFollower, FollowOptions};
use ww_tape_testalloc::{Counting, thread_heap_of};

#[global_allocator]
static ALLOC: Counting = Counting::new();

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

/// Definitions of a clock and `n` state tracks on it, and the tracks' numbers.
fn defined(n: usize) -> (Definitions, Vec<TrackNum>) {
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
    let tracks = (0..n as u32)
        .map(|k| defs.define_track(track(k, clock)).unwrap())
        .collect();
    (defs, tracks)
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
    let ((blocks, bytes), heap) = thread_heap_of(|| {
        let (mut blocks, mut bytes) = (0usize, 0usize);
        while let Some(b) = f.try_next().unwrap() {
            blocks += 1;
            bytes += b.block.bytes().len();
        }
        (blocks, bytes)
    });
    (blocks, bytes, heap.peak_bytes)
}

/// The heap of a follower over a group of puts of [GROUP_BYTES_MAX] on as many tracks as items: once with the
/// tracks' definitions inside the run, and once with them written before it, which leaves the run's own cost.
#[test]
fn a_follower_holds_a_runs_places_not_its_bytes() {
    let dir = std::env::temp_dir().join(format!("ww_tape_follow_heap_{}", std::process::id()));
    _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let n = GROUP_BYTES_MAX / ITEM;
    let (defs, tracks) = defined(n);
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
                item: ww_tape::GroupItem::State(item),
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

/// The blocks of a group of puts of `items`, one on each of `tracks`, as a producer writing it from its first call
/// sends them: the tracks' definitions among them.
fn group_blocks(defs: &Definitions, tracks: &[TrackNum], items: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    let tape = w.sink().tape();
    let puts: Vec<GroupPut<'_>> = tracks
        .iter()
        .zip(items)
        .map(|(track, item)| GroupPut {
            track: *track,
            ticks: 1,
            item: ww_tape::GroupItem::State(item),
        })
        .collect();
    w.put_group(defs, &puts).unwrap();
    let bytes = tape.to_vec().unwrap();
    let mut rd = FileBlocks::with_recovery(&bytes[..], Recovery::Prefix);
    let mut out = Vec::new();
    while let Some(Ok(b)) = rd.next_block() {
        if b.flags & FLAG_GROUP != 0 {
            out.push(b.bytes().to_vec());
        }
    }
    out
}

/// A relay holding all of `blocks` but the last: the heap it holds after, its peak while it takes them, and what it
/// counts ([TapeWriter::relay_held_bytes]).
fn relay_held(defs: &Definitions, blocks: &[Vec<u8>]) -> (usize, usize, usize) {
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    let (_, last) = blocks.split_last().unwrap();
    assert_ne!(*last, [] as [Vec<u8>; 0]);
    let ((), heap) = thread_heap_of(|| {
        for b in &blocks[..blocks.len() - 1] {
            w.put_block(defs, b).unwrap();
        }
    });
    (heap.held_bytes, heap.peak_bytes, w.relay_held_bytes())
}

/// What a relay holds of a group stays within what it counts against [RELAY_HELD_BYTES_MAX], its peak too: for a group
/// of one-byte items on 5000 tracks with their definitions, where the overhead per record weighs most, and for one of
/// [GROUP_BYTES_MAX] of items.
#[test]
fn a_relay_holds_no_more_than_it_counts() {
    let n = 5_000;
    let (defs, tracks) = defined(n);
    let tiny: Vec<Vec<u8>> = (0..n).map(|k| vec![k as u8]).collect();
    let blocks = group_blocks(&defs, &tracks, &tiny);
    assert!(blocks.len() > 2 && blocks.len() < RUN_BLOCKS_MAX);
    let (held, peak, counted) = relay_held(&defs, &blocks);
    eprintln!(
        "TINY held {held} peak {peak} counted {counted} blocks {}",
        blocks.len()
    );
    assert!(
        held <= counted,
        "{n} one-byte items: {held} bytes held, {counted} counted"
    );
    assert!(
        peak <= counted,
        "{n} one-byte items: a peak of {peak} bytes, {counted} counted"
    );
    assert!(counted <= RELAY_HELD_BYTES_MAX);

    let n = GROUP_BYTES_MAX / ITEM;
    let (defs, tracks) = defined(n);
    let large: Vec<Vec<u8>> = (0..n).map(|k| vec![k as u8; ITEM]).collect();
    let blocks = group_blocks(&defs, &tracks, &large);
    drop(large);
    let (held, peak, counted) = relay_held(&defs, &blocks);
    eprintln!(
        "LARGE held {held} peak {peak} counted {counted} blocks {}",
        blocks.len()
    );
    assert!(
        held <= counted,
        "{n} items of {ITEM} bytes: {held} bytes held, {counted} counted"
    );
    assert!(
        peak <= counted,
        "{n} items of {ITEM} bytes: a peak of {peak} bytes, {counted} counted"
    );
}
