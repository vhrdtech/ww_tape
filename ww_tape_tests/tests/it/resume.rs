//! Resume after a crash, memory sinks and PieceChain on any sink, through the public API.

use ww_tape::ItemNum;

use std::collections::BTreeMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use ww_tape::TrackId;
use ww_tape::index::RECORDS_TRACK_ID;
use ww_tape::{ClockNum, RecordTrack, TrackNum};

use crate::definitions::{CLOCK, base_defs, clock, state, track, tracks};
use crate::id;
use crate::pieces::{chunk, chunk_id, column, records, scratch};
use shrink_wrap::prelude::*;
use ww_tape::index::IndexPolicy;
use ww_tape::piece::PieceId;
use ww_tape::times::{EncodedRelation, EncodedTimes};
use ww_tape::writer::NearPolicy;
use ww_tape::{
    ALIGNMENT, BlockKind, ChainOptions, ChunkHead, ColumnSpec, CutPolicy, Definitions, Describe,
    FileBlocks, InputError, Layout, MemError, MemPieces, MemSink, OwnerId, PieceChain, PieceHead,
    PieceIdent, PieceStore, Recovery, RelationDef, RelationIndex, RelationValues, ResumeError,
    SessionTag, Slot, StatePolicy, SyncPolicy, TapeReader, TapeSink, TapeWriter, TrackDef,
    WriteError, WriterOptions,
};
use ww_tape_fs::{FilePieces, FsError, piece_name};

/// One call a test makes: on a writer (each but a put places one counted block) or on the definitions.
#[derive(Clone, Copy, Debug)]
enum Op {
    /// Hold definition `i` of those added mid-piece.
    Add(usize),
    /// A records block with tick `n`.
    Records(i64),
    /// A records block with tick `n`, deferred.
    Defer(i64),
    /// Track `t`'s chunk `k`.
    Chunk(u64, u64),
    /// Put item `k` on state track `t`.
    Put(u32, u64),
    /// Chunk `k` of track 13, whose range relation's range goes into the chunk's entry.
    Ranged(u64),
}

/// The calls of a recording: `rounds` rounds of two chunks, a records block and a deferred one, definitions added
/// after half of them, and puts on the two state tracks and chunks of the ranged track they hold from then on: with
/// the writer's memory of three slots, puts of items the memory forgot.
fn ops(rounds: u64) -> Vec<Op> {
    let mut out = vec![];
    for k in 0..rounds {
        if k == rounds / 2 {
            out.extend((0..added().len()).map(Op::Add));
        }
        if k >= rounds / 2 {
            out.extend([Op::Put(11, k % 3), Op::Put(12, k % 2)]);
        }
        out.extend([Op::Chunk(0, k), Op::Records(k as i64)]);
        if k >= rounds / 2 {
            out.extend([Op::Put(11, (k + 1) % 3), Op::Ranged(k)]);
        }
        out.extend([Op::Chunk(1, k), Op::Defer(1000 + k as i64)]);
    }
    out
}

/// Track 0's policy in the tests: a definition is put again 3 KiB on, with two copies.
const DEFINITIONS: StatePolicy = StatePolicy {
    near: exact_units::Bytes::new(3 << 10),
    copies: exact_units::Count::new(2),
};

/// The definitions added mid-piece: clock 2, track 10 on it, state tracks 11 and 12, ranged track 13.
fn added() -> Vec<Describe<'static>> {
    vec![clock(2), track(10, 2), state(11), state(12), ranged()]
}

/// Track 13's relation: its values never decrease and feed the range index.
const SOURCE: &[RelationDef<'static>] = &[RelationDef {
    name: "source",
    target: None,
    values: RelationValues::Stored,
    monotone: true,
    index: RelationIndex::Range,
}];

/// Events track 13 on [CLOCK] with the relation [SOURCE].
fn ranged() -> Describe<'static> {
    let Describe::Track(t) = track(13, CLOCK) else {
        unreachable!("track() gives a track")
    };
    Describe::Track(TrackDef {
        relations: RefVec::Slice { slice: SOURCE },
        ..t
    })
}

/// Track 13's chunk `k`: 20 items, their sources from `7k` on; its head and columns.
fn ranged_chunk(k: u64) -> (ChunkHead, Vec<(ColumnSpec, Vec<u8>)>) {
    let starts: Vec<i64> = (0..20).map(|i| (k * 20 + i) as i64 * 10).collect();
    let sources: Vec<u64> = (0..20).map(|i| 7 * k + i / 3).collect();
    let times = EncodedTimes::encode(&starts, None, None).unwrap();
    let source = EncodedRelation::encode(0, &sources, true).unwrap();
    let mut cols: Vec<_> = times.columns().map(|(s, b)| (s, b.to_vec())).collect();
    let (s, b) = source.column();
    cols.push((s, b.to_vec()));
    (
        times.head.chunk_head(
            crate::definitions::track_id(13),
            TrackNum::new(13),
            ItemNum::new(k * 20),
        ),
        cols,
    )
}

/// Make the call `op` on `w` with the definitions `defs`.
fn apply<S: TapeSink>(w: &mut TapeWriter<S>, defs: &mut Definitions, op: Op) {
    match op {
        Op::Add(i) => crate::definitions::hold(defs, core::slice::from_ref(&added()[i])).unwrap(),
        Op::Records(n) => {
            w.write_block(defs, &records(w.next_id(), n)).unwrap();
        }
        Op::Defer(n) => w.defer_block(defs, records(id(0), n)).unwrap(),
        Op::Chunk(t, k) => {
            let (h, c) = chunk(t, k);
            w.write_chunk(defs, &h, &column(&c)).unwrap();
        }
        Op::Put(t, k) => {
            w.put(defs, TrackNum::new(t), k as i64, &[k as u8; 40])
                .unwrap();
        }
        Op::Ranged(k) => {
            let (h, cols) = ranged_chunk(k);
            let cols: Vec<_> = cols.iter().map(|(s, b)| (*s, &b[..])).collect();
            w.write_chunk(defs, &h, &cols).unwrap();
        }
    }
}

/// The options of the tests: checkpoints every 1 KiB, copies 2 KiB apart, a memory of three slots, so a piece holds
/// every kind of block a writer adds and the writer forgets slots on the way.
pub(crate) fn options(alignment: u32) -> WriterOptions {
    WriterOptions {
        sync: SyncPolicy::None,
        clock: ww_tape::ClockRef::NONE,
        index: IndexPolicy {
            bytes: exact_units::Bytes::new(1024),
            interval: None,
            ..IndexPolicy::default()
        },
        layout: if alignment == 0 {
            Layout::UNALIGNED
        } else {
            Layout::ALIGNED
        }
        .with_definitions(DEFINITIONS),
        producer: "resume".to_owned(),
        near: NearPolicy {
            copy_spacing: exact_units::Bytes::new(2 << 10),
            memory_max: exact_units::Count::new(3),
            ..NearPolicy::default()
        },
        integrity: ww_tape::Integrity::Full,
    }
}

pub(crate) fn head() -> PieceHead {
    head_at(ww_tape::Integrity::Full)
}

/// [head] of a piece of `level`.
pub(crate) fn head_at(integrity: ww_tape::Integrity) -> PieceHead {
    let owner = OwnerId([3; 32]);
    PieceHead {
        ident: PieceIdent {
            id: PieceId::first(&owner),
            owner,
            seq: 0,
        },
        prev: None,
        prev_session: SessionTag::NONE,
        integrity,
        unlisted_keys_unknown: false,
    }
}

/// A first piece of `ops` on a memory sink with `options`, begun but given no call yet.
pub(crate) fn begin(alignment: u32) -> TapeWriter<MemSink> {
    begin_at(alignment, ww_tape::Integrity::Full)
}

/// [begin] with a piece of `level`.
pub(crate) fn begin_at(alignment: u32, level: ww_tape::Integrity) -> TapeWriter<MemSink> {
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    let o = options(alignment);
    w.set_layout(o.layout).unwrap();
    w.set_producer(&o.producer);
    w.set_index_policy(o.index)
        .expect("a policy with room for a piece head");
    w.set_near_policy(o.near);
    w.begin_piece(&head_at(level), &[]).unwrap();
    w
}

/// The seq of each record block by its tick in `bytes`, the first records of each.
fn record_seqs(bytes: &[u8]) -> BTreeMap<i64, u64> {
    let mut rd = FileBlocks::with_recovery(bytes, Recovery::Prefix);
    let mut out = BTreeMap::new();
    while let Some(Ok(b)) = rd.next_block() {
        if b.kind == BlockKind::Records
            && !b.is_repeat()
            && b.first_track() != Some(RecordTrack::Definitions)
        {
            let at = b.records().next().unwrap().unwrap().at.unwrap();
            out.insert(at.ticks, b.seq);
        }
    }
    out
}

/// A resumed piece's numbers ([ww_tape::Resumed::numbers]) give the tracks and clock an application defines again
/// their numbers in any order; a new track takes a number none of them has, and data on them writes on.
#[test]
fn a_resume_gives_tracks_defined_in_another_order_their_numbers() {
    let batch = [
        clock(CLOCK),
        track(1, CLOCK),
        track(2, CLOCK),
        track(3, CLOCK),
    ];
    let mut defs = Definitions::new();
    crate::definitions::hold(&mut defs, &batch).unwrap();
    let mut w = begin(0);
    let tape = w.sink().tape();
    for n in 1..=3u32 {
        let block = crate::definitions::on_tracks(w.next_id(), &[(n, i64::from(n), b"x")]);
        w.write_block(&defs, &block).unwrap();
    }
    // a crash: the piece is left unclosed
    drop(w);
    let mut sink = MemSink::new();
    sink.write_all(&tape.to_vec().unwrap()).unwrap();
    let (mut w, resumed) = TapeWriter::resume_on(sink, options(0)).unwrap();
    assert_eq!(&resumed.numbers, defs.numbers());
    let mut again = Definitions::with_numbers(resumed.numbers.clone());
    // the clock first, as its tracks need it held; the tracks last to first
    for d in batch.iter().take(1).chain(batch[1..].iter().rev()) {
        match d {
            Describe::Clock(c) => assert_eq!(again.define_clock(c.spec()), Ok(c.number)),
            Describe::Track(t) => assert_eq!(again.define_track(t.spec()), Ok(t.number)),
            _ => unreachable!("a clock or a track"),
        }
    }
    let Describe::Track(new) = track(9, CLOCK) else {
        unreachable!("track() gives a track")
    };
    assert_eq!(again.define_track(new.spec()), Ok(TrackNum::new(4)));
    let tape = w.sink().tape();
    for n in [3, 4] {
        let block = crate::definitions::on_tracks(w.next_id(), &[(n, 10, b"y")]);
        w.write_block(&again, &block).unwrap();
    }
    w.close().unwrap();
    let bytes = tape.to_vec().unwrap();
    let r = TapeReader::open(&bytes[..]).unwrap();
    assert_eq!(
        tracks(&r.definitions_at(0, u64::MAX).unwrap()),
        [1, 2, 3, 4]
    );
}

