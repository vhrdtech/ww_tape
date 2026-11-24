//! Groups of puts ([TapeWriter::put_group]): a reader and a resume take a group whole or not at all, wherever a crash
//! cuts it; a follower hands it out whole; a chain cuts before a group, never inside it; the writer refuses a group
//! it can't write whole before writing any of it.

use std::fs::OpenOptions as FileOptions;
use std::io::Write;
use std::path::Path;

use ww_tape::index::IndexPolicy;
use ww_tape::{
    ChainOptions, CutPolicy, DescribeError, GROUP_BYTES_MAX, GroupPut, GroupRefusal, InputError,
    MemSink, PieceChain, PieceTapes, Slot, TapeReader, TapeSink, TapeWriter, TrackNum, WriteError,
};
use ww_tape_fs::follow::{ChainFollower, FollowOptions};

use crate::definitions::{Rig, begin_numbered, held, options, state};
use crate::pieces::scratch;

/// The state tracks of the groups.
const TRACKS: [u32; 3] = [10, 11, 12];
/// Bytes of an item of the large group: two don't fit one block of puts, so the group takes three blocks.
const LARGE: usize = 40 << 10;

/// A writer with [TRACKS] defined.
fn rig() -> Rig {
    let states = TRACKS.map(state);
    let mut w = begin_numbered(options(u64::MAX), ww_tape::Layout::UNALIGNED, &states);
    for d in &states {
        w.define(d).unwrap();
    }
    w
}

/// `items` put as one group, one on each of [TRACKS].
fn put_group(w: &mut Rig, items: &[Vec<u8>]) -> Result<Vec<bool>, WriteError<ww_tape::MemError>> {
    let puts: Vec<GroupPut<'_>> = TRACKS
        .iter()
        .zip(items)
        .map(|(t, item)| GroupPut {
            track: TrackNum::new(*t),
            ticks: 7,
            item,
        })
        .collect();
    w.w.put_group(&w.defs, &puts)
}

/// Items of `n` bytes each, one per track, made from `seed`.
fn items(seed: u8, n: usize) -> Vec<Vec<u8>> {
    (0..TRACKS.len())
        .map(|k| {
            (0..n)
                .map(|i| seed.wrapping_add(k as u8).wrapping_add(i as u8))
                .collect()
        })
        .collect()
}

/// The item in force at the end of `bytes` on each of [TRACKS].
fn in_force(bytes: &[u8]) -> Vec<Option<Vec<u8>>> {
    let r = TapeReader::open(bytes).unwrap();
    TRACKS
        .iter()
        .map(|t| {
            r.state_at(
                0,
                Slot::State {
                    track: TrackNum::new(*t),
                },
                u64::MAX,
            )
            .unwrap()
            .item
            .map(|i| i.item)
        })
        .collect()
}

/// A piece with a small group in force, then a large group of three blocks: the bytes, where the large group starts
/// and ends, the small and the large items.
struct Written {
    bytes: Vec<u8>,
    start: usize,
    end: usize,
    old: Vec<Vec<u8>>,
    new: Vec<Vec<u8>>,
}

fn written() -> Written {
    let mut w = rig();
    let tape = w.sink().tape();
    let old = items(1, 8);
    assert_eq!(put_group(&mut w, &old).unwrap(), vec![true; 3]);
    let start = tape.to_vec().unwrap().len();
    let new = items(100, LARGE);
    assert_eq!(put_group(&mut w, &new).unwrap(), vec![true; 3]);
    let bytes = tape.to_vec().unwrap();
    Written {
        end: bytes.len(),
        bytes,
        start,
        old,
        new,
    }
}

/// The places a crash cuts the large group at: every 509th byte, and each block's first, second and last byte.
fn cuts(w: &Written) -> Vec<usize> {
    let mut at: Vec<usize> = (w.start..=w.end).step_by(509).collect();
    let r = TapeReader::open(&w.bytes[..]).unwrap();
    for e in r.entries(0).unwrap() {
        let s = usize::try_from(e.offset).unwrap();
        let l = usize::try_from(e.len).unwrap();
        if s >= w.start && s < w.end {
            at.extend([s, s + 1, s + l - 1, s + l]);
        }
    }
    at.push(w.end);
    at.sort_unstable();
    at.dedup();
    at
}

