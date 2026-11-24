//! Groups of puts ([TapeWriter::put_group]): a reader and a resume take a group whole or not at all, wherever a crash
//! cuts it; a follower hands it out whole; a chain cuts before a group, never inside it; the writer refuses a group
//! it can't write whole before writing any of it.

use std::fs::OpenOptions as FileOptions;
use std::io::Write;
use std::path::Path;

use ww_tape::index::IndexPolicy;
use ww_tape::{
    Block, ChainOptions, CutPolicy, Describe, DescribeError, FLAG_GROUP, FLAG_GROUP_START,
    FLAG_TWICE, FLAG_WITH_NEXT, FileBlocks, GROUP_BYTES_MAX, GroupPut, GroupRefusal, InputError,
    MemSink, PieceChain, PieceTapes, RELAY_GROUPS_MAX, Recovery, SessionTag, Slot, TapeReader,
    TapeSink, TapeWriter, TrackDef, TrackNum, WriteError,
};
use ww_tape_fs::follow::{ChainFollower, FollowOptions};

use crate::definitions::{Rig, begin_numbered, held, options, record_blocks, state};
use crate::pieces::scratch;

/// The state tracks of the groups.
const TRACKS: [u32; 3] = [10, 11, 12];
/// Bytes of an item of the large group: two don't fit one block of puts, so the group takes three blocks.
const LARGE: usize = 40 << 10;