/// Where each call of a run was made: the seq of the block it placed (a put: of the last block before it), the
/// bytes of its piece before and after it (a put places deferred blocks before its own) and its piece.
#[derive(Clone, Debug)]
struct Made {
    seqs: Vec<u64>,
    before: Vec<u64>,
    after: Vec<u64>,
    pieces: Vec<u64>,
}

/// Whether call `op`, made where `made` says at `i`, is to be given again to a writer resumed in piece `piece` with
/// next seq `next` that kept `kept` bytes: a block it doesn't hold, a put made after the last block it kept (in this
/// piece, or in a later one). Definitions are the application's: always made again.
fn lost(op: &Op, made: &Made, i: usize, piece: u64, next: u64, kept: u64) -> bool {
    match op {
        Op::Add(_) => true,
        Op::Put(..) => {
            let (before, after) = (made.before[i], made.after[i]);
            // a put whose last block is kept isn't given again: its own records changed what the writer remembers
            made.pieces[i] > piece
                || (made.pieces[i] == piece && (after > kept || (after == kept && before == after)))
        }
        _ => made.seqs[i] >= next,
    }
}

/// The piece of `ops` written without a crash, and where each call was made.
fn reference(alignment: u32, ops: &[Op], level: ww_tape::Integrity) -> (Vec<u8>, Made) {
    let mut w = begin_at(alignment, level);
    let mut defs = base_defs().clone();
    let mut made = Made {
        seqs: vec![],
        before: vec![],
        after: vec![],
        pieces: vec![],
    };
    for op in ops {
        made.before.push(w.bytes());
        apply(&mut w, &mut defs, *op);
        made.after.push(w.bytes());
        made.seqs.push(w.next_seq().wrapping_sub(1));
        made.pieces.push(0);
    }
    let tape = w.sink().tape();
    w.close().unwrap();
    let bytes = tape.to_vec().unwrap();
    let ranged: Vec<_> = TapeReader::open(&bytes[..])
        .unwrap()
        .entries(0)
        .unwrap()
        .into_iter()
        .filter(|e| e.track() == Some(TrackNum::new(13)) && e.kind == BlockKind::Events)
        .map(|e| e.range)
        .collect();
    assert!(
        !ranged.is_empty() && ranged.iter().all(Option::is_some),
        "every chunk of track 13 has its range: {ranged:?}"
    );
    // a deferred block takes its seq where it is placed
    let placed = record_seqs(&bytes);
    for (op, seq) in ops.iter().zip(&mut made.seqs) {
        if let Op::Defer(n) = op {
            *seq = placed[n];
        }
    }
    (bytes, made)
}