/// Cut anywhere inside it, a group reads as none of it: the small group stays in force on every track, and the
/// marked blocks before the cut are listed as a dropped run. Whole, all of it is in force.
#[test]
fn a_torn_group_reads_as_none_of_it() {
    let w = written();
    let old: Vec<_> = w.old.iter().cloned().map(Some).collect();
    let new: Vec<_> = w.new.iter().cloned().map(Some).collect();
    let mut runs = 0;
    for cut in cuts(&w) {
        let bytes = &w.bytes[..cut];
        let want = if cut == w.end { &new } else { &old };
        assert_eq!(&in_force(bytes), want, "cut at {cut}");
        let r = TapeReader::open(bytes).unwrap();
        let dropped = &r.pieces()[0].dropped_runs;
        if cut == w.end || cut == w.start {
            assert!(dropped.is_empty(), "cut at {cut}: {dropped:?}");
        }
        for (at, len) in dropped {
            assert_eq!(*at, w.start as u64, "a dropped run starts with the group");
            assert!(at + len <= cut as u64, "a dropped run is whole blocks");
            runs += 1;
        }
        for e in r.entries(0).unwrap() {
            assert!(
                e.offset < w.start as u64 || cut == w.end,
                "cut at {cut}: the torn group's block at {} is listed",
                e.offset
            );
        }
    }
    assert!(runs > 0, "some cut leaves whole marked blocks");
}

/// A resume after a crash inside a group drops what the crash left of it, says so, and the group given again
/// writes the bytes of a run without the crash.
#[test]
fn a_resume_drops_a_torn_group_and_the_group_again_writes_the_same_bytes() {
    let w = written();
    let defs = rig().defs;
    for cut in cuts(&w) {
        if cut == w.end {
            continue;
        }
        let mut sink = MemSink::new();
        sink.write_all(&w.bytes[..cut]).unwrap();
        let tape = sink.tape();
        let (resumed, r) = TapeWriter::resume_on(sink, options(u64::MAX)).unwrap();
        assert_eq!(r.kept_bytes, w.start as u64, "cut at {cut}");
        if let Some(run) = &r.dropped_run {
            assert_eq!(run.start, w.start as u64, "cut at {cut}");
            assert!(run.end <= cut as u64, "cut at {cut}");
        }
        let mut again = Rig {
            w: resumed,
            defs: defs.clone(),
        };
        assert_eq!(put_group(&mut again, &w.new).unwrap(), vec![true; 3]);
        assert_eq!(tape.to_vec().unwrap(), w.bytes, "cut at {cut}");
    }
}

/// A group whose items are all in force already writes nothing; one whose items partly are writes only the others,
/// and both read as the group.
#[test]
fn a_group_writes_only_what_is_not_in_force() {
    let mut w = rig();
    let tape = w.sink().tape();
    let old = items(1, 8);
    put_group(&mut w, &old).unwrap();
    let before = tape.to_vec().unwrap().len();
    assert_eq!(put_group(&mut w, &old).unwrap(), vec![false; 3]);
    assert_eq!(tape.to_vec().unwrap().len(), before);
    let mut mixed = old.clone();
    mixed[1] = b"changed".to_vec();
    assert_eq!(put_group(&mut w, &mixed).unwrap(), vec![false, true, false]);
    let got: Vec<_> = in_force(&tape.to_vec().unwrap())
        .into_iter()
        .map(Option::unwrap)
        .collect();
    assert_eq!(got, mixed);
}