/// A writer with [TRACKS] defined.
fn rig() -> Rig {
    let states = TRACKS.map(state);
    let mut w = begin_numbered(&options(u64::MAX), ww_tape::Layout::UNALIGNED, &states);
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
            item: ww_tape::GroupItem::State(item),
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
        item: ww_tape::GroupItem::State(b"x"),
    });
    assert!(matches!(
        w.w.put_group(&w.defs, &twice),
        Err(WriteError::Input(InputError::Group(GroupRefusal::SlotTwice { track }))) if track == TrackNum::new(10)
    ));
    let big = vec![0u8; GROUP_BYTES_MAX / 2 + 1];
    let long = [10, 11].map(|t| GroupPut {
        track: TrackNum::new(t),
        ticks: 1,
        item: ww_tape::GroupItem::State(&big),
    });
    assert!(matches!(
        w.w.put_group(&w.defs, &long),
        Err(WriteError::Input(InputError::Group(GroupRefusal::TooLong { len, max })))
            if len == GROUP_BYTES_MAX + 2 && max == GROUP_BYTES_MAX
    ));
    let events = [GroupPut {
        track: TrackNum::new(1),
        ticks: 1,
        item: ww_tape::GroupItem::State(b"x"),
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
                item: ww_tape::GroupItem::State(item),
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

/// A closed piece on an SD card layout with a small group in force, then a large group of three blocks, then a block
/// of data: the bytes, the large group's blocks (offset and length) and the items of both groups.
struct Damaged {
    bytes: Vec<u8>,
    blocks: Vec<(usize, usize)>,
    old: Vec<Option<Vec<u8>>>,
}

fn closed_on_card() -> Damaged {
    let mut w = begin_numbered(
        &options(u64::MAX),
        ww_tape::Layout::UNALIGNED.on_sd_card(),
        &TRACKS.map(state),
    );
    for d in &TRACKS.map(state) {
        w.define(d).unwrap();
    }
    let tape = w.sink().tape();
    let old = items(1, 8);
    put_group(&mut w, &old).unwrap();
    let start = tape.to_vec().unwrap().len();
    put_group(&mut w, &items(100, LARGE)).unwrap();
    let end = tape.to_vec().unwrap().len();
    crate::definitions::write_records(&mut w, 50, 1);
    w.w.close().unwrap();
    let bytes = tape.to_vec().unwrap();
    let r = TapeReader::open(&bytes[..]).unwrap();
    let mut blocks: Vec<(usize, usize)> = r
        .entries(0)
        .unwrap()
        .iter()
        .map(|e| (e.offset as usize, e.len as usize))
        .filter(|(o, _)| *o >= start && *o < end)
        .collect();
    blocks.dedup();
    assert_eq!(blocks.len(), 3, "the large group takes three blocks");
    Damaged {
        bytes,
        blocks,
        old: old.into_iter().map(Some).collect(),
    }
}

/// `bytes` with the block from `at` of `len` bytes blanked, as an erased stretch of a card reads.
fn blanked(bytes: &[u8], (at, len): (usize, usize)) -> Vec<u8> {
    let mut b = bytes.to_vec();
    b[at..at + len].fill(0xFF);
    b
}

/// `bytes` with one byte flipped in the middle of the block from `at` of `len` bytes.
fn flipped(bytes: &[u8], (at, len): (usize, usize)) -> Vec<u8> {
    let mut b = bytes.to_vec();
    b[at + len / 2] ^= 0x40;
    b
}

/// The item in force at the end of piece 0 on each of [TRACKS] through `r`, and whether each lookup is whole.
fn in_force_of<S: ww_tape::ByteSource>(r: &TapeReader<S>) -> Vec<(Option<Vec<u8>>, bool)> {
    TRACKS
        .iter()
        .map(|t| {
            let l = r
                .state_at(
                    0,
                    Slot::State {
                        track: TrackNum::new(*t),
                    },
                    u64::MAX,
                )
                .unwrap();
            let whole = l.is_whole();
            (l.item.map(|i| i.item), whole)
        })
        .collect()
}

fn raw() -> ww_tape::OpenOptions {
    ww_tape::OpenOptions {
        medium: ww_tape::Medium::Raw,
        ..Default::default()
    }
}

/// A group with a block that doesn't read is dropped whole, by a lookup through the final index and by a scan alike:
/// the group before it stays in force on every track, and neither lookup says it is whole.
#[test]
fn a_damaged_block_drops_its_group_in_the_index_and_the_scan() {
    let d = closed_on_card();
    for k in 0..3 {
        let block = d.blocks[k];
        let bad = flipped(&d.bytes, block);
        let by_index = TapeReader::open(&bad[..]).unwrap();
        let got = in_force_of(&by_index);
        for (t, (item, whole)) in got.iter().enumerate() {
            assert_eq!(item, &d.old[t], "block {k} damaged: track {t} by the index");
            assert!(
                !whole,
                "block {k} damaged: track {t} by the index is not whole"
            );
        }
        let gap = blanked(&d.bytes, block);
        let by_scan = TapeReader::open_with(&gap[..], raw()).unwrap();
        let got = in_force_of(&by_scan);
        for (t, (item, _)) in got.iter().enumerate() {
            assert_eq!(item, &d.old[t], "block {k} blanked: track {t} by a scan");
        }
        let p = &by_scan.pieces()[0];
        assert_eq!(p.gaps.len(), 1, "block {k} blanked: {:?}", p.gaps);
        assert!(!p.dropped_runs.is_empty(), "block {k} blanked");
    }
}

/// Damage to a block of puts costs no block of data: a scan past it drops the puts and takes the block of data after
/// them.
#[test]
fn damaged_puts_cost_no_block_of_data() {
    let d = closed_on_card();
    let last = d.blocks[2];
    let gap = blanked(&d.bytes, last);
    let r = TapeReader::open_with(&gap[..], raw()).unwrap();
    let data_at = (last.0 + last.1) as u64;
    let entries = r.entries(0).unwrap();
    let data = entries
        .iter()
        .find(|e| e.offset >= data_at && e.kind == ww_tape::BlockKind::Records && !e.repeat)
        .expect("the block of data after the damaged puts is listed");
    let mut buf = Vec::new();
    assert!(r.read_block(0, data, &mut buf).is_ok());
    for e in &entries {
        let o = e.offset as usize;
        assert!(
            !d.blocks.iter().any(|b| b.0 == o),
            "no block of the damaged group is listed: {o}"
        );
    }
}

/// A piece closed with no data after its head: its head's puts, marked to go with the next block, end whole at the
/// close. The final index and a scan list the same blocks, the layout and the producer among them.
#[test]
fn a_piece_closed_without_data_keeps_its_head_puts_in_both_readers() {
    let mut w = TapeWriter::create_on(MemSink::new(), ww_tape::SyncPolicy::None);
    w.set_layout(ww_tape::Layout::UNALIGNED).unwrap();
    w.set_producer("groups");
    let owner = ww_tape::OwnerId([9; 32]);
    w.begin_piece(
        &ww_tape::PieceHead {
            ident: ww_tape::PieceIdent {
                id: ww_tape::PieceId::first(&owner),
                owner,
                seq: 0,
            },
            prev: None,
            prev_session: ww_tape::SessionTag::NONE,
            integrity: ww_tape::Integrity::Full,
            unlisted_keys_unknown: false,
        },
        &[],
    )
    .unwrap();
    let tape = w.sink().tape();
    w.close().unwrap();
    let bytes = tape.to_vec().unwrap();
    let by_index = TapeReader::open(&bytes[..]).unwrap();
    let by_scan = TapeReader::open_with(&bytes[..], raw()).unwrap();
    assert_eq!(by_scan.pieces()[0].dropped_runs, [] as [(u64, u64); 0]);
    assert_eq!(by_index.entries(0).unwrap(), by_scan.entries(0).unwrap());
    for r in [&by_index, &by_scan] {
        let defs = r.definitions_at(0, u64::MAX).unwrap();
        assert!(defs.items.contains_key(&Slot::Layout));
        assert!(defs.items.contains_key(&Slot::Producer));
    }
}

/// A group of one block that doesn't read is lost alone: the group after it, past a block of data, stays whole, and
/// before that group the one before the damaged one is in force. A scan and a lookup through the index agree.
#[test]
fn a_damaged_one_block_group_is_lost_alone() {
    let mut w = begin_numbered(
        &options(u64::MAX),
        ww_tape::Layout::UNALIGNED.on_sd_card(),
        &TRACKS.map(state),
    );
    for d in &TRACKS.map(state) {
        w.define(d).unwrap();
    }
    let tape = w.sink().tape();
    let [a, b, c] = [1, 2, 3].map(|seed| items(seed * 40, 8));
    put_group(&mut w, &a).unwrap();
    crate::definitions::write_records(&mut w, 10, 1);
    let b_at = tape.to_vec().unwrap().len();
    put_group(&mut w, &b).unwrap();
    let b_end = tape.to_vec().unwrap().len();
    crate::definitions::write_records(&mut w, 20, 1);
    let c_at = tape.to_vec().unwrap().len() as u64;
    put_group(&mut w, &c).unwrap();
    w.w.close().unwrap();
    let bytes = tape.to_vec().unwrap();
    let by_index = flipped(&bytes, (b_at, b_end - b_at));
    let by_scan = blanked(&bytes, (b_at, b_end - b_at));
    let readers = [
        TapeReader::open(&by_index[..]).unwrap(),
        TapeReader::open_with(&by_scan[..], raw()).unwrap(),
    ];
    for (n, r) in readers.iter().enumerate() {
        for (t, track) in TRACKS.iter().enumerate() {
            let slot = Slot::State {
                track: TrackNum::new(*track),
            };
            let end = r.state_at(0, slot, u64::MAX).unwrap();
            assert_eq!(end.item.map(|i| i.item), Some(c[t].clone()), "reader {n}");
            assert!(end.lost.is_empty(), "reader {n}: {:?}", end.lost);
            let before = r.state_at(0, slot, c_at).unwrap();
            assert_eq!(
                before.item.map(|i| i.item),
                Some(a[t].clone()),
                "reader {n}"
            );
        }
    }
}

/// A follower hands out no block of a group with a block that doesn't read, as a scan drops it, and goes on with the
/// blocks after it.
#[test]
fn a_follower_hands_out_no_block_of_a_damaged_group() {
    let d = closed_on_card();
    for k in 0..3 {
        let bad = blanked(&d.bytes, d.blocks[k]);
        let dir = scratch("groups_follow_damaged");
        let path = dir.join("tape.wwt");
        append(&path, &bad);
        let mut f = ChainFollower::new(
            &path,
            FollowOptions {
                tapes: PieceTapes::One,
                ..FollowOptions::default()
            },
        )
        .unwrap();
        let mut got = Vec::new();
        while let Some(b) = f.try_next().unwrap() {
            got.push(b.offset as usize);
        }
        std::fs::remove_dir_all(&dir).unwrap();
        for (at, _) in &d.blocks {
            assert!(
                !got.contains(at),
                "block {k} blanked: the group's block at {at} handed out"
            );
        }
        let end = d.blocks[2].0 + d.blocks[2].1;
        assert!(
            got.iter().any(|o| *o >= end),
            "block {k} blanked: the blocks after it"
        );
    }
}

/// A closed piece on an SD card layout: group A, a block of data, group C of `c_len`-byte items, a block of data.
/// The bytes, the block of data between A and C, C's blocks, and the items of A and C.
struct AroundData {
    bytes: Vec<u8>,
    data: (usize, usize),
    c: Vec<(usize, usize)>,
    a: Vec<Vec<u8>>,
    c_items: Vec<Vec<u8>>,
}

fn around_data(c_len: usize) -> AroundData {
    let mut w = begin_numbered(
        &options(u64::MAX),
        ww_tape::Layout::UNALIGNED.on_sd_card(),
        &TRACKS.map(state),
    );
    for d in &TRACKS.map(state) {
        w.define(d).unwrap();
    }
    let tape = w.sink().tape();
    let a = items(1, 8);
    put_group(&mut w, &a).unwrap();
    let data_at = tape.to_vec().unwrap().len();
    crate::definitions::write_records(&mut w, 10, 1);
    let c_at = tape.to_vec().unwrap().len();
    let c_items = items(60, c_len);
    put_group(&mut w, &c_items).unwrap();
    let c_end = tape.to_vec().unwrap().len();
    crate::definitions::write_records(&mut w, 20, 1);
    w.w.close().unwrap();
    let bytes = tape.to_vec().unwrap();
    let r = TapeReader::open(&bytes[..]).unwrap();
    let mut c: Vec<(usize, usize)> = r
        .entries(0)
        .unwrap()
        .iter()
        .map(|e| (e.offset as usize, e.len as usize))
        .filter(|(o, _)| *o >= c_at && *o < c_end)
        .collect();
    c.dedup();
    AroundData {
        bytes,
        data: (data_at, c_at - data_at),
        c,
        a,
        c_items,
    }
}

/// What both readers hold in force at the end of the bytes, `flipped` read through the final index and `gap` by a
/// raw scan; asserted equal to `want` on every track.
fn both_hold(flipped_bytes: &[u8], gap: &[u8], want: &[Vec<u8>], what: &str) {
    let readers = [
        TapeReader::open(flipped_bytes).unwrap(),
        TapeReader::open_with(gap, raw()).unwrap(),
    ];
    for (n, r) in readers.iter().enumerate() {
        for (t, (item, _)) in in_force_of(r).into_iter().enumerate() {
            assert_eq!(
                item.as_ref(),
                Some(&want[t]),
                "{what}: reader {n}, track {t}"
            );
        }
    }
}

/// Damage to a block of data before a group costs the group nothing: it starts after the damage, with its start
/// mark, so a scan and a lookup through the index both hold it whole, a group of one block or of three.
#[test]
fn a_group_that_starts_after_damage_is_whole() {
    for c_len in [8, LARGE] {
        let d = around_data(c_len);
        assert_eq!(d.c.len(), if c_len == 8 { 1 } else { 3 });
        both_hold(
            &flipped(&d.bytes, d.data),
            &blanked(&d.bytes, d.data),
            &d.c_items,
            &format!("data damaged, C of {c_len}"),
        );
        let got = followed(&blanked(&d.bytes, d.data));
        for (at, _) in &d.c {
            assert!(
                got.contains(at),
                "data damaged, C of {c_len}: the follower hands out C's block at {at}"
            );
        }
    }
}

/// The offsets of every block a follower hands out of a file holding `bytes`.
fn followed(bytes: &[u8]) -> Vec<usize> {
    let dir = scratch("groups_followed");
    let path = dir.join("tape.wwt");
    append(&path, bytes);
    let mut f = ChainFollower::new(
        &path,
        FollowOptions {
            tapes: PieceTapes::One,
            ..FollowOptions::default()
        },
    )
    .unwrap();
    let mut got = Vec::new();
    while let Some(b) = f.try_next().unwrap() {
        got.push(b.offset as usize);
    }
    std::fs::remove_dir_all(&dir).unwrap();
    got
}

/// Damage to the first block of a group drops the whole group in both readers: the rest of it carries no start
/// mark, so the group before it stays in force.
#[test]
fn a_group_whose_first_block_is_damaged_is_dropped_whole() {
    let d = around_data(LARGE);
    both_hold(
        &flipped(&d.bytes, d.c[0]),
        &blanked(&d.bytes, d.c[0]),
        &d.a,
        "C's first block damaged",
    );
    let one = around_data(8);
    both_hold(
        &flipped(&one.bytes, one.c[0]),
        &blanked(&one.bytes, one.c[0]),
        &one.a,
        "C of one block damaged",
    );
}

/// The blocks of the large group of [written], as a producer sends them.
fn large_blocks(w: &Written) -> Vec<Vec<u8>> {
    record_blocks(&w.bytes)
        .into_iter()
        .filter(|b| b.repeat && b.offset >= w.start as u64 && b.offset < w.end as u64)
        .map(|b| b.bytes)
        .collect()
}

/// A relay holds a producer's group until its last block, then writes it whole as a group: a crash of the relay
/// between the group's blocks leaves none of it in force. Blocks of a group whose first block the relay never saw
/// are refused and passed over.
#[test]
fn a_relay_writes_a_producer_group_whole() {
    let w = written();
    let blocks = large_blocks(&w);
    assert_eq!(blocks.len(), 3, "the large group takes three blocks");
    let none = vec![None; TRACKS.len()];
    let new: Vec<_> = w.new.iter().cloned().map(Some).collect();

    let mut relay = rig();
    let tape = relay.sink().tape();
    let start = tape.to_vec().unwrap().len();
    let mut defs = relay.defs.clone();
    for (k, b) in blocks.iter().enumerate() {
        relay.w.relay_block(&mut defs, b).unwrap();
        let want = if k + 1 == blocks.len() { &new } else { &none };
        assert_eq!(
            &in_force(&tape.to_vec().unwrap()),
            want,
            "a crash after block {k}"
        );
    }
    let bytes = tape.to_vec().unwrap();
    let flags: Vec<u16> = record_blocks(&bytes)
        .into_iter()
        .filter(|b| b.repeat && b.offset >= start as u64)
        .map(|b| Block::read(&b.bytes).unwrap().flags)
        .collect();
    assert_ne!(flags, [] as [u16; 0]);
    for (k, f) in flags.iter().enumerate() {
        assert_ne!(
            f & FLAG_GROUP,
            0,
            "block {k} of the relayed group is marked a group's"
        );
        assert_eq!(
            f & FLAG_GROUP_START != 0,
            k == 0,
            "block {k}: only the first starts the group"
        );
        assert_eq!(
            f & FLAG_WITH_NEXT == 0,
            k + 1 == flags.len(),
            "block {k}: only the last is unmarked"
        );
    }

    let mut late = rig();
    let tape = late.sink().tape();
    let mut defs = late.defs.clone();
    for b in &blocks[1..] {
        assert!(
            matches!(
                late.w.relay_block(&mut defs, b),
                Err(WriteError::Input(InputError::Group(
                    GroupRefusal::NoStart { .. }
                )))
            ),
            "the rest of a group is refused"
        );
    }
    assert_eq!(
        in_force(&tape.to_vec().unwrap()),
        none,
        "the rest of a group is passed over"
    );
}

/// A cut of a relay's chain between a producer group's blocks carries the group held to the next piece, which
/// holds the group whole; the piece before holds none of it.
#[test]
fn a_relayed_group_held_over_a_cut_is_whole_in_the_next_piece() {
    let w = written();
    let blocks = large_blocks(&w);
    let mem = ww_tape::MemPieces::new();
    let o = ChainOptions {
        cut: CutPolicy::default(),
        writer: options(u64::MAX),
        tapes: PieceTapes::Each,
    };
    let mut chain =
        PieceChain::create_on(mem.clone(), ww_tape::OwnerId([0x3d; 32]), 0, &[], o).unwrap();
    let mut batch = crate::definitions::base(ww_tape::Layout::UNALIGNED)[2..].to_vec();
    batch.extend(TRACKS.map(state));
    let mut defs = held(&batch);
    chain.relay_block(&mut defs, &blocks[0]).unwrap();
    chain.cut_now(&defs).unwrap();
    for b in &blocks[1..] {
        chain.relay_block(&mut defs, b).unwrap();
    }
    chain.close().unwrap();
    let pieces: Vec<Vec<Option<Vec<u8>>>> = mem
        .keys()
        .unwrap()
        .iter()
        .map(|k| in_force(&mem.get(k).unwrap().unwrap().to_vec().unwrap()))
        .collect();
    let new: Vec<_> = w.new.iter().cloned().map(Some).collect();
    assert_eq!(pieces, vec![vec![None; TRACKS.len()], new]);
}

/// A piece whose first group, of 8-byte items on [TRACKS] defined twice at once, writes their definitions; closed on
/// an SD card layout after a block of data when `closed`, else left open on the layout a resume takes ([options]).
struct TwiceGroup {
    bytes: Vec<u8>,
    /// The group's blocks, then the copy after it: offset, length, header flags
    blocks: Vec<(usize, usize, u16)>,
    items: Vec<Vec<u8>>,
}

/// A writer on `layout` with [TRACKS] defined, each defined twice at once.
fn twice_rig(layout: ww_tape::Layout) -> Rig {
    let tracks = TRACKS.map(|t| {
        let Describe::Track(d) = state(t) else {
            unreachable!("a track")
        };
        Describe::Track(TrackDef {
            defined_twice: true,
            ..d
        })
    });
    let mut w = begin_numbered(&options(u64::MAX), layout, &tracks);
    for d in &tracks {
        w.define(d).unwrap();
    }
    w
}

fn twice_group(closed: bool) -> TwiceGroup {
    let mut w = twice_rig(if closed {
        ww_tape::Layout::UNALIGNED.on_sd_card()
    } else {
        ww_tape::Layout::UNALIGNED
    });
    let tape = w.sink().tape();
    let items = items(1, 8);
    assert_eq!(put_group(&mut w, &items).unwrap(), vec![true; 3]);
    if closed {
        crate::definitions::write_records(&mut w, 50, 1);
        w.w.close().unwrap();
    }
    let bytes = tape.to_vec().unwrap();
    let mut rd = FileBlocks::with_recovery(&bytes[..], Recovery::Prefix);
    let mut blocks = Vec::new();
    while let Some(Ok(b)) = rd.next_block() {
        let (flags, len) = (b.flags, b.bytes().len());
        if flags & (FLAG_GROUP | FLAG_TWICE) != 0 {
            blocks.push((rd.offset() as usize, len, flags));
        }
    }
    TwiceGroup {
        bytes,
        blocks,
        items,
    }
}

/// A group on tracks defined twice at once opens with its block marked [FLAG_TWICE]: that block is the group's
/// first, and the copy after the group's last block is none of the group's. A crash anywhere inside the group leaves
/// none of it, and the group given again after a resume writes the same bytes; a crash before the copy keeps it.
#[test]
fn a_group_on_tracks_defined_twice_holds_its_twice_block() {
    let g = twice_group(false);
    let r = ww_tape::FLAG_REPEAT;
    assert_eq!(
        g.blocks.iter().map(|b| b.2).collect::<Vec<_>>(),
        vec![
            r | FLAG_TWICE | FLAG_GROUP | FLAG_GROUP_START | FLAG_WITH_NEXT,
            r | FLAG_GROUP,
            r | FLAG_TWICE,
        ],
        "the twice block opens the group, its copy follows it unmarked"
    );
    let (start, end) = (g.blocks[0].0, g.blocks[1].0 + g.blocks[1].1);
    let whole: Vec<_> = g.items.iter().cloned().map(Some).collect();
    let none = vec![None; TRACKS.len()];
    let defs = twice_rig(ww_tape::Layout::UNALIGNED).defs;
    let mut at: Vec<usize> = g
        .blocks
        .iter()
        .flat_map(|(o, l, _)| [*o, o + 1, o + l - 1, o + l])
        .collect();
    at.dedup();
    for cut in at {
        let want = if cut >= end { &whole } else { &none };
        assert_eq!(&in_force(&g.bytes[..cut]), want, "cut at {cut}");
        if cut == g.bytes.len() {
            continue;
        }
        let mut sink = MemSink::new();
        sink.write_all(&g.bytes[..cut]).unwrap();
        let tape = sink.tape();
        let (resumed, r) = TapeWriter::resume_on(sink, options(u64::MAX)).unwrap();
        if cut < end {
            assert!(r.kept_bytes <= start as u64, "cut at {cut}");
        }
        let mut again = Rig {
            w: resumed,
            defs: defs.clone(),
        };
        put_group(&mut again, &g.items).unwrap();
        assert_eq!(tape.to_vec().unwrap(), g.bytes, "cut at {cut}");
    }
}

/// Damage to a block of a group on tracks defined twice at once, its twice block included, drops the group in the
/// index, a scan and a follower alike; damage to the copy after it leaves the group whole in all three.
#[test]
fn a_group_on_tracks_defined_twice_is_dropped_or_kept_at_each_block() {
    let g = twice_group(true);
    assert_eq!(g.blocks.len(), 3, "two blocks of the group and the copy");
    for (k, (at, len, _)) in g.blocks.iter().enumerate() {
        let block = (*at, *len);
        let copy = k == 2;
        let want = |item: Option<Vec<u8>>, t: usize| {
            if copy {
                assert_eq!(
                    item.as_ref(),
                    Some(&g.items[t]),
                    "block {k} damaged: track {t}"
                );
            } else {
                assert_eq!(item, None, "block {k} damaged: track {t}");
            }
        };
        let bad = flipped(&g.bytes, block);
        for (t, (item, _)) in in_force_of(&TapeReader::open(&bad[..]).unwrap())
            .into_iter()
            .enumerate()
        {
            want(item, t);
        }
        let gap = blanked(&g.bytes, block);
        for (t, (item, _)) in in_force_of(&TapeReader::open_with(&gap[..], raw()).unwrap())
            .into_iter()
            .enumerate()
        {
            want(item, t);
        }
        let got = followed(&gap);
        for (o, _, _) in &g.blocks[..2] {
            assert_eq!(
                got.contains(o),
                copy,
                "block {k} damaged: the follower and the group's block at {o}"
            );
        }
    }
}

/// `block` as producer `session` sends it: the same seq, its session tag `session`.
fn as_producer(block: &[u8], session: u32) -> Vec<u8> {
    let mut b = block.to_vec();
    let seq = Block::read(&b).unwrap().seq;
    ww_tape::set_id(
        &mut b,
        ww_tape::BlockId::new(SessionTag::from_u32(session), seq),
    )
    .unwrap();
    b
}

/// The blocks of puts a producer of session `session` sends after its definitions: a group of [LARGE]-byte items
/// made from `seed` on [TRACKS], in three blocks, then a put of `single` on track [TRACKS]\[0], none of a group.
/// The group's items too.
struct Producer {
    group: Vec<Vec<u8>>,
    single: Vec<Vec<u8>>,
    items: Vec<Vec<u8>>,
}

fn producer(seed: u8, single: &[u8], session: u32) -> Producer {
    let mut w = rig();
    let tape = w.sink().tape();
    put_group(&mut w, &items(1, 8)).unwrap();
    let start = tape.to_vec().unwrap().len() as u64;
    let items = items(seed, LARGE);
    put_group(&mut w, &items).unwrap();
    let end = tape.to_vec().unwrap().len() as u64;
    assert!(
        w.w.put(&w.defs, TrackNum::new(TRACKS[0]), 9, single)
            .unwrap()
    );
    let blocks: Vec<_> = record_blocks(&tape.to_vec().unwrap())
        .into_iter()
        .filter(|b| b.repeat && b.offset >= start)
        .collect();
    let of = |keep: &dyn Fn(u64) -> bool| -> Vec<Vec<u8>> {
        blocks
            .iter()
            .filter(|b| keep(b.offset))
            .map(|b| as_producer(&b.bytes, session))
            .collect()
    };
    let group = of(&|o| o < end);
    assert_eq!(group.len(), 3, "the group takes three blocks");
    Producer {
        group,
        single: of(&|o| o >= end),
        items,
    }
}

/// A relay holds a group per producer: another producer's put between the group's blocks goes through, and the
/// group is whole at its last block.
#[test]
fn a_relayed_group_stays_whole_past_another_producers_put() {
    let a = producer(100, b"a", 0x100);
    let b = producer(150, b"between", 0x200);
    let mut relay = rig();
    let tape = relay.sink().tape();
    let mut defs = relay.defs.clone();
    relay.w.relay_block(&mut defs, &a.group[0]).unwrap();
    for s in &b.single {
        relay.w.relay_block(&mut defs, s).unwrap();
    }
    let mut between = vec![None; TRACKS.len()];
    between[0] = Some(b"between".to_vec());
    assert_eq!(
        in_force(&tape.to_vec().unwrap()),
        between,
        "B's put went through"
    );
    for g in &a.group[1..] {
        relay.w.relay_block(&mut defs, g).unwrap();
    }
    let whole: Vec<_> = a.items.iter().cloned().map(Some).collect();
    assert_eq!(in_force(&tape.to_vec().unwrap()), whole, "A's group whole");
}

/// Two producers' groups relayed block by block in turn are each written whole at their last block.
#[test]
fn two_relayed_groups_interleaved_are_each_whole() {
    let a = producer(100, b"a", 0x100);
    let b = producer(150, b"b", 0x200);
    let mut relay = rig();
    let tape = relay.sink().tape();
    let mut defs = relay.defs.clone();
    let none = vec![None; TRACKS.len()];
    for k in 0..2 {
        relay.w.relay_block(&mut defs, &a.group[k]).unwrap();
        relay.w.relay_block(&mut defs, &b.group[k]).unwrap();
        assert_eq!(in_force(&tape.to_vec().unwrap()), none, "after block {k}");
    }
    relay.w.relay_block(&mut defs, &a.group[2]).unwrap();
    let of = |p: &Producer| -> Vec<_> { p.items.iter().cloned().map(Some).collect() };
    assert_eq!(in_force(&tape.to_vec().unwrap()), of(&a), "A whole");
    relay.w.relay_block(&mut defs, &b.group[2]).unwrap();
    assert_eq!(in_force(&tape.to_vec().unwrap()), of(&b), "B whole after A");
}

/// A relay never drops a group without saying so: groups of more than [RELAY_GROUPS_MAX] producers at once are
/// refused, a block of a group not held is refused, and a producer's own put inside its group tears it with an error
/// after the put.
#[test]
fn a_relay_refuses_what_it_cannot_hold_and_names_a_torn_group() {
    let a = producer(100, b"single", 0x100);
    let mut relay = rig();
    let tape = relay.sink().tape();
    let mut defs = relay.defs.clone();
    for p in 0..RELAY_GROUPS_MAX as u32 {
        let first = as_producer(&a.group[0], 0x1000 + p);
        relay.w.relay_block(&mut defs, &first).unwrap();
    }
    let late = 0x1000 + RELAY_GROUPS_MAX as u32;
    let session = SessionTag::from_u32(late);
    assert!(matches!(
        relay.w.relay_block(&mut defs, &as_producer(&a.group[0], late)),
        Err(WriteError::Input(InputError::Group(GroupRefusal::TooManyHeld { session: s, max })))
            if s == session && max == RELAY_GROUPS_MAX
    ));
    assert!(matches!(
        relay.w.relay_block(&mut defs, &as_producer(&a.group[1], late)),
        Err(WriteError::Input(InputError::Group(GroupRefusal::NoStart { session: s }))) if s == session
    ));

    let first = SessionTag::from_u32(0x1000);
    let single = as_producer(&a.single[0], 0x1000);
    assert!(matches!(
        relay.w.relay_block(&mut defs, &single),
        Err(WriteError::Input(InputError::Group(GroupRefusal::Torn { session: s, blocks: 1 }))) if s == first
    ));
    let mut put = vec![None; TRACKS.len()];
    put[0] = Some(b"single".to_vec());
    assert_eq!(
        in_force(&tape.to_vec().unwrap()),
        put,
        "the put that tore the group is written"
    );
    assert!(matches!(
        relay.w.relay_block(&mut defs, &as_producer(&a.group[1], 0x1000)),
        Err(WriteError::Input(InputError::Group(GroupRefusal::NoStart { session: s }))) if s == first
    ));
}

/// `block` with header flags `flags`, its CRCs written again.
fn with_flags(block: &[u8], flags: u16) -> Vec<u8> {
    let mut b = block.to_vec();
    Block::read(&b).unwrap();
    b[6..8].copy_from_slice(&flags.to_le_bytes());
    let n = b.len() - ww_tape::CRC_LEN;
    let crc = crc::Crc::<u32>::new(&crc::CRC_32_ISCSI).checksum(&b[..n]);
    b[n..].copy_from_slice(&crc.to_le_bytes());
    Block::read(&b).unwrap();
    b
}

/// The blocks of puts of `items`, each put on track `track` after the tracks' definitions, as a producer sends them.
fn puts_on(track: u32, items: &[&[u8]]) -> Vec<Vec<u8>> {
    let mut w = rig();
    let tape = w.sink().tape();
    put_group(&mut w, &self::items(1, 8)).unwrap();
    let start = tape.to_vec().unwrap().len() as u64;
    for (k, item) in items.iter().enumerate() {
        assert!(
            w.w.put(&w.defs, TrackNum::new(track), k as i64 + 20, item)
                .unwrap()
        );
    }
    record_blocks(&tape.to_vec().unwrap())
        .into_iter()
        .filter(|b| b.repeat && b.offset >= start)
        .map(|b| b.bytes)
        .collect()
}

/// `blocks` marked as one group of puts: each [FLAG_GROUP], the first [FLAG_GROUP_START], all but the last
/// [FLAG_WITH_NEXT].
fn as_group(blocks: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let last = blocks.len() - 1;
    blocks
        .iter()
        .enumerate()
        .map(|(k, b)| {
            let mut f = ww_tape::FLAG_REPEAT | FLAG_GROUP;
            if k == 0 {
                f |= FLAG_GROUP_START;
            }
            if k < last {
                f |= FLAG_WITH_NEXT;
            }
            with_flags(b, f)
        })
        .collect()
}

/// A relayed group that puts a state track twice is refused as [Self::put_group] refuses one, the same item again
/// or another: a hostile producer can't fill a group with tiny items on one track. None of it is written.
#[test]
fn a_relayed_group_putting_a_track_twice_is_refused() {
    let other = puts_on(TRACKS[0], &[b"x", b"y"]);
    assert_eq!(other.len(), 2, "one block per put");
    let same = vec![other[0].clone(), other[0].clone()];
    for (what, blocks) in [("another item", other), ("the same item", same)] {
        let group = as_group(&blocks);
        let mut relay = rig();
        let tape = relay.sink().tape();
        let mut defs = relay.defs.clone();
        relay.w.relay_block(&mut defs, &group[0]).unwrap();
        assert!(matches!(
            relay.w.relay_block(&mut defs, &group[1]),
            Err(WriteError::Input(InputError::Group(GroupRefusal::SlotTwice { track })))
                if track == TrackNum::new(TRACKS[0])
        ));
        assert_eq!(relay.w.relay_held_bytes(), 0, "the group is dropped");
        assert_eq!(
            in_force(&tape.to_vec().unwrap()),
            vec![None; TRACKS.len()],
            "{what}: none of the group"
        );
    }
}

/// A producer's first block of a new group tears the group held of it; when that block is refused too, the error
/// names the torn group, and the block given again names its own refusal.
#[test]
fn a_refused_block_that_tears_a_group_names_the_torn_group() {
    let a = producer(100, b"a", 0x100);
    // a group of one block on track 13, a track the relay doesn't hold
    let mut w = begin_numbered(&options(u64::MAX), ww_tape::Layout::UNALIGNED, &[state(13)]);
    w.define(&state(13)).unwrap();
    let tape = w.sink().tape();
    w.w.put(&w.defs, TrackNum::new(13), 1, b"other").unwrap();
    let start = tape.to_vec().unwrap().len() as u64;
    w.w.put(&w.defs, TrackNum::new(13), 2, b"unknown").unwrap();
    let unknown: Vec<_> = record_blocks(&tape.to_vec().unwrap())
        .into_iter()
        .filter(|b| b.repeat && b.offset >= start)
        .map(|b| as_producer(&b.bytes, 0x100))
        .collect();
    let unknown = as_group(&unknown);
    assert_eq!(unknown.len(), 1);

    let mut relay = rig();
    let mut defs = relay.defs.clone();
    relay.w.relay_block(&mut defs, &a.group[0]).unwrap();
    assert!(matches!(
        relay.w.put_block(&defs, &unknown[0]),
        Err(WriteError::Input(InputError::Group(GroupRefusal::Torn {
            blocks: 1,
            ..
        })))
    ));
    let again = relay.w.put_block(&defs, &unknown[0]);
    assert!(
        matches!(again, Err(WriteError::Describe(_))),
        "its own refusal: {again:?}"
    );
}