/// The ends of the blocks of `bytes` and some places in between, ascending: where the tests cut a piece.
pub(crate) fn cut_points(bytes: &[u8], extra: usize) -> Vec<usize> {
    let mut rd = FileBlocks::with_recovery(bytes, Recovery::Prefix);
    let mut out = vec![0];
    while let Some(Ok(_)) = rd.next_block() {
        out.push(rd.offset() as usize);
    }
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    for _ in 0..extra {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.push((x % bytes.len() as u64) as usize);
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Every `stride`th of `points`, the first and the last kept: a sweep that only needs a few cuts of each kind.
fn every(points: Vec<usize>, stride: usize) -> Vec<usize> {
    let last = points.last().copied();
    let mut out: Vec<usize> = points.into_iter().step_by(stride).collect();
    out.extend(last.filter(|l| out.last() != Some(l)));
    out
}

/// Give a writer resumed after a crash, keeping `kept` bytes, the calls it lost, then close it; its bytes.
fn close_resumed(mut w: TapeWriter<MemSink>, ops: &[Op], made: &Made, kept: u64) -> Vec<u8> {
    let next = w.next_seq();
    let mut defs = base_defs().clone();
    for (i, op) in ops.iter().enumerate() {
        if lost(op, made, i, 0, next, kept) {
            apply(&mut w, &mut defs, *op);
        }
    }
    let tape = w.sink().tape();
    w.close().unwrap();
    tape.to_vec().unwrap()
}

/// The blocks of `bytes` around `at`, one line each (offset, kind, flags, seq, the slots of its records): what a
/// failed comparison shows.
fn blocks_around(bytes: &[u8], at: u64) -> String {
    let mut rd = FileBlocks::with_recovery(bytes, Recovery::Prefix);
    let mut out = vec![];
    while let Some(Ok(b)) = rd.next_block() {
        let slots: Vec<Option<Slot>> = if b.kind == BlockKind::Records {
            b.records()
                .flatten()
                .map(|r| Slot::of(r.track, r.item, b.is_keyed()))
                .collect()
        } else {
            vec![]
        };
        let line = format!("{:?} flags {} seq {} {slots:?}", b.kind, b.flags, b.seq);
        let offset = rd.offset();
        if offset + 600 > at && offset < at + 1500 {
            out.push(format!("{offset} {line}"));
        }
    }
    out.join("\n")
}

/// A piece cut at every block boundary and at bytes in between resumes, takes the calls it lost and ends with the
/// bytes, and so the content hash, of the piece written without the crash: with deferred blocks, checkpoints,
/// definitions put again and copied, state items put, a memory small enough to forget, definitions made mid-piece,
/// and cuts inside the end of the piece.
fn cut_anywhere(alignment: u32, level: ww_tape::Integrity) {
    let ops = ops(8);
    let (want, made) = reference(alignment, &ops, level);
    let r = TapeReader::open(&want[..]).unwrap();
    let entries = r.entries(0).unwrap();
    let defs = r.track_entries(0, ww_tape::index::STATE_TRACK_ID).unwrap();
    let puts = crate::definitions::puts(&want).len();
    assert!(puts >= 8, "puts and copies: {puts}");
    assert!(defs.iter().all(|e| e.repeat), "definitions are put");
    assert!(entries.len() >= 40, "{} blocks", entries.len());
    let all = r.definitions_at(0, u64::MAX).unwrap();
    assert_eq!(
        tracks(&all),
        [1, 2, 9, 11, 12, 13],
        "the tracks with data or items, those added mid-piece among them"
    );
    let forgotten = (0..3u64).all(|k| {
        let hash = ww_tape::ItemHash::of(&[k as u8; 40]);
        r.state_entries(0)
            .unwrap()
            .iter()
            .filter(|(_, s)| {
                s.slot
                    == Slot::State {
                        track: TrackNum::new(11),
                    }
                    && s.hash == hash
            })
            .count()
            >= 2
    });
    assert!(forgotten, "items of track 11 written again");
    let mut ended = 0;
    for cut in cut_points(&want, 40) {
        let mut sink = MemSink::new();
        sink.write_all(&want[..cut]).unwrap();
        let (mut w, resumed) = TapeWriter::resume_on(sink, options(alignment)).unwrap();
        assert!(resumed.kept_bytes <= cut as u64);
        assert_eq!(resumed.kept_bytes + resumed.dropped_bytes, cut as u64);
        assert!(
            resumed
                .entries
                .iter()
                .all(|e| e.offset < resumed.kept_bytes)
        );
        ended += usize::from(resumed.ended.is_some());
        if w.bytes() == 0 {
            // cut inside the head: the piece starts again
            w.begin_piece(&head_at(level), &[]).unwrap();
        }
        assert_eq!(w.integrity(), level, "the level its head names");
        let got = close_resumed(w, &ops, &made, resumed.kept_bytes);
        assert!(
            got == want,
            "{level:?}, alignment {alignment}, cut at {cut} of {}, kept {}\nwant\n{}\ngot\n{}",
            want.len(),
            resumed.kept_bytes,
            blocks_around(&want, cut as u64),
            blocks_around(&got, cut as u64)
        );
    }
    assert!(ended > 0, "cuts after the close");
}

#[test]
fn a_piece_cut_anywhere_resumes_to_the_same_bytes() {
    cut_anywhere(0, ww_tape::Integrity::Full);
}

#[test]
fn an_aligned_piece_cut_anywhere_resumes_to_the_same_bytes() {
    cut_anywhere(ALIGNMENT, ww_tape::Integrity::Full);
}

/// Below Integrity::Full a resume replays the piece unhashed and goes on at the level its head names, whatever the
/// options say: the same bytes as without the crash.
#[test]
fn a_piece_below_full_cut_anywhere_resumes_to_the_same_bytes() {
    for level in [ww_tape::Integrity::CrcOnly, ww_tape::Integrity::None] {
        cut_anywhere(0, level);
        cut_anywhere(ALIGNMENT, level);
    }
}

/// A resume whose options contradict the layout the piece describes is refused before the piece is touched: an
/// aligned piece with unaligned options and the reverse. The error names both alignments.
#[test]
fn a_resume_with_another_alignment_is_refused() {
    let ops = ops(8);
    for (written, given) in [(ALIGNMENT, 0), (0, ALIGNMENT)] {
        let (want, _) = reference(written, &ops, ww_tape::Integrity::Full);
        let cut = want.len() / 2;
        let mut sink = MemSink::new();
        sink.write_all(&want[..cut]).unwrap();
        let tape = sink.tape();
        let Err(e) = TapeWriter::resume_on(sink, options(given)) else {
            panic!("alignment {given} on a piece written with {written} is refused");
        };
        match &e {
            WriteError::Input(InputError::Layout {
                described,
                given: g,
            }) => {
                assert_eq!(described.alignment.get(), written);
                assert_eq!(g.alignment.get(), given);
            }
            other => panic!("{other:?}"),
        }
        let text = e.to_string();
        assert!(
            text.contains(&written.to_string()) && text.contains(&given.to_string()),
            "{text}"
        );
        assert_eq!(
            tape.to_vec().unwrap(),
            want[..cut],
            "the piece is untouched"
        );

        // the options it was written with resume it, and say what was checked
        let mut sink = MemSink::new();
        sink.write_all(&want[..cut]).unwrap();
        let (_, resumed) = TapeWriter::resume_on(sink, options(written)).unwrap();
        assert_eq!(resumed.layout.map(|l| l.alignment.get()), Some(written));
    }
}

/// A piece cut off inside the block that holds its layout (the first after the head) has no layout to check against:
/// it resumes with any options, and says nothing was checked.
#[test]
fn a_piece_without_its_layout_resumes_unchecked() {
    let ops = ops(2);
    let (want, _) = reference(0, &ops, ww_tape::Integrity::Full);
    let r = TapeReader::open(&want[..]).unwrap();
    let first = (r.track_entries(0, ww_tape::index::STATE_TRACK_ID).unwrap())
        .into_iter()
        .next()
        .unwrap();
    // inside the block of the layout and the producer
    let cut = (first.offset + 10) as usize;
    let mut sink = MemSink::new();
    sink.write_all(&want[..cut]).unwrap();
    let (_, resumed) = TapeWriter::resume_on(sink, options(ALIGNMENT)).unwrap();
    assert_eq!(resumed.layout, None);
}

/// [TapeWriter::resume] reopens an unclosed file, keeps its good blocks and reports them.
#[test]
fn resume_reopens_an_unclosed_file() {
    let dir = scratch("resume_file");
    let path = dir.join("p.wwt");
    let ops = ops(6);
    let (want, made) = reference(0, &ops, ww_tape::Integrity::Full);
    let cut = want.len() * 2 / 3;
    std::fs::write(&path, &want[..cut]).unwrap();
    let (mut w, resumed) = ww_tape_fs::writer::resume(&path, options(0)).unwrap();
    assert!(resumed.kept_bytes <= cut as u64);
    assert!(
        resumed
            .entries
            .iter()
            .all(|e| e.offset < resumed.kept_bytes)
    );
    assert!(resumed.entries.iter().any(|e| e.kind == BlockKind::Events));
    let next = w.next_seq();
    let mut defs = base_defs().clone();
    for (i, op) in ops.iter().enumerate() {
        if lost(op, &made, i, 0, next, resumed.kept_bytes) {
            apply(&mut w, &mut defs, *op);
        }
    }
    w.close().unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), want);
    // closed: written on in place after its close, the closed bytes a prefix
    let (w, resumed) = ww_tape_fs::writer::resume(&path, options(0)).unwrap();
    assert_eq!(resumed.kept_bytes, want.len() as u64);
    assert!(resumed.after_close.is_some());
    w.close().unwrap();
    let got = std::fs::read(&path).unwrap();
    assert_eq!(got[..want.len()], want[..]);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A resumed piece whose newest block of track 1's definition is damaged takes that slot as unknown (the index lists
/// it): the definition is put again before the next data, so every block of data after the resume has its
/// definitions close behind and readable; it ends whole, its content hash over the damaged bytes (writing side
/// review 2, point 6).
#[test]
fn a_damaged_block_of_definitions_is_put_again_after_a_resume() {
    let ops = ops(12);
    let (want, made) = reference(0, &ops, ww_tape::Integrity::Full);
    let r = TapeReader::open(&want[..]).unwrap();
    let cut = want.len() / 2;
    let newest = r
        .state_entries(0)
        .unwrap()
        .into_iter()
        .rfind(|(e, s)| {
            s.slot
                == Slot::Track {
                    number: TrackNum::new(1),
                }
                && (e.offset as usize) < cut - 200
        })
        .unwrap()
        .0;
    let flipped = newest.offset as usize + 40;
    let mut sink = MemSink::new();
    let mut damaged = want[..cut].to_vec();
    damaged[flipped] ^= 1;
    sink.write_all(&damaged).unwrap();
    let (w, resumed) = TapeWriter::resume_on(sink, options(0)).unwrap();
    let kept = resumed.kept_bytes;
    let got = close_resumed(w, &ops, &made, kept);
    let r = TapeReader::open(&got[..]).unwrap();
    assert!(r.verify(0).map(|v| v.holds).unwrap());
    let data: Vec<u64> = r
        .entries(0)
        .unwrap()
        .iter()
        .filter(|e| {
            e.kind == BlockKind::Events
                || (e.kind == BlockKind::Records && e.track_id == RECORDS_TRACK_ID)
        })
        .map(|e| e.offset)
        .filter(|at| *at >= kept)
        .collect();
    assert_ne!(data, [] as [u64; 0]);
    for at in data {
        for slot in [
            Slot::Track {
                number: TrackNum::new(1),
            },
            Slot::Clock {
                number: ClockNum::new(1),
            },
        ] {
            let found = r.state_at(0, slot, at).unwrap();
            assert!(found.item.is_some(), "at {at}: {slot:?}");
        }
    }
}

/// The hash of track `id` in the close of the one piece of `bytes`.
fn close_hash(bytes: &[u8], id: TrackId) -> ww_tape::TrackHash {
    let r = TapeReader::open(bytes).unwrap();
    let (_, rows) = r.pieces()[0].close.as_ref().unwrap();
    rows.iter().find(|t| t.track_id == id).unwrap().hash
}

/// A damaged chunk a resume keeps, which an index lists, goes into its track's hash by the unit hash in its entry:
/// the close states the hash of the piece written without the damage.
#[test]
fn a_damaged_chunk_kept_by_a_resume_counts_in_its_track_hash() {
    let ops = ops(12);
    let (want, made) = reference(0, &ops, ww_tape::Integrity::Full);
    let r = TapeReader::open(&want[..]).unwrap();
    let cut = want.len() / 2;
    let listed = r
        .entries(0)
        .unwrap()
        .into_iter()
        .rfind(|e| {
            e.track() == Some(TrackNum::new(1))
                && e.kind == BlockKind::Events
                && (e.offset as usize) < cut - 2000
        })
        .unwrap();
    // a byte of its payload: the chunk doesn't read, its entry in a checkpoint stands for it
    let flipped = (listed.offset + u64::from(listed.len)) as usize - 20;
    let mut sink = MemSink::new();
    let mut damaged = want[..cut].to_vec();
    damaged[flipped] ^= 1;
    sink.write_all(&damaged).unwrap();
    let (w, resumed) = TapeWriter::resume_on(sink, options(0)).unwrap();
    assert!(resumed.kept_bytes > listed.offset + u64::from(listed.len));
    let got = close_resumed(w, &ops, &made, resumed.kept_bytes);
    assert_ne!(got, want, "the damaged byte is kept");
    assert_eq!(
        close_hash(&got, chunk_id(0)),
        close_hash(&want, chunk_id(0))
    );
    assert!(
        close_rows(&got).iter().all(|t| !t.incomplete),
        "a chunk's unit hash stands for it"
    );
}

fn close_rows(bytes: &[u8]) -> Vec<ww_tape::piece::TrackRange> {
    let r = TapeReader::open(bytes).unwrap();
    r.pieces()[0].close.as_ref().unwrap().1.clone()
}

/// A damaged block of records a resume keeps names no track in its index entry: each track that may hold records
/// leaves them out of its hash, and its close row says the hash is incomplete; a piece written whole has none.
#[test]
fn a_damaged_record_block_kept_by_a_resume_marks_its_tracks_incomplete() {
    let ops = ops(12);
    let (want, made) = reference(0, &ops, ww_tape::Integrity::Full);
    assert!(close_rows(&want).iter().all(|t| !t.incomplete));
    let r = TapeReader::open(&want[..]).unwrap();
    let cut = want.len() / 2;
    let listed = r
        .entries(0)
        .unwrap()
        .into_iter()
        .rfind(|e| {
            e.kind == BlockKind::Records
                && !e.repeat
                && e.track_id == RECORDS_TRACK_ID
                && (e.offset as usize) < cut - 2000
        })
        .unwrap();
    let flipped = (listed.offset + u64::from(listed.len)) as usize - 8;
    let mut sink = MemSink::new();
    let mut damaged = want[..cut].to_vec();
    damaged[flipped] ^= 1;
    sink.write_all(&damaged).unwrap();
    let (w, resumed) = TapeWriter::resume_on(sink, options(0)).unwrap();
    assert!(resumed.kept_bytes > listed.offset + u64::from(listed.len));
    let got = close_resumed(w, &ops, &made, resumed.kept_bytes);
    let rows = close_rows(&got);
    let marked: Vec<_> = rows.iter().filter(|t| t.incomplete).collect();
    assert!(!marked.is_empty(), "{rows:?}");
    // the track whose records the damage took is among them, its hash short of those records
    let whole = close_rows(&want);
    assert!(
        marked.iter().any(|t| whole
            .iter()
            .any(|w| w.track_id == t.track_id && w.records > 0 && w.hash != t.hash)),
        "{marked:?}"
    );
    // the chunks' tracks that hold no records keep their hash
    assert_eq!(
        close_hash(&got, chunk_id(0)),
        close_hash(&want, chunk_id(0))
    );
}

/// A track hash a resume left incomplete stays marked in the pieces after: the next piece's head row carries the mark
/// and each later close row of the track says so, with records or without.
#[test]
fn an_incomplete_track_hash_stays_marked_in_later_pieces() {
    let dir = scratch("incomplete_on");
    let owner = OwnerId([4; 32]);
    let defs = base_defs();
    let mut c = ww_tape_fs::chain::create(&dir, owner, 0, &[], chain_options()).unwrap();
    for n in 0..40 {
        let b = records(c.writer().unwrap().next_id(), n);
        c.write_block(defs, &b).unwrap();
    }
    drop(c);
    let first = dir.join(piece_name(0));
    let mut bytes = std::fs::read(&first).unwrap();
    let listed = TapeReader::open(&bytes[..])
        .unwrap()
        .entries(0)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == BlockKind::Records && !e.repeat && e.track_id == RECORDS_TRACK_ID)
        .unwrap();
    bytes[(listed.offset + u64::from(listed.len)) as usize - 8] ^= 1;
    std::fs::write(&first, &bytes).unwrap();
    let (mut c, _) = ww_tape_fs::chain::resume(&dir, owner, 0, chain_options()).unwrap();
    for n in 40..42 {
        let b = records(c.writer().unwrap().next_id(), n);
        c.write_block(defs, &b).unwrap();
    }
    c.cut_now(defs).unwrap();
    for n in 42..44 {
        let b = records(c.writer().unwrap().next_id(), n);
        c.write_block(defs, &b).unwrap();
    }
    // a piece without records of the track: its head still carries the mark
    c.cut_now(defs).unwrap();
    let done = c.close().unwrap();
    assert_eq!(done.len(), 3);
    let Describe::Track(t9) = track(9, CLOCK) else {
        unreachable!("track() gives a track")
    };
    for (i, f) in done.iter().enumerate() {
        let b = std::fs::read(FilePieces::new(&dir).path_of(f.tape)).unwrap();
        let r = TapeReader::open(&b[..]).unwrap();
        let p = &r.pieces()[0];
        let row = p
            .close
            .as_ref()
            .unwrap()
            .1
            .iter()
            .find(|t| t.track_id == t9.id());
        let head = p
            .head
            .as_ref()
            .unwrap()
            .1
            .iter()
            .find(|t| t.track_id == t9.id());
        assert_eq!(
            row.map(|r| r.incomplete),
            (i < 2).then_some(true),
            "piece {i} close"
        );
        assert_eq!(
            head.map(|h| h.incomplete),
            (i > 0).then_some(true),
            "piece {i} head"
        );
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A chain written to memory reads through [TapeReader] while it is written and once closed, and its pieces are the
/// bytes of the same chain written to files.
#[test]
fn a_chain_in_memory_reads_and_matches_files() {
    let dir = scratch("mem_chain");
    let opts = ChainOptions {
        tapes: ww_tape::PieceTapes::Each,
        writer: options(0),
        cut: CutPolicy {
            chunks: exact_units::Count::new(5),
            ..Default::default()
        },
    };
    let owner = OwnerId([5; 32]);
    let mem = MemPieces::new();
    let mut in_mem = PieceChain::create_on(mem.clone(), owner, 1, &[], opts.clone()).unwrap();
    let mut on_disk = ww_tape_fs::chain::create(&dir, owner, 1, &[], opts).unwrap();
    let (mut a, mut b) = (base_defs().clone(), base_defs().clone());
    for op in ops(10) {
        apply_chain(&mut in_mem, &mut a, op);
        apply_chain(&mut on_disk, &mut b, op);
    }
    // the open piece reads while it is written
    let live = mem.get(&in_mem.seq()).unwrap().unwrap();
    let r = TapeReader::open(live.clone()).unwrap();
    assert_ne!(r.pieces(), []);
    assert!(
        r.entries(0)
            .unwrap()
            .iter()
            .any(|e| e.kind == BlockKind::Events)
    );
    let a = in_mem.close().unwrap();
    let b = on_disk.close().unwrap();
    assert!(a.len() >= 3, "{} pieces", a.len());
    assert_eq!(a.len(), b.len());
    let mut all = vec![];
    for (fa, fb) in a.iter().zip(&b) {
        let tape = mem.get(&fa.tape).unwrap().unwrap();
        assert!(tape.is_closed());
        let bytes = tape.to_vec().unwrap();
        assert_eq!(
            bytes,
            std::fs::read(FilePieces::new(&dir).path_of(fb.tape)).unwrap()
        );
        assert_eq!(fa.tape, fb.tape);
        assert_eq!(fa.piece.hash, fb.piece.hash);
        let r = TapeReader::open(tape).unwrap();
        assert!(r.verify(0).map(|v| v.holds).unwrap());
        all.extend(bytes);
    }
    let r = TapeReader::open(&all[..]).unwrap();
    assert!(r.chain().is_whole(), "{:?}", r.chain());
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The calls of a chain: each chunk through [PieceChain::write_chunk], so cuts come before chunks.
fn apply_chain<F: PieceStore>(c: &mut PieceChain<F>, defs: &mut Definitions, op: Op) {
    match op {
        Op::Chunk(t, k) => {
            let (h, b) = chunk(t, k);
            c.write_chunk(defs, &h, &column(&b)).unwrap();
        }
        Op::Ranged(k) => {
            let (h, cols) = ranged_chunk(k);
            let cols: Vec<_> = cols.iter().map(|(s, b)| (*s, &b[..])).collect();
            c.write_chunk(defs, &h, &cols).unwrap();
        }
        _ => apply(c.writer().unwrap(), defs, op),
    }
}

fn chain_options() -> ChainOptions {
    ChainOptions {
        tapes: ww_tape::PieceTapes::Each,
        writer: options(0),
        cut: CutPolicy {
            chunks: exact_units::Count::new(6),
            ..Default::default()
        },
    }
}

/// The pieces of a chain of `ops` written to `dir` without a crash, and where each call was made.
fn reference_chain(dir: &Path, ops: &[Op]) -> (Vec<PathBuf>, Made) {
    let mut c = ww_tape_fs::chain::create(dir, OwnerId([9; 32]), 0, &[], chain_options()).unwrap();
    let mut defs = base_defs().clone();
    let mut made = Made {
        seqs: vec![],
        before: vec![],
        after: vec![],
        pieces: vec![],
    };
    for op in ops {
        // a chunk may cut first: the bytes before it are in the piece it lands in
        if matches!(op, Op::Chunk(..) | Op::Ranged(_)) && c.cut_due() {
            c.cut_now(&defs).unwrap();
        }
        made.before.push(c.writer().unwrap().bytes());
        apply_chain(&mut c, &mut defs, *op);
        made.after.push(c.writer().unwrap().bytes());
        made.seqs
            .push(c.writer().unwrap().next_seq().wrapping_sub(1));
        made.pieces.push(c.seq());
    }
    let paths: Vec<PathBuf> = c
        .close()
        .unwrap()
        .into_iter()
        .map(|f| FilePieces::new(dir).path_of(f.tape))
        .collect();
    let mut placed = BTreeMap::new();
    for p in &paths {
        placed.extend(record_seqs(&std::fs::read(p).unwrap()));
    }
    for (op, seq) in ops.iter().zip(&mut made.seqs) {
        if let Op::Defer(n) = op {
            *seq = placed[n];
        }
    }
    (paths, made)
}

/// Give a chain resumed keeping `kept` bytes of its last piece the calls it lost.
fn replay_chain<F: PieceStore>(c: &mut PieceChain<F>, ops: &[Op], made: &Made, kept: u64) {
    let next = c.writer().unwrap().next_seq();
    let piece = c.seq();
    let mut defs = base_defs().clone();
    for (i, op) in ops.iter().enumerate() {
        if lost(op, made, i, piece, next, kept) {
            apply_chain(c, &mut defs, *op);
        }
    }
}

/// A chain in memory left unclosed resumes on the same [MemPieces] and ends as one written in one go.
#[test]
fn a_chain_in_memory_resumes() {
    let tmp = scratch("mem_resume");
    let ops = ops(10);
    let (paths, made) = reference_chain(&tmp, &ops);
    let mem = MemPieces::new();
    let mut c =
        PieceChain::create_on(mem.clone(), OwnerId([9; 32]), 0, &[], chain_options()).unwrap();
    let mut defs = base_defs().clone();
    for op in &ops[..ops.len() / 2] {
        apply_chain(&mut c, &mut defs, *op);
    }
    drop(c);
    let (mut c, resumed) =
        PieceChain::resume_on(mem.clone(), OwnerId([9; 32]), 0, chain_options()).unwrap();
    assert_ne!(resumed.entries, [] as [ww_tape::IndexEntry; 0]);
    assert_ne!(c.closed(), []);
    replay_chain(&mut c, &ops, &made, resumed.kept_bytes);
    let got = c.close().unwrap();
    assert_eq!(got.len(), paths.len());
    for (f, p) in got.iter().zip(&paths) {
        assert_eq!(
            mem.get(&f.tape).unwrap().unwrap().to_vec().unwrap(),
            std::fs::read(p).unwrap()
        );
    }
    // no piece of a chain in a place: nothing to resume
    assert!(
        FilePieces::new(&tmp.join("none"))
            .closed(&0)
            .unwrap()
            .is_none()
    );
    assert!(
        ww_tape_fs::chain::resume(&tmp.join("none"), OwnerId([9; 32]), 0, chain_options()).is_err()
    );
    std::fs::remove_dir_all(&tmp).unwrap();
}

/// Where a test keeps a chain of pieces in one file: on disk or in memory, through the same test body.
trait OneFile {
    type F: PieceStore<Key = u64> + Clone;
    /// A fresh place named `name` holding `bytes` as the chain's file; the store and the file's key.
    fn plant(&self, name: &str, bytes: &[u8]) -> (Self::F, u64);
    /// The bytes of the file under `key`.
    fn bytes(&self, f: &Self::F, key: u64) -> Vec<u8>;
}

struct OnDisk(PathBuf);

impl OneFile for OnDisk {
    type F = FilePieces;
    fn plant(&self, name: &str, bytes: &[u8]) -> (FilePieces, u64) {
        let path = self.0.join(name);
        std::fs::write(&path, bytes).unwrap();
        (FilePieces::file(&path), 0)
    }
    fn bytes(&self, f: &FilePieces, key: u64) -> Vec<u8> {
        std::fs::read(f.path_of(key)).unwrap()
    }
}

struct InMemory;

impl OneFile for InMemory {
    type F = MemPieces;
    fn plant(&self, _: &str, bytes: &[u8]) -> (MemPieces, u64) {
        let mem = MemPieces::new();
        let mut sink = mem.create_whole(&0).unwrap();
        sink.write_all(bytes).unwrap();
        (mem, 0)
    }
    fn bytes(&self, f: &MemPieces, key: u64) -> Vec<u8> {
        f.get(&key).unwrap().unwrap().to_vec().unwrap()
    }
}

/// Where a test keeps a chain of a file per piece: on disk or in memory, through the same test body.
trait EachPiece {
    type F: PieceStore<Key = u64> + Clone;
    /// A fresh place for run `run` holding the closed pieces `done` and, unless None, the start of the next piece
    /// as an unclosed one.
    fn plant(&self, run: usize, done: &[Vec<u8>], open: Option<&[u8]>) -> Self::F;
    /// The bytes of piece `seq`.
    fn bytes(&self, f: &Self::F, seq: u64) -> Vec<u8>;
}

impl EachPiece for OnDisk {
    type F = FilePieces;
    fn plant(&self, run: usize, done: &[Vec<u8>], open: Option<&[u8]>) -> FilePieces {
        let d = self.0.join(format!("run{run}"));
        std::fs::create_dir(&d).unwrap();
        for (j, w) in done.iter().enumerate() {
            std::fs::write(d.join(piece_name(j as u64)), w).unwrap();
        }
        if let Some(open) = open {
            std::fs::write(d.join(piece_name(done.len() as u64)), open).unwrap();
        }
        FilePieces::new(&d)
    }
    fn bytes(&self, f: &FilePieces, seq: u64) -> Vec<u8> {
        std::fs::read(f.path_of(seq)).unwrap()
    }
}

impl EachPiece for InMemory {
    type F = MemPieces;
    fn plant(&self, _: usize, done: &[Vec<u8>], open: Option<&[u8]>) -> MemPieces {
        let mem = MemPieces::new();
        for (j, w) in done.iter().enumerate() {
            let mut s = mem.create(&(j as u64)).unwrap();
            s.write_all(w).unwrap();
            s.close().unwrap();
        }
        if let Some(open) = open {
            let mut s = mem.create(&(done.len() as u64)).unwrap();
            s.write_all(open).unwrap();
        }
        mem
    }
    fn bytes(&self, f: &MemPieces, seq: u64) -> Vec<u8> {
        f.get(&seq).unwrap().unwrap().to_vec().unwrap()
    }
}

/// A chain cut off anywhere in pieces `swept` (at every `stride`th of [cut_points] with `extra` places between block
/// ends), or between a cut's close and the next piece, resumes and ends with the pieces of the chain written without
/// the crash; the pieces closed before are read back.
fn chain_cut_anywhere<P: EachPiece>(
    place: &P,
    want: &[Vec<u8>],
    ops: &[Op],
    made: &Made,
    swept: Range<usize>,
    extra: usize,
    stride: usize,
) {
    let mut runs = 0;
    for (i, piece) in want.iter().enumerate().take(swept.end).skip(swept.start) {
        // a crash between the cut's close and the next piece: no file of piece i yet
        let mut cuts: Vec<Option<usize>> = vec![None];
        cuts.extend(
            every(cut_points(piece, extra), stride)
                .into_iter()
                .map(Some),
        );
        for cut in cuts {
            if cut.is_none() && i == 0 {
                continue;
            }
            let f = place.plant(runs, &want[..i], cut.map(|c| &piece[..c]));
            runs += 1;
            let (mut c, resumed) = PieceChain::resume_on(f, OwnerId([9; 32]), 0, chain_options())
                .unwrap_or_else(|e| panic!("piece {i}, cut {cut:?}: {e}"));
            assert_eq!(c.closed().len(), i);
            replay_chain(&mut c, ops, made, resumed.kept_bytes);
            let f = c.store().clone();
            let got = c.close().unwrap();
            assert_eq!(got.len(), want.len(), "piece {i}, cut {cut:?}");
            for (j, w) in want.iter().enumerate() {
                let g = place.bytes(&f, j as u64);
                let first = g.iter().zip(w).position(|(a, b)| a != b);
                assert!(
                    &g == w,
                    "piece {i}, cut {cut:?}: piece {j} differs at {first:?}, {} vs {} bytes",
                    g.len(),
                    w.len()
                );
            }
        }
    }
}

/// A chain cut off anywhere in any piece resumes to the pieces written without the crash: every piece in memory;
/// on files, a few cuts of the last piece (inside it, at its end, before its file exists).
#[test]
fn a_chain_cut_anywhere_resumes_to_the_same_pieces() {
    let dir = scratch("chain_resume");
    let want_dir = dir.join("want");
    std::fs::create_dir(&want_dir).unwrap();
    let ops = ops(14);
    let (paths, made) = reference_chain(&want_dir, &ops);
    assert!(paths.len() >= 4, "{} pieces", paths.len());
    let want: Vec<Vec<u8>> = paths.iter().map(|p| std::fs::read(p).unwrap()).collect();
    chain_cut_anywhere(&InMemory, &want, &ops, &made, 0..want.len(), 20, 1);
    let disk = dir.join("disk");
    std::fs::create_dir(&disk).unwrap();
    chain_cut_anywhere(
        &OnDisk(disk),
        &want,
        &ops,
        &made,
        want.len() - 1..want.len(),
        0,
        6,
    );
    // a chain whose last piece is closed resumes after its close; closed again with nothing appended, no byte changes
    let (c, resumed) =
        ww_tape_fs::chain::resume(&want_dir, OwnerId([9; 32]), 0, chain_options()).unwrap();
    assert!(resumed.after_close.is_some());
    assert_eq!(c.closed().len(), want.len() - 1);
    c.close().unwrap();
    for (p, w) in paths.iter().zip(&want) {
        let got = std::fs::read(p).unwrap();
        assert!(got.len() >= w.len() && got[..w.len()] == w[..]);
    }
    assert_eq!(
        &std::fs::read(paths.last().unwrap()).unwrap(),
        want.last().unwrap()
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

fn one_file_options() -> ChainOptions {
    ChainOptions {
        tapes: ww_tape::PieceTapes::One,
        ..chain_options()
    }
}

/// A chain written into one file is its pieces written as files of their own, back to back, byte for byte, and
/// reads to the same items: the files' chain is the model of the one file.
fn one_file_is_its_pieces<P: OneFile>(place: &P, scratch_dir: &Path) {
    let ops = ops(14);
    let want_dir = scratch_dir.join("pieces");
    std::fs::create_dir(&want_dir).unwrap();
    let (paths, _) = reference_chain(&want_dir, &ops);
    let want: Vec<u8> = paths
        .iter()
        .flat_map(|p| std::fs::read(p).unwrap())
        .collect();
    let (f, path) = place.plant("one.wwt", &[]);
    // planted empty: a new chain takes the place of the empty file
    let (mut c, _) = PieceChain::resume_on(f, OwnerId([9; 32]), 0, one_file_options()).unwrap();
    let mut defs = base_defs().clone();
    for op in &ops {
        if matches!(op, Op::Chunk(..) | Op::Ranged(_)) && c.cut_due() {
            c.cut_now(&defs).unwrap();
        }
        apply_chain(&mut c, &mut defs, *op);
    }
    let f = c.store().clone();
    let done = c.close().unwrap();
    assert_eq!(done.len(), paths.len());
    let got = place.bytes(&f, path);
    assert!(got == want, "{} vs {} bytes", got.len(), want.len());
    let mut at = 0;
    for d in &done {
        assert_eq!((d.tape, d.piece.start), (path, at));
        at += d.piece.len;
    }
    // the same items whether read from the one file or from each piece's file
    let one = TapeReader::open(&got[..]).unwrap();
    assert!(one.chain().is_whole(), "{:?}", one.chain());
    assert_eq!(one.pieces().len(), paths.len());
    let mut buf = vec![];
    for (i, p) in paths.iter().enumerate() {
        let bytes = std::fs::read(p).unwrap();
        let alone = TapeReader::open(&bytes[..]).unwrap();
        assert!(
            one.verify(i).map(|v| v.holds).unwrap() && alone.verify(0).map(|v| v.holds).unwrap()
        );
        let (a, b) = (one.entries(i).unwrap(), alone.entries(0).unwrap());
        assert_eq!(a, b);
        for e in &a {
            let x = one.read_block(i, e, &mut buf).unwrap().bytes().to_vec();
            let y = alone.read_block(0, e, &mut buf).unwrap().bytes().to_vec();
            assert_eq!(x, y);
        }
        assert_eq!(one.pieces()[i].close, alone.pieces()[0].close);
    }
}

#[test]
fn a_one_file_chain_is_its_pieces_back_to_back() {
    let dir = scratch("one_file_pieces");
    std::fs::create_dir(dir.join("disk")).unwrap();
    one_file_is_its_pieces(&OnDisk(dir.clone()), &dir.join("disk"));
    let mem_dir = dir.join("mem");
    std::fs::create_dir(&mem_dir).unwrap();
    one_file_is_its_pieces(&InMemory, &mem_dir);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A chain in one file cut off at every `stride`th block boundary of its `last` pieces and at `extra` bytes in
/// between in each (inside the next block, inside a piece's end, between a cut's close and the next piece head)
/// resumes, takes the calls it lost and ends with the bytes of the file written without the crash.
fn one_file_cut_anywhere<P: OneFile>(
    place: &P,
    scratch_dir: &Path,
    last: usize,
    extra: usize,
    stride: usize,
) {
    let ops = ops(14);
    let want_dir = scratch_dir.join("pieces");
    std::fs::create_dir_all(&want_dir).unwrap();
    let (paths, made) = reference_chain(&want_dir, &ops);
    let pieces: Vec<Vec<u8>> = paths.iter().map(|p| std::fs::read(p).unwrap()).collect();
    let want: Vec<u8> = pieces.concat();
    let mut start = 0usize;
    let mut runs = 0;
    for (i, piece) in pieces.iter().enumerate() {
        if i < pieces.len().saturating_sub(last) {
            start += piece.len();
            continue;
        }
        for cut in every(cut_points(piece, extra), stride) {
            // the cut at a piece's very end is the next piece's start, with nothing of it written
            let (f, path) = place.plant(&format!("run{runs}.wwt"), &want[..start + cut]);
            runs += 1;
            let (mut c, resumed) =
                PieceChain::resume_on(f, OwnerId([9; 32]), 0, one_file_options())
                    .unwrap_or_else(|e| panic!("piece {i}, cut {cut}: {e}"));
            let whole = cut == piece.len() && i + 1 < pieces.len();
            assert_eq!(
                c.closed().len(),
                if whole { i + 1 } else { i },
                "piece {i}, cut {cut}"
            );
            replay_chain(&mut c, &ops, &made, resumed.kept_bytes);
            let f = c.store().clone();
            let got = c.close().unwrap();
            assert_eq!(got.len(), pieces.len(), "piece {i}, cut {cut}");
            let bytes = place.bytes(&f, path);
            let first = bytes.iter().zip(&want).position(|(a, b)| a != b);
            assert!(
                bytes == want,
                "piece {i}, cut {cut}: differs at {first:?}, {} vs {} bytes",
                bytes.len(),
                want.len()
            );
        }
        start += piece.len();
    }
}

#[test]
fn a_one_file_chain_cut_anywhere_resumes_to_the_same_bytes() {
    let dir = scratch("one_file_resume");
    // every piece in memory; a few cuts of the last two pieces on a file (inside, at a piece's end, past it)
    one_file_cut_anywhere(&InMemory, &dir.join("mem"), usize::MAX, 20, 1);
    one_file_cut_anywhere(&OnDisk(dir.clone()), &dir.join("disk"), 2, 0, 6);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A closed file of pieces opened again is written on after its last piece's close: every byte before stays, the
/// last piece closes again naming its first close, the chain reads whole and verifies, and every track's hash goes
/// on as if the two sessions were one.
fn one_file_reopened<P: OneFile>(place: &P) {
    let ops = ops(14);
    let (first, second) = ops.split_at(ops.len() / 2);
    let (f, path) = place.plant("reopen.wwt", &[]);
    let write = |f: P::F, part: &[Op], defs: &mut Definitions| {
        let (mut c, _) = PieceChain::resume_on(f, OwnerId([9; 32]), 0, one_file_options()).unwrap();
        for op in part {
            apply_chain(&mut c, defs, *op);
        }
        let f = c.store().clone();
        (f, c.close().unwrap())
    };
    let mut defs = base_defs().clone();
    let (f, a) = write(f, first, &mut defs);
    let before = place.bytes(&f, path);
    let (f, b) = write(f, second, &mut defs);
    assert!(b.len() >= a.len(), "{} then {} pieces", a.len(), b.len());
    let kept = a.len() - 1;
    assert_eq!(b[..kept], a[..kept], "the pieces cut before stay");
    let bytes = place.bytes(&f, path);
    assert_eq!(bytes[..before.len()], before[..], "every byte before stays");
    let r = TapeReader::open(&bytes[..]).unwrap();
    let chain = r.chain();
    assert!(chain.is_whole(), "{chain:?}");
    assert_eq!(r.pieces().len(), b.len());
    // the piece closed first is written on: its first close is named by the one after
    let closes = r.closes(kept).unwrap();
    assert!(closes.len() >= 2, "{closes:?}");
    assert_eq!(closes[0].1.hash, a[kept].piece.hash);
    assert_eq!(closes[1].1.prev, Some(closes[0].0));
    assert!((0..r.pieces().len()).all(|i| r.verify(i).map(|v| v.holds).unwrap()));

    // the same calls in one session: every track's hash is the same
    let mem = MemPieces::new();
    let mut c = PieceChain::create_on(mem, OwnerId([9; 32]), 0, &[], one_file_options()).unwrap();
    let mut defs = base_defs().clone();
    for op in &ops {
        apply_chain(&mut c, &mut defs, *op);
    }
    let once = c.close().unwrap();
    let hashes = |d: &[ww_tape::ChainPiece<u64>]| {
        d.last()
            .unwrap()
            .piece
            .tracks
            .iter()
            .map(|t| (t.track_id, t.owner, t.hash))
            .collect::<Vec<_>>()
    };
    assert_eq!(hashes(&b), hashes(&once));
}

#[test]
fn a_closed_one_file_chain_is_written_on_after_its_close() {
    let dir = scratch("one_file_reopen");
    one_file_reopened(&OnDisk(dir.clone()));
    one_file_reopened(&InMemory);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A chain whose unclosed last piece lost its piece head resumes to an error, in a file per piece and in one file:
/// the blocks after the head would make a piece without a link to the chain, which can't be cut.
#[test]
fn a_chain_whose_last_piece_lost_its_head_is_refused() {
    let dir = scratch("headless");
    for files in [ww_tape::PieceTapes::Each, ww_tape::PieceTapes::One] {
        let options = ChainOptions {
            tapes: files,
            ..chain_options()
        };
        let at = match files {
            ww_tape::PieceTapes::Each => dir.join("each"),
            ww_tape::PieceTapes::One => dir.join("one.wwt"),
        };
        if files == ww_tape::PieceTapes::Each {
            std::fs::create_dir(&at).unwrap();
        }
        let mut c =
            ww_tape_fs::chain::create(&at, OwnerId([9; 32]), 0, &[], options.clone()).unwrap();
        let mut defs = base_defs().clone();
        for op in ops(14) {
            apply_chain(&mut c, &mut defs, op);
        }
        assert!(c.seq() > 0, "{files:?}: a piece was cut");
        let w = c.writer().unwrap();
        w.flush_deferred().unwrap();
        let (file, start) = (w.sink().path().to_path_buf(), w.sink().base());
        drop(c);
        let mut bytes = std::fs::read(&file).unwrap();
        // inside the piece head's rows of carried-over tracks
        bytes[start as usize + 160] ^= 0x40;
        std::fs::write(&file, &bytes).unwrap();
        let resumed = ww_tape_fs::chain::resume(&at, OwnerId([9; 32]), 0, options);
        assert!(
            matches!(resumed, Err(WriteError::Index(_))),
            "{files:?}: {:?}",
            resumed.map(|(_, r)| r)
        );
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A store's refusal for another writer holding the bytes.
trait Held {
    fn is_locked(&self) -> bool;
}

impl Held for FsError {
    fn is_locked(&self) -> bool {
        matches!(self, Self::Locked(_))
    }
}

impl Held for MemError {
    fn is_locked(&self) -> bool {
        matches!(self, Self::Locked(_))
    }
}

/// Whether `r` is refused for another writer holding the bytes.
fn locked<T, E: Held>(r: Result<T, WriteError<E>>) -> bool {
    matches!(r, Err(WriteError::Sink { error, .. }) if error.is_locked())
}

/// One writer per file: a second chain resumed on a one-file chain another one writes is refused, on files and in
/// memory, as is a second writer of a piece file; once the first is gone the second goes on.
#[test]
fn a_second_writer_of_one_file_is_refused() {
    let dir = scratch("second_writer");
    let options = || ChainOptions {
        tapes: ww_tape::PieceTapes::One,
        ..chain_options()
    };
    let owner = OwnerId([7; 32]);
    fn write<F: PieceStore>(c: &mut PieceChain<F>, k: u64) {
        let (h, b) = chunk(0, k);
        c.write_chunk(base_defs(), &h, &column(&b)).unwrap();
    }

    let path = dir.join("run.wwt");
    let mut a = ww_tape_fs::chain::create(&path, owner, 0, &[], options()).unwrap();
    write(&mut a, 0);
    a.close().unwrap();
    let (mut a, _) = ww_tape_fs::chain::resume(&path, owner, 0, options()).unwrap();
    assert!(locked(ww_tape_fs::chain::resume(
        &path,
        owner,
        0,
        options()
    )));
    write(&mut a, 1);
    drop(a);
    let (mut b, _) = ww_tape_fs::chain::resume(&path, owner, 0, options()).unwrap();
    write(&mut b, 2);
    b.close().unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let r = TapeReader::open(&bytes[..]).unwrap();
    assert!(r.chain().is_whole(), "{:?}", r.chain());

    let mem = MemPieces::new();
    let mut a = PieceChain::create_on(mem.clone(), owner, 0, &[], options()).unwrap();
    assert!(locked(PieceChain::resume_on(
        mem.clone(),
        owner,
        0,
        options()
    )));
    write(&mut a, 0);
    a.close().unwrap();
    let (a, _) = PieceChain::resume_on(mem.clone(), owner, 0, options()).unwrap();
    assert!(locked(PieceChain::resume_on(
        mem.clone(),
        owner,
        0,
        options()
    )));
    drop(a);
    PieceChain::resume_on(mem.clone(), owner, 0, options()).unwrap();

    // a piece file: held from create to close; a second create finds it there
    let piece = dir.join("piece.wwt");
    let w = ww_tape_fs::writer::create(&piece, SyncPolicy::None).unwrap();
    assert!(matches!(
        ww_tape_fs::writer::create(&piece, SyncPolicy::None),
        Err(WriteError::Sink {
            error: ww_tape_fs::FsError::Exists(_),
            ..
        })
    ));
    assert!(locked(ww_tape_fs::writer::resume(&piece, options().writer)));
    drop(w);
    ww_tape_fs::writer::resume(&piece, options().writer).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A cut in place takes a sink of pieces back to back: a writer of one piece (a new file, a plain memory sink)
/// is refused before anything is written and handed back, so its piece goes on and closes, or resumes after a
/// crash.
#[test]
fn a_sink_of_one_piece_takes_no_cut_in_place() {
    let dir = scratch("one_piece_cut");
    let path = dir.join("one.wwt");
    let mut w = ww_tape_fs::writer::create(&path, SyncPolicy::None).unwrap();
    w.begin_piece(&head(), &[]).unwrap();
    let (h, c) = chunk(0, 0);
    w.write_chunk(base_defs(), &h, &column(&c)).unwrap();
    w.flush_deferred().unwrap();
    let len = w.bytes();
    let refused = w.cut_in_place(base_defs()).err().unwrap();
    assert!(matches!(refused.error, WriteError::Index(_)), "{refused}");
    assert_eq!(
        refused.to_string(),
        "cut refused, the writer handed back: index or piece block: a sink of one piece takes no cut in place"
    );
    assert_eq!(std::fs::metadata(&path).unwrap().len(), len);
    drop(refused);
    let (w, resumed) = ww_tape_fs::writer::resume(&path, WriterOptions::default()).unwrap();
    assert_eq!(resumed.kept_bytes, len);
    w.close().unwrap();

    let mut w = begin(0);
    let (h, c) = chunk(0, 0);
    w.write_chunk(base_defs(), &h, &column(&c)).unwrap();
    let refused = w.cut_in_place(base_defs()).err().unwrap();
    assert!(matches!(refused.error, WriteError::Index(_)), "{refused}");
    let mut w = *refused.writer.unwrap();
    let (h, c) = chunk(0, 1);
    w.write_chunk(base_defs(), &h, &column(&c)).unwrap();
    let bytes = w.sink().tape();
    w.close().unwrap();
    let bytes = bytes.to_vec().unwrap();
    let r = TapeReader::open(&bytes[..]).unwrap();
    assert_eq!(r.pieces().len(), 1);
    assert_eq!(
        r.entries(0)
            .unwrap()
            .iter()
            .filter(|e| e.kind == ww_tape::BlockKind::Events)
            .count(),
        2
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A closed piece an earlier ww_tape wrote (`data/other-layout.wwt`, its second piece): this reader takes its blocks
/// up to the close. Its tail is whole but ends no close this reader takes: a resume refuses and keeps every byte. Cut
/// short anywhere in its close or tail, the end is torn: a resume cuts off the close and what follows, never a block
/// before them.
#[test]
fn a_resume_of_another_layout_cuts_off_only_a_torn_end() {
    let bytes =
        std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/it/data/other-layout.wwt"))
            .unwrap();
    let r = TapeReader::open(&bytes[..]).unwrap();
    let p1 = r.pieces()[1].start as usize;
    let piece = &bytes[p1..];
    // the blocks by their headers: the close and the tail end it
    let mut blocks = Vec::new();
    let mut at = 0;
    while at + 32 <= piece.len() {
        let len = u32::from_le_bytes(piece[at + 8..at + 12].try_into().unwrap()) as usize;
        blocks.push((at, piece[at + 5]));
        at += len;
    }
    let close_at = blocks.iter().find(|(_, k)| *k == 8).unwrap().0;
    let tail_at = blocks.iter().find(|(_, k)| *k == 9).unwrap().0;
    let resume = |cut: usize| {
        let mut sink = MemSink::new();
        sink.write_all(&piece[..cut]).unwrap();
        let tape = sink.tape();
        let r = TapeWriter::resume_on(
            sink,
            WriterOptions {
                // the layout the earlier ww_tape wrote: one copy of each definition
                layout: Layout {
                    definitions: ww_tape::StatePolicy {
                        copies: exact_units::Count::new(1),
                        ..ww_tape::StatePolicy::DEFAULT
                    },
                    ..Layout::UNALIGNED
                },
                producer: String::new(),
                ..options(0)
            },
        )
        .map(|(_, r)| r);
        (r, tape.to_vec().unwrap())
    };
    match resume(piece.len()) {
        (Err(WriteError::Resume(ResumeError::Damaged { offset })), after) => {
            assert_eq!(offset, tail_at as u64);
            assert_eq!(after, piece, "the bytes stay");
        }
        (other, _) => panic!("{other:?}"),
    }
    for cut in [piece.len() - 1, piece.len() - 300, close_at + 10] {
        let resumed = resume(cut).0.unwrap();
        assert_eq!(resumed.kept_bytes, close_at as u64, "cut at {cut}");
        assert_eq!(
            resumed.dropped_bytes,
            (cut - close_at) as u64,
            "cut at {cut}"
        );
    }
}

/// A block of a piece damaged in place, good blocks of its session after it, no index naming them: a resume would cut
/// them off with the damage, more than a torn end, so it refuses and leaves the bytes as they are.
#[test]
fn a_resume_never_cuts_off_good_blocks() {
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    let tape = w.sink().tape();
    let defs = base_defs();
    for n in 0..6 {
        let block = records(w.next_id(), n);
        w.write_block(defs, &block).unwrap();
    }
    let mut bytes = tape.to_vec().unwrap();
    drop(w);
    let starts: Vec<usize> = (0..bytes.len() - 3)
        .filter(|&i| bytes[i..i + 4] == *b"WWTP")
        .collect();
    assert!(starts.len() >= 6, "{starts:?}");
    let bad = starts[starts.len() - 3];
    bytes[bad + 40] ^= 1;
    let mut sink = MemSink::new();
    sink.write_all(&bytes).unwrap();
    let tape = sink.tape();
    match TapeWriter::resume_on(sink, WriterOptions::default()) {
        Err(WriteError::Resume(ResumeError::GoodBlockAfterCut { offset })) => {
            assert_eq!(offset, starts[starts.len() - 2] as u64);
        }
        other => panic!("{:?}", other.map(|(_, r)| r)),
    }
    assert_eq!(tape.to_vec().unwrap(), bytes, "the bytes stay");
    // undamaged, with its last block torn: that alone is cut off
    let mut torn = bytes[..starts[starts.len() - 1] + 20].to_vec();
    torn[bad + 40] ^= 1;
    let mut sink = MemSink::new();
    sink.write_all(&torn).unwrap();
    let (_, resumed) = TapeWriter::resume_on(sink, WriterOptions::default()).unwrap();
    assert_eq!(resumed.kept_bytes, starts[starts.len() - 1] as u64);
}

/// The last block of an unclosed piece, whole, its length field rotted to name more bytes than are left: it reads as
/// a record the bytes stop inside of, but its bytes read as a good block of their own length, so a resume takes it as
/// damage and keeps every byte. A block start inside those bytes says the same.
#[test]
fn a_resume_refuses_a_whole_last_block_whose_length_rotted() {
    let mut w = begin(0);
    let tape = w.sink().tape();
    let mut last = 0;
    for n in 0..4 {
        last = tape.to_vec().unwrap().len();
        let block = records(w.next_id(), n);
        w.write_block(base_defs(), &block).unwrap();
    }
    let open = tape.to_vec().unwrap();
    drop(w);
    assert_eq!(&open[last..last + 4], b"WWTP");
    let resume = |bytes: &[u8]| {
        let mut sink = MemSink::new();
        sink.write_all(bytes).unwrap();
        let tape = sink.tape();
        let r = TapeWriter::resume_on(sink, options(0)).map(|(_, r)| r);
        (r, tape.to_vec().unwrap())
    };
    let mut rotted = open.clone();
    rotted[last + 9] ^= 0x01;
    match resume(&rotted) {
        (Err(WriteError::Resume(ResumeError::Damaged { offset })), after) => {
            assert_eq!(offset, last as u64);
            assert_eq!(after, rotted, "the bytes stay");
        }
        (other, _) => panic!("{other:?}"),
    }
    // the same with its CRC rotted too, and a block start after it
    let n = rotted.len();
    rotted[n - 1] ^= 0x01;
    rotted.extend_from_slice(b"WWTP");
    match resume(&rotted) {
        (Err(WriteError::Resume(ResumeError::Damaged { offset })), after) => {
            assert_eq!(offset, last as u64);
            assert_eq!(after, rotted, "the bytes stay");
        }
        (other, _) => panic!("{other:?}"),
    }
}

/// A closed piece followed by what a crash leaves (a torn block, blank bytes): the resume cuts off only what follows
/// the tail and writes on after it; the bytes up to the tail stay. Garbage or a good block after the tail refuses.
#[test]
fn a_resume_of_a_whole_close_cuts_only_the_junk_after_its_tail() {
    let dir = scratch("junk_after_tail");
    let path = dir.join("p.wwt");
    let mut w = ww_tape_fs::writer::create(&path, SyncPolicy::None).unwrap();
    for n in 0..6 {
        let block = records(w.next_id(), n);
        w.write_block(base_defs(), &block).unwrap();
    }
    let next = w.next_id();
    w.close().unwrap();
    let done = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    let good = records(next, 9);
    // the piece ends on 4 KiB: a crash that cut this block's write off leaves zeros from a sector boundary on
    let long = crate::puts::long_records(next, 2, 2000);
    assert_eq!(done.len() % 512, 0);
    for (name, junk) in [
        ("blank bytes", vec![0; 100]),
        ("a torn block", good[..40].to_vec()),
        (
            "a torn block run into blank bytes from a sector boundary",
            [&long[..512], &[0; 4096][..]].concat(),
        ),
    ] {
        std::fs::write(&path, [&done[..], &junk[..]].concat()).unwrap();
        let (w, resumed) = ww_tape_fs::writer::resume(&path, WriterOptions::default())
            .unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert_eq!(resumed.kept_bytes, done.len() as u64, "{name}");
        assert!(resumed.after_close.is_some(), "{name}");
        w.close().unwrap();
        let got = std::fs::read(&path).unwrap();
        assert_eq!(got, done, "{name}: the junk is cut, nothing appended");
    }
    let bytes = [&done[..], &[0xab; 100][..]].concat();
    std::fs::write(&path, &bytes).unwrap();
    match ww_tape_fs::writer::resume(&path, WriterOptions::default()) {
        Err(WriteError::Resume(ResumeError::Damaged { offset })) => {
            assert_eq!(offset, done.len() as u64);
        }
        other => panic!("{:?}", other.map(|(_, r)| r)),
    }
    assert_eq!(
        std::fs::read(&path).unwrap(),
        bytes,
        "garbage: the bytes stay"
    );
    let bytes = [&done[..], &good[..]].concat();
    std::fs::write(&path, &bytes).unwrap();
    match ww_tape_fs::writer::resume(&path, WriterOptions::default()) {
        Err(WriteError::Resume(ResumeError::GoodBlockAfterCut { offset })) => {
            assert_eq!(offset, done.len() as u64);
        }
        other => panic!("{:?}", other.map(|(_, r)| r)),
    }
    assert_eq!(std::fs::read(&path).unwrap(), bytes, "left as it was");
}

/// A piece a cut closed, naming the piece after it: a resume never writes on in it ([ResumeError::Continued] names
/// the next piece). A torn block or blank bytes after its tail are cut off first; garbage there is damage, and an
/// append head a good block: both refuse, every byte kept.
#[test]
fn a_resume_of_a_piece_a_cut_closed_names_the_next() {
    let dir = scratch("cut_closed");
    let owner = OwnerId([7; 32]);
    let mut c = ww_tape_fs::chain::create(&dir, owner, 0, &[], chain_options()).unwrap();
    for n in 0..3 {
        let block = records(c.writer().unwrap().next_id(), n);
        c.write_block(base_defs(), &block).unwrap();
    }
    let good = records(c.writer().unwrap().next_id(), 9);
    c.cut_now(base_defs()).unwrap();
    drop(c);
    let done = std::fs::read(dir.join(piece_name(0))).unwrap();
    let r = TapeReader::open(&done[..]).unwrap();
    let next = r.pieces()[0].close.as_ref().unwrap().0.next.unwrap();
    let resume = |bytes: &[u8]| {
        let mut sink = MemSink::new();
        sink.write_all(bytes).unwrap();
        let tape = sink.tape();
        let r = TapeWriter::resume_on(sink, chain_options().writer).map(|(_, r)| r);
        (r, tape.to_vec().unwrap())
    };
    for (name, after) in [
        ("nothing", vec![]),
        ("a torn block", good[..40].to_vec()),
        ("blank bytes", vec![0; 100]),
    ] {
        match resume(&[&done[..], &after[..]].concat()) {
            (Err(WriteError::Resume(ResumeError::Continued { next: n })), bytes) => {
                assert_eq!(n, next, "{name}");
                assert_eq!(bytes, done, "{name}");
            }
            (other, _) => panic!("{name}: {other:?}"),
        }
    }
    // an append head after it, naming its close: no writer goes on in a piece a cut closed, and the head, a good
    // block, is not cut off
    let (close_at, close) = r.closes(0).unwrap().pop().unwrap();
    let tail_at = done.len() as u64 - ww_tape::piece::TAIL_LEN as u64;
    let mut head = vec![0u8; ww_tape::piece::APPEND_HEAD_LEN];
    let tail = ww_tape::Block::read(&done[tail_at as usize..]).unwrap();
    ww_tape::piece::AppendHead {
        close_offset: close_at,
        tail_offset: tail_at,
        hash: close.hash,
    }
    .write(&mut head, ww_tape::BlockId::new(tail.session, tail.seq));
    let appended = [&done[..], &head[..]].concat();
    match resume(&appended) {
        (Err(WriteError::Resume(ResumeError::GoodBlockAfterCut { offset })), bytes) => {
            assert_eq!(offset, done.len() as u64);
            assert_eq!(bytes, appended, "the bytes stay");
        }
        (other, _) => panic!("{other:?}"),
    }
    let garbage = [&done[..], &[0xab; 100][..]].concat();
    match resume(&garbage) {
        (Err(WriteError::Resume(ResumeError::Damaged { offset })), bytes) => {
            assert_eq!(offset, done.len() as u64);
            assert_eq!(bytes, garbage, "the bytes stay");
        }
        (other, _) => panic!("{other:?}"),
    }
}

/// A piece whose close and tail read whole: a resume writes on after its close, every byte before the append head as
/// it was, in a file and in memory.
#[test]
fn a_resume_of_a_whole_close_writes_on_after_it() {
    let dir = scratch("whole_close");
    let path = dir.join("p.wwt");
    let mut w = ww_tape_fs::writer::create(&path, SyncPolicy::None).unwrap();
    for n in 0..6 {
        let block = records(w.next_id(), n);
        w.write_block(base_defs(), &block).unwrap();
    }
    w.close().unwrap();
    let done = std::fs::read(&path).unwrap();
    let (mut w, resumed) = ww_tape_fs::writer::resume(&path, WriterOptions::default()).unwrap();
    assert!(resumed.after_close.is_some());
    let block = records(w.next_id(), 6);
    w.write_block(base_defs(), &block).unwrap();
    w.close().unwrap();
    let got = std::fs::read(&path).unwrap();
    assert_eq!(got[..done.len()], done[..], "bytes before as they were");
    assert_eq!(got[done.len() + 5], BlockKind::AppendHead.to_u8());
    let mut sink = MemSink::new();
    sink.write_all(&done).unwrap();
    let tape = sink.tape();
    let (mut w, _) = TapeWriter::resume_on(sink, WriterOptions::default()).unwrap();
    let block = records(w.next_id(), 6);
    w.write_block(base_defs(), &block).unwrap();
    w.close().unwrap();
    assert_eq!(tape.to_vec().unwrap(), got);
    // a chain of files, a crash after a cut's close and before the next piece's file: the next one starts
    let chain = dir.join("chain");
    std::fs::create_dir(&chain).unwrap();
    let owner = OwnerId([9; 32]);
    let mut c = ww_tape_fs::chain::create(&chain, owner, 0, &[], chain_options()).unwrap();
    for n in 0..3 {
        let block = records(c.writer().unwrap().next_id(), n);
        c.write_block(base_defs(), &block).unwrap();
    }
    c.cut_now(base_defs()).unwrap();
    drop(c);
    let first = chain.join(piece_name(0));
    let done = std::fs::read(&first).unwrap();
    std::fs::remove_file(chain.join(piece_name(1))).unwrap();
    let (c, _) = ww_tape_fs::chain::resume(&chain, owner, 0, chain_options()).unwrap();
    assert_eq!((c.closed().len(), c.seq()), (1, 1));
    assert_eq!(std::fs::read(&first).unwrap(), done, "bytes as they were");
    drop(c);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Past where the reader stopped, more places of the piece's session that don't read than a resume reads whole, then a
/// good block; or a good block farther than a resume looks: it can't tell the bytes after hold none, so it refuses and
/// leaves them. As many such places as it reads, and nothing after them, are a torn end: cut off.
#[test]
fn a_resume_refuses_where_its_look_is_spent() {
    let mut w = begin(0);
    let tape = w.sink().tape();
    let defs = base_defs();
    for n in 0..6 {
        let block = records(w.next_id(), n);
        w.write_block(defs, &block).unwrap();
    }
    let good = records(w.next_id(), 77);
    let open = tape.to_vec().unwrap();
    drop(w);
    let mut fake = good.clone();
    *fake.last_mut().unwrap() ^= 0xff;
    let resume = |bytes: &[u8]| {
        let mut sink = MemSink::new();
        sink.write_all(bytes).unwrap();
        let tape = sink.tape();
        let r = TapeWriter::resume_on(sink, options(0)).map(|(_, r)| r);
        (r, tape.to_vec().unwrap())
    };
    // as many places as it reads whole, then a good block
    let mut bytes = open.clone();
    bytes.extend_from_slice(&[0xab; 7]);
    for _ in 0..ww_tape::writer::RESUME_PROBES_MAX {
        bytes.extend_from_slice(&fake);
    }
    let good_at = bytes.len() as u64;
    bytes.extend_from_slice(&good);
    match resume(&bytes) {
        (Err(WriteError::Resume(ResumeError::LookSpent { offset })), after) => {
            assert_eq!(offset, good_at);
            assert_eq!(after, bytes, "the bytes stay");
        }
        (other, _) => panic!("{other:?}"),
    }
    // the same places alone: damage, not a torn end
    match resume(&bytes[..good_at as usize]) {
        (Err(WriteError::Resume(ResumeError::Damaged { offset })), after) => {
            assert_eq!(offset, open.len() as u64);
            assert_eq!(after, bytes[..good_at as usize], "the bytes stay");
        }
        (other, _) => panic!("{other:?}"),
    }
    // a torn block after them: cut off
    let (r, after) = resume(&[&open[..], &good[..good.len() - 1]].concat());
    assert_eq!(r.unwrap().kept_bytes, open.len() as u64);
    assert_eq!(after, open);
    // a good block past the bytes a resume looks through
    let mut far = open.clone();
    far.resize(open.len() + ww_tape::writer::RESUME_LOOK_MAX as usize, 0);
    far.extend_from_slice(&good);
    match resume(&far) {
        (Err(WriteError::Resume(ResumeError::LookSpent { offset })), after) => {
            assert_eq!(offset, open.len() as u64 + ww_tape::writer::RESUME_LOOK_MAX);
            assert_eq!(after.len(), far.len(), "the bytes stay");
        }
        (other, _) => panic!("{other:?}"),
    }
}

/// A block early in the piece whose length damage made claim more than the content-hash walk may miss: blocks written
/// after the resume would be bytes no block explains to a reader's walk, so the resume refuses and leaves the bytes. A
/// claim within the walk's budget resumes, and the closed piece verifies.
#[test]
fn a_resume_refuses_where_the_hash_walk_is_spent() {
    let mut w = begin(0);
    let tape = w.sink().tape();
    let defs = base_defs();
    for n in 0..24 {
        let block = records(w.next_id(), n);
        w.write_block(defs, &block).unwrap();
    }
    let good = tape.to_vec().unwrap();
    drop(w);
    let starts: Vec<usize> = (0..good.len() - 3)
        .filter(|&i| good[i..i + 4] == *b"WWTP")
        .collect();
    let bad = starts[2];
    let budget = ww_tape::piece::WALK_MISS_BASE_MAX as u32;
    for (claim, spent) in [(budget * 4, true), (budget / 16, false)] {
        let mut bytes = good.clone();
        bytes[bad + 8..bad + 12].copy_from_slice(&claim.to_le_bytes());
        let mut sink = MemSink::new();
        sink.write_all(&bytes).unwrap();
        let tape = sink.tape();
        match (TapeWriter::resume_on(sink, options(0)), spent) {
            (Err(WriteError::Resume(ResumeError::HashWalkSpent { end })), true) => {
                assert_eq!(end, bytes.len() as u64);
                assert_eq!(tape.to_vec().unwrap(), bytes, "the bytes stay");
            }
            (Ok((mut w, resumed)), false) => {
                assert_eq!(resumed.kept_bytes, bytes.len() as u64);
                let block = records(w.next_id(), 99);
                w.write_block(defs, &block).unwrap();
                w.close().unwrap();
                let out = tape.to_vec().unwrap();
                let r = TapeReader::open(&out[..]).unwrap();
                assert!(
                    r.verify(r.pieces().len() - 1).unwrap().holds,
                    "the content hash verifies"
                );
            }
            (other, _) => panic!("claim {claim}: {:?}", other.map(|(_, r)| r)),
        }
    }
}

/// `bytes` with the integrity byte of the piece head at their start set to `level`, its CRC made good again.
fn with_head_level(bytes: &[u8], level: u8) -> Vec<u8> {
    let mut b = bytes.to_vec();
    assert_eq!(b[5], BlockKind::PieceHead.to_u8());
    b[146] = level;
    let len = ww_tape::block_len(&b).unwrap();
    let crc = crc::Crc::<u32>::new(&crc::CRC_32_ISCSI).checksum(&b[..len - 4]);
    b[len - 4..len].copy_from_slice(&crc.to_le_bytes());
    b
}

/// A piece whose head names an integrity level this build doesn't know, its CRC good, is listed with that error
/// and refused with it by every lookup, the chain check and a resume: never read as damage or as a piece without a
/// head, never appended to.
#[test]
fn a_piece_of_an_unknown_integrity_level_is_refused_everywhere() {
    use ww_tape::ReadError;
    use ww_tape::index::IndexError;
    let levels = [
        (3, IndexError::UnknownIntegrity { level: 3 }),
        (9, IndexError::UnknownIntegrity { level: 9 }),
    ];
    for closed in [false, true] {
        let mut w = begin(0);
        let tape = w.sink().tape();
        for k in 0..6 {
            let id = w.next_id();
            w.write_block(base_defs(), &records(id, k)).unwrap();
        }
        if closed {
            drop(w.close().unwrap());
        } else {
            w.flush_deferred().unwrap();
            drop(w);
        }
        let good = tape.to_vec().unwrap();
        for (level, error) in levels {
            let bytes = with_head_level(&good, level);
            let r = TapeReader::open(&bytes[..]).unwrap();
            let p = &r.pieces()[0];
            assert!(p.head.is_none(), "{level}");
            assert_eq!(p.head_error, Some(error), "{level}");
            let refused =
                |e: Result<_, ReadError<_>>| matches!(e, Err(ReadError::Index(e)) if e == error);
            assert!(
                refused(r.verify(0).map(|v| v.holds).map(|_| ())),
                "{level}: verify"
            );
            assert!(refused(r.entries(0).map(|_| ())), "{level}: entries");
            assert!(refused(
                r.range_hash(
                    TrackId::new(1),
                    OwnerId::NONE,
                    ItemNum::new(0)..ItemNum::new(u64::MAX),
                    ww_tape::TrackHash::START
                )
                .map(|_| ())
            ));
            let chain = r.chain();
            assert_eq!(chain.refused, [0], "{level}: {chain:?}");
            assert!(!chain.is_whole());
            let mut sink = MemSink::new();
            sink.write_all(&bytes).unwrap();
            let resumed = TapeWriter::resume_on(sink, options(0)).map(|(_, r)| r);
            assert!(
                matches!(resumed, Err(WriteError::Index(e) | WriteError::Read(ReadError::Index(e))) if e == error),
                "{level} closed {closed}: {resumed:?}"
            );
        }
    }
}

/// A one-file chain whose closed last piece has a head of an unknown integrity level is refused with that level by
/// a resume, which would start a new piece behind it.
#[test]
fn a_chain_whose_last_piece_is_of_an_unknown_level_is_refused() {
    use ww_tape::index::IndexError;
    let dir = scratch("other_level");
    let at = dir.join("one.wwt");
    let mut c =
        ww_tape_fs::chain::create(&at, OwnerId([9; 32]), 0, &[], one_file_options()).unwrap();
    let mut defs = base_defs().clone();
    for op in ops(14) {
        apply_chain(&mut c, &mut defs, op);
    }
    c.close().unwrap();
    let bytes = std::fs::read(&at).unwrap();
    let last = TapeReader::open(&bytes[..])
        .unwrap()
        .pieces()
        .last()
        .unwrap()
        .start as usize;
    let changed = [&bytes[..last], &with_head_level(&bytes[last..], 9)].concat();
    std::fs::write(&at, &changed).unwrap();
    let error = IndexError::UnknownIntegrity { level: 9 };
    let resumed =
        ww_tape_fs::chain::resume(&at, OwnerId([9; 32]), 0, one_file_options()).map(|(_, r)| r);
    assert!(
        matches!(resumed, Err(WriteError::Index(e) | WriteError::Read(ww_tape::ReadError::Index(e))) if e == error),
        "{resumed:?}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A piece of `level` holding the first half of [ops], closed with a summary, then written on after its close with
/// the second half and closed again: two closes, an append head and a close summary.
pub(crate) fn written_on(alignment: u32, level: ww_tape::Integrity) -> Vec<u8> {
    let all = ops(6);
    let (first, second) = all.split_at(all.len() / 2);
    let mut w = begin_at(alignment, level);
    let mut defs = base_defs().clone();
    for op in first {
        apply(&mut w, &mut defs, *op);
    }
    let tape = w.sink().tape();
    w.close_with_summary(ww_tape::piece::CloseReason::Closed, b"first half")
        .unwrap();
    let mem = MemPieces::new();
    let mut sink = mem.create(&0).unwrap();
    sink.write_all(&tape.to_vec().unwrap()).unwrap();
    sink.close().unwrap();
    drop(sink);
    let sink = mem.reopen(&0).unwrap().unwrap();
    let (mut w, resumed) = TapeWriter::resume_on(sink, options(alignment)).unwrap();
    assert!(resumed.after_close.is_some(), "written on after its close");
    for op in second {
        apply(&mut w, &mut defs, *op);
    }
    w.close().unwrap();
    mem.get(&0).unwrap().unwrap().to_vec().unwrap()
}

/// A piece without a head whose blocks are all puts, cut inside its last block: the resume takes the session of its
/// blocks, so a put after it lies in that session and a reader takes it.
#[test]
fn a_piece_of_puts_alone_resumes_in_its_session() {
    let mut defs = base_defs().clone();
    crate::definitions::hold(&mut defs, &[state(10)]).unwrap();
    let on = TrackNum::new(10);
    let o = crate::definitions::options(u64::MAX);
    let mut w = TapeWriter::create_on(MemSink::new(), o.sync);
    w.set_index_policy(o.index).unwrap();
    w.set_layout(o.layout).unwrap();
    w.set_producer(&o.producer);
    w.set_session(ww_tape::SessionTag::writable(0x7075_7473))
        .unwrap();
    let tape = w.sink().tape();
    w.put(&defs, on, 0, b"first").unwrap();
    let kept = tape.to_vec().unwrap().len();
    w.put(&defs, on, 1, b"second").unwrap();
    let bytes = tape.to_vec().unwrap();
    let mut sink = MemSink::new();
    sink.write_all(&bytes[..kept + 20]).unwrap();
    let tape = sink.tape();
    let (mut w, resumed) = TapeWriter::resume_on(sink, o).unwrap();
    assert_eq!(resumed.kept_bytes, kept as u64);
    assert!(w.put(&defs, on, 2, b"third").unwrap());
    let bytes = tape.to_vec().unwrap();
    let r = TapeReader::open(&bytes[..]).unwrap();
    let got = r
        .state_at(0, ww_tape::Slot::State { track: on }, u64::MAX)
        .unwrap()
        .item
        .map(|i| i.item);
    assert_eq!(got.as_deref(), Some(&b"third"[..]));
}