/// The writer refuses a group naming a track twice, one past [GROUP_BYTES_MAX] and one on a track that isn't a state
/// track, each before writing anything; an empty group writes nothing.
#[test]
fn a_group_the_writer_refuses_writes_nothing() {
    let mut w = rig();
    let tape = w.sink().tape();
    put_group(&mut w, &items(1, 8)).unwrap();
    let before = tape.to_vec().unwrap();
    let twice = [10, 11, 10].map(|t| GroupPut {
        track: TrackNum::new(t),
        ticks: 1,
        item: b"x",
    });
    assert!(matches!(
        w.w.put_group(&w.defs, &twice),
        Err(WriteError::Input(InputError::Group(GroupRefusal::SlotTwice { track }))) if track == TrackNum::new(10)
    ));
    let big = vec![0u8; GROUP_BYTES_MAX / 2 + 1];
    let long = [10, 11].map(|t| GroupPut {
        track: TrackNum::new(t),
        ticks: 1,
        item: &big,
    });
    assert!(matches!(
        w.w.put_group(&w.defs, &long),
        Err(WriteError::Input(InputError::Group(GroupRefusal::TooLong { len, max })))
            if len == GROUP_BYTES_MAX + 2 && max == GROUP_BYTES_MAX
    ));
    let events = [GroupPut {
        track: TrackNum::new(1),
        ticks: 1,
        item: b"x",
    }];
    assert!(matches!(
        w.w.put_group(&w.defs, &events),
        Err(WriteError::Describe(DescribeError::NotState { .. }))
    ));
    assert_eq!(w.w.put_group(&w.defs, &[]).unwrap(), Vec::<bool>::new());
    assert_eq!(tape.to_vec().unwrap(), before);
}

/// A chain whose piece has no room for a group's state entries cuts before it and puts it whole in the next piece.
#[test]
fn a_chain_cuts_before_a_group_never_inside() {
    let mem = ww_tape::MemPieces::new();
    let o = ChainOptions {
        cut: CutPolicy::default(),
        writer: ww_tape::WriterOptions {
            index: IndexPolicy {
                state_entries_max: exact_units::Count::new(24),
                ..IndexPolicy::NONE
            },
            ..options(u64::MAX)
        },
        tapes: PieceTapes::Each,
    };
    let mut chain =
        PieceChain::create_on(mem.clone(), ww_tape::OwnerId([0x3c; 32]), 0, &[], o).unwrap();
    let mut batch = crate::definitions::base(ww_tape::Layout::UNALIGNED)[2..].to_vec();
    batch.extend(TRACKS.map(state));
    let defs = held(&batch);
    let mut cut = false;
    for round in 0..12u8 {
        let group = items(round.wrapping_mul(17), 4);
        let puts: Vec<GroupPut<'_>> = TRACKS
            .iter()
            .zip(&group)
            .map(|(t, item)| GroupPut {
                track: TrackNum::new(*t),
                ticks: i64::from(round),
                item,
            })
            .collect();
        let seq = chain.seq();
        chain.put_group(&defs, &puts).unwrap();
        cut |= chain.seq() > seq;
    }
    assert!(cut, "the groups fill a piece");
    chain.close().unwrap();
    // every piece holds whole groups
    for key in mem.keys().unwrap() {
        let bytes = mem.get(&key).unwrap().unwrap().to_vec().unwrap();
        let r = TapeReader::open(&bytes[..]).unwrap();
        assert!(r.pieces().iter().all(|p| p.dropped_runs.is_empty()));
    }
}

/// Write `bytes` to the end of the file at `path`.
fn append(path: &Path, bytes: &[u8]) {
    let mut f = FileOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    f.write_all(bytes).unwrap();
}

/// A follower hands out none of a group while a crash or a writer leaves it torn, and all of it once its last block
/// is there, each block checked.
#[test]
fn a_follower_hands_out_a_group_whole() {
    let w = written();
    let dir = scratch("groups_follow");
    let path = dir.join("tape.wwt");
    let mid = w.start + (w.end - w.start) / 2;
    append(&path, &w.bytes[..mid]);
    let mut f = ChainFollower::new(
        &path,
        FollowOptions {
            tapes: PieceTapes::One,
            ..FollowOptions::default()
        },
    )
    .unwrap();
    let mut before = Vec::new();
    while let Some(b) = f.try_next().unwrap() {
        before.push(b.offset);
    }
    assert!(
        before.iter().all(|o| *o < w.start as u64),
        "no block of the torn group: {before:?}"
    );
    append(&path, &w.bytes[mid..]);
    let mut after = Vec::new();
    while let Some(b) = f.try_next().unwrap() {
        assert_eq!(
            b.block.bytes(),
            &w.bytes[b.offset as usize..b.offset as usize + b.block.bytes().len()]
        );
        after.push(b.offset);
    }
    assert_eq!(after.first().copied(), Some(w.start as u64));
    let last = after.last().copied().unwrap();
    assert!(last < w.end as u64 && after.len() >= 3);
}
