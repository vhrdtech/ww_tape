//! Track relations in a written tape: relation columns in events chunks and the range relation's range in each
//! chunk's index entry, against the values written.

use std::path::Path;

use shrink_wrap::prelude::*;
use ww_tape::times::{EncodedRelation, EncodedTimes, RelationColumn, StoredRelations};
use ww_tape::{
    BlockKind, Definitions, Describe, DescribeError, MemSink, RelationDef, RelationIndex,
    RelationTarget, RelationValues, SyncPolicy, TapeReader, TapeSink, TapeWriter, TrackDef,
    WriteError,
};

use crate::definitions::{CLOCK, clock, track};

const fn rel(
    name: &'static str,
    values: RelationValues,
    monotone: bool,
    index: RelationIndex,
) -> RelationDef<'static> {
    RelationDef {
        name,
        target: Some(RelationTarget {
            run: [3; 16],
            track: 0xc0ffee,
        }),
        values,
        monotone,
        index,
    }
}

/// Track 3: `source` (monotone, range index), `context` (no values), `key` (any values).
const RELATIONS: &[RelationDef<'static>] = &[
    rel("source", RelationValues::Stored, true, RelationIndex::Range),
    rel("context", RelationValues::None, false, RelationIndex::None),
    rel("key", RelationValues::Stored, false, RelationIndex::None),
];

pub(crate) fn decoded() -> Describe<'static> {
    let Describe::Track(t) = track(3, CLOCK) else {
        unreachable!("track() gives a track")
    };
    Describe::Track(TrackDef {
        relations: RefVec::Slice { slice: RELATIONS },
        ..t
    })
}

/// xorshift64*.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) % n
    }
}

/// A chunk of `sources.len()` items of track 3 from item `first` with these relation values: its head and columns.
pub(crate) fn relation_chunk(
    first: u64,
    sources: &[u64],
    keys: &[u64],
) -> (ww_tape::ChunkHead, Vec<(ww_tape::ColumnSpec, Vec<u8>)>) {
    let starts: Vec<i64> = (0..sources.len() as i64)
        .map(|i| first as i64 * 10 + i * 10)
        .collect();
    let times = EncodedTimes::encode(&starts, None, None).unwrap();
    let source = EncodedRelation::encode(0, sources, true).unwrap();
    let key = EncodedRelation::encode(2, keys, false).unwrap();
    let mut cols: Vec<_> = times.columns().map(|(s, b)| (s, b.to_vec())).collect();
    for (s, b) in [source.column(), key.column()] {
        cols.push((s, b.to_vec()));
    }
    (times.head.chunk_head(track_id(), 3, first), cols)
}

/// A chunk of `sources.len()` items of track 3 from item `first` with these relation values; its head and columns.
fn write_chunk(
    w: &mut TapeWriter<MemSink>,
    defs: &Definitions,
    first: u64,
    sources: &[u64],
    keys: &[u64],
) -> Result<(), WriteError> {
    let starts: Vec<i64> = (0..sources.len() as i64)
        .map(|i| first as i64 * 10 + i * 10)
        .collect();
    let times = EncodedTimes::encode(&starts, None, None).unwrap();
    let source = EncodedRelation::encode(0, sources, true).unwrap();
    let key = EncodedRelation::encode(2, keys, false).unwrap();
    let mut cols: Vec<_> = times.columns().collect();
    cols.extend([source.column(), key.column()]);
    let head = times.head.chunk_head(track_id(), 3, first);
    w.write_chunk(defs, &head, &cols).map(|_| ())
}

fn track_id() -> u64 {
    let Describe::Track(t) = decoded() else {
        unreachable!()
    };
    t.id()
}

#[test]
fn relation_columns_and_ranges_read_as_written() {
    let mut defs = Definitions::new();
    defs.define_all(&[clock(CLOCK), decoded()]).unwrap();
    let mut w = TapeWriter::create_on(MemSink::new(Path::new("relations")), SyncPolicy::None);
    let tape = w.sink().tape();
    let mut r = Rng(0x5eed);
    let mut written: Vec<(Vec<u64>, Vec<u64>)> = Vec::new();
    let mut src = 1_000u64;
    for k in 0..20u64 {
        // 10 children per parent at most, some chunks of one item, a long last one
        let n = match k {
            3 => 1,
            19 => 4096,
            _ => 1 + r.below(300) as usize,
        };
        let sources: Vec<u64> = (0..n)
            .map(|_| {
                src += r.below(3);
                src
            })
            .collect();
        let keys: Vec<u64> = (0..n).map(|_| r.below(20) << 40).collect();
        let first = written.iter().map(|w| w.0.len() as u64).sum();
        write_chunk(&mut w, &defs, first, &sources, &keys).unwrap();
        written.push((sources, keys));
    }
    w.finish().unwrap();
    let bytes = tape.to_vec().unwrap();
    let reader = TapeReader::open(&bytes[..]).unwrap();
    let entries: Vec<_> = reader
        .entries(0)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == BlockKind::Events)
        .collect();
    assert_eq!(entries.len(), written.len());
    for (e, (sources, keys)) in entries.iter().zip(&written) {
        assert_eq!(
            e.range,
            Some((sources[0], *sources.last().unwrap())),
            "chunk at {}",
            e.first_item
        );
        let block_bytes = &bytes[e.offset as usize..e.offset as usize + e.len as usize];
        let block = ww_tape::Block::read(block_bytes).unwrap();
        let chunk = block.chunk().unwrap();
        let Describe::Track(def) = decoded() else {
            unreachable!("decoded() gives a track")
        };
        let stored = StoredRelations::of(&def);
        let source = RelationColumn::read(&chunk, stored, 0).unwrap().unwrap();
        let key = RelationColumn::read(&chunk, stored, 2).unwrap().unwrap();
        assert!(
            RelationColumn::read(&chunk, stored, 1).unwrap().is_none(),
            "no column for a relation without values"
        );
        assert_eq!(
            (0..source.len()).map(|i| source.get(i)).collect::<Vec<_>>(),
            *sources
        );
        assert_eq!(
            (0..key.len()).map(|i| key.get(i)).collect::<Vec<_>>(),
            *keys
        );
        // a source item's children: two lower bounds
        let probe = sources[sources.len() / 2];
        let from = source.lower_bound(probe).unwrap();
        let to = source.lower_bound(probe + 1).unwrap();
        assert_eq!(
            from..to,
            sources.partition_point(|v| *v < probe)..sources.partition_point(|v| *v <= probe)
        );
        let sorted = keys.windows(2).all(|w| w[0] <= w[1]);
        assert_eq!(
            key.lower_bound(0).is_some(),
            sorted,
            "a search needs values that never decrease"
        );
    }
}

#[test]
fn a_chunk_without_its_relation_columns_is_refused() {
    let mut defs = Definitions::new();
    defs.define_all(&[clock(CLOCK), decoded()]).unwrap();
    let mut w = TapeWriter::create_on(MemSink::new(Path::new("refused")), SyncPolicy::None);
    let times = EncodedTimes::encode(&[1, 2, 3], None, None).unwrap();
    let head = times.head.chunk_head(track_id(), 3, 0);
    let cols: Vec<_> = times.columns().collect();
    assert!(matches!(
        w.write_chunk(&defs, &head, &cols),
        Err(WriteError::Describe(DescribeError::RelationColumns {
            track: 3
        }))
    ));
    // the writer goes on: a good chunk after it is written
    write_chunk(&mut w, &defs, 0, &[1, 2, 3], &[9, 8, 7]).unwrap();
}

/// The chunk `write_chunk` makes of track 3: its head and columns, for a writer of any kind.
fn chunk_of(
    first: u64,
    sources: &[u64],
    keys: &[u64],
) -> (ww_tape::ChunkHead, Vec<(ww_tape::ColumnSpec, Vec<u8>)>) {
    let starts: Vec<i64> = (0..sources.len() as i64)
        .map(|i| first as i64 * 10 + i * 10)
        .collect();
    let times = EncodedTimes::encode(&starts, None, None).unwrap();
    let source = EncodedRelation::encode(0, sources, true).unwrap();
    let key = EncodedRelation::encode(2, keys, false).unwrap();
    let mut cols: Vec<_> = times.columns().map(|(s, b)| (s, b.to_vec())).collect();
    cols.extend([source.column(), key.column()].map(|(s, b)| (s, b.to_vec())));
    (times.head.chunk_head(track_id(), 3, first), cols)
}

/// The ranges of the events chunks' entries of piece `piece` of `bytes`, in file order.
fn ranges(bytes: &[u8], piece: usize) -> Vec<Option<(u64, u64)>> {
    let reader = TapeReader::open(bytes).unwrap();
    reader
        .entries(piece)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == BlockKind::Events)
        .map(|e| e.range)
        .collect()
}

#[test]
fn a_resume_keeps_the_ranges_written_before_the_crash() {
    use ww_tape::{ChainOptions, MemTapes, OwnerId, PieceChain};
    let mut defs = Definitions::new();
    defs.define_all(&[clock(CLOCK), decoded()]).unwrap();
    let mem = MemTapes::new();
    let dir = Path::new("p");
    let write = |c: &mut PieceChain<MemTapes>, k: u64| {
        let (head, cols) = chunk_of(k * 3, &[10 * k, 10 * k + 1, 10 * k + 2], &[5, 4, 3]);
        let cols: Vec<_> = cols.iter().map(|(s, b)| (*s, &b[..])).collect();
        c.write_chunk(&defs, &head, &cols).unwrap();
    };
    let options = ChainOptions::default;
    let mut c =
        PieceChain::create_on(mem.clone(), dir, OwnerId([9; 32]), 0, &[], options()).unwrap();
    for k in 0..4 {
        write(&mut c, k);
    }
    c.writer().unwrap().flush_deferred().unwrap();
    // a crash: the piece is left unfinished
    drop(c);
    let (mut c, _) =
        PieceChain::resume_on(mem.clone(), dir, OwnerId([9; 32]), 0, options()).unwrap();
    write(&mut c, 4);
    let done = c.finish().unwrap();
    let bytes = mem.get(&done[0].path).unwrap().unwrap().to_vec().unwrap();
    let want: Vec<_> = (0..5).map(|k| Some((10 * k, 10 * k + 2))).collect();
    assert_eq!(ranges(&bytes, 0), want);
}

#[test]
fn a_whole_chunk_block_written_or_adopted_keeps_its_range() {
    let mut defs = Definitions::new();
    defs.define_all(&[clock(CLOCK), decoded()]).unwrap();
    let mut w = TapeWriter::create_on(MemSink::new(Path::new("a")), SyncPolicy::None);
    let tape = w.sink().tape();
    write_chunk(&mut w, &defs, 0, &[1, 2, 3], &[9, 8, 7]).unwrap();
    w.finish().unwrap();
    let bytes = tape.to_vec().unwrap();
    let r = TapeReader::open(&bytes[..]).unwrap();
    let e = r
        .entries(0)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == BlockKind::Events)
        .unwrap();
    let block = &bytes[e.offset as usize..][..e.len as usize];
    for adopt in [false, true] {
        let mut w = TapeWriter::create_on(MemSink::new(Path::new("b")), SyncPolicy::None);
        let tape = w.sink().tape();
        if adopt {
            w.adopt_block(&defs, block).unwrap();
        } else {
            let mut own = block.to_vec();
            ww_tape::set_id(&mut own, w.next_id()).unwrap();
            w.write_block(&defs, &own).unwrap();
        }
        w.finish().unwrap();
        assert_eq!(
            ranges(&tape.to_vec().unwrap(), 0),
            [Some((1, 3))],
            "adopted {adopt}"
        );
    }
}

#[test]
fn a_relation_column_that_does_not_read_is_refused() {
    let mut defs = Definitions::new();
    defs.define_all(&[clock(CLOCK), decoded()]).unwrap();
    let mut w = TapeWriter::create_on(MemSink::new(Path::new("len")), SyncPolicy::None);
    let tape = w.sink().tape();
    let times = EncodedTimes::encode(&[0, 10, 20, 30, 40], None, None).unwrap();
    let head = times.head.chunk_head(track_id(), 3, 0);
    let keys = EncodedRelation::encode(2, &[1, 2, 3, 4, 5], false).unwrap();
    // 2 sources for 5 items: one period that doesn't reach the last value
    let short = EncodedRelation::encode(0, &[10, 11], true).unwrap();
    let mut cols: Vec<_> = times.columns().collect();
    cols.extend([short.column(), keys.column()]);
    let refused = w.write_chunk(&defs, &head, &cols);
    assert!(
        matches!(
            refused,
            Err(WriteError::Describe(DescribeError::RelationColumnRead {
                track: 3,
                relation: 0,
                error: ww_tape::times::TimesError::Corrupt {
                    kind: ww_tape::ColumnKind::Relation,
                    ..
                },
            }))
        ),
        "{refused:?}"
    );
    // the same chunk as a whole block of another writer's
    let mut other = TapeWriter::create_on(MemSink::new(Path::new("other")), SyncPolicy::None);
    let specs: Vec<_> = cols.iter().map(|c| c.0).collect();
    let mut block = vec![0u8; ww_tape::chunk::chunk_len(&head, &specs, 0)];
    let n = ww_tape::chunk::write_chunk(&mut block, other.next_id(), &head, &cols, 0).unwrap();
    block.truncate(n);
    assert!(matches!(
        other.write_block(&defs, &block),
        Err(WriteError::Describe(DescribeError::RelationColumnRead {
            track: 3,
            relation: 0,
            ..
        }))
    ));
    // nothing of the refused chunk is written: a good chunk after it is the first block of data
    write_chunk(&mut w, &defs, 0, &[1, 2, 3, 4, 5], &[9, 8, 7, 6, 5]).unwrap();
    w.finish().unwrap();
    assert_eq!(ranges(&tape.to_vec().unwrap(), 0), [Some((1, 5))]);
}

/// A STARTS relation column of 3 items in one linear group: count, mode 1, first and last (ordered), a 25-byte group
/// record (first, slope 0, rmin, width 8, offset 0), then the residuals.
fn crafted_starts(first: u64, last: u64, rmin: i32, residuals: [u8; 3]) -> Vec<u8> {
    let ord = |v: u64| (v ^ (1 << 63)) as i64;
    let mut b = Vec::new();
    b.extend_from_slice(&3u32.to_le_bytes());
    b.push(1);
    b.extend_from_slice(&ord(first).to_le_bytes());
    b.extend_from_slice(&ord(last).to_le_bytes());
    b.extend_from_slice(&ord(first).to_le_bytes());
    b.extend_from_slice(&0i64.to_le_bytes());
    b.extend_from_slice(&rmin.to_le_bytes());
    b.push(8);
    b.extend_from_slice(&0u32.to_le_bytes());
    b.extend_from_slice(&residuals);
    b
}

#[test]
fn a_callers_monotone_column_out_of_order_is_refused() {
    let mut defs = Definitions::new();
    defs.define_all(&[clock(CLOCK), decoded()]).unwrap();
    let times = EncodedTimes::encode(&[0, 10, 20], None, None).unwrap();
    let head = times.head.chunk_head(track_id(), 3, 0);
    let keys = EncodedRelation::encode(2, &[1, 1, 1], false).unwrap();
    // first 10 and last 20 in the head; the items 10, 3, 20 decrease, 10, 99, 20 leave [first, last]
    for (rmin, residuals) in [(-7, [7u8, 0, 17]), (0, [0u8, 89, 10])] {
        let source = crafted_starts(10, 20, rmin, residuals);
        let mut w = TapeWriter::create_on(MemSink::new(Path::new("starts")), SyncPolicy::None);
        let mut cols: Vec<_> = times.columns().collect();
        let spec = ww_tape::ColumnSpec {
            kind: ww_tape::ColumnKind::Relation,
            codec: ww_tape::times::STARTS,
            param: 0,
            len: source.len(),
        };
        cols.extend([(spec, &source[..]), keys.column()]);
        let refused = w.write_chunk(&defs, &head, &cols);
        assert!(
            matches!(
                refused,
                Err(WriteError::Describe(DescribeError::RelationColumnRead {
                    track: 3,
                    relation: 0,
                    ..
                }))
            ),
            "{residuals:?}: {refused:?}"
        );
    }
}

/// The whole chunk block of [chunk_of], numbered `id`.
fn chunk_block(id: ww_tape::BlockId, first: u64, sources: &[u64], keys: &[u64]) -> Vec<u8> {
    let (head, cols) = chunk_of(first, sources, keys);
    let cols: Vec<_> = cols.iter().map(|(s, b)| (*s, &b[..])).collect();
    let specs: Vec<_> = cols.iter().map(|c| c.0).collect();
    let mut block = vec![0u8; ww_tape::chunk::chunk_len(&head, &specs, 0)];
    let n = ww_tape::chunk::write_chunk(&mut block, id, &head, &cols, 0).unwrap();
    block.truncate(n);
    block
}

/// Whether `r` refuses track 3's chunk from item `item` for a decrease of its monotone relation `source`.
fn decrease<T: std::fmt::Debug>(r: Result<T, WriteError>, item: u64) -> bool {
    matches!(
        r,
        Err(WriteError::Describe(DescribeError::RelationDecrease { track: 3, relation: 0, item: i })) if i == item
    )
}

#[test]
fn a_monotone_relation_never_decreases_across_chunks() {
    let mut defs = Definitions::new();
    defs.define_all(&[clock(CLOCK), decoded()]).unwrap();
    let mut w = TapeWriter::create_on(MemSink::new(Path::new("m")), SyncPolicy::None);
    write_chunk(&mut w, &defs, 0, &[100, 101], &[1, 1]).unwrap();
    assert!(decrease(write_chunk(&mut w, &defs, 2, &[5, 6], &[1, 1]), 2));
    // a whole block, given or adopted, as well
    let block = chunk_block(w.next_id(), 2, &[5, 6], &[1, 1]);
    assert!(decrease(w.write_block(&defs, &block), 2));
    assert!(decrease(w.adopt_block(&defs, &block), 2));
    // the same value again goes on
    write_chunk(&mut w, &defs, 2, &[101, 102], &[1, 1]).unwrap();
    // a block taking the last one's place is checked against the one before that
    let again = chunk_block(
        ww_tape::BlockId::new(w.session(), w.next_seq() - 1),
        2,
        &[101, 101],
        &[2, 2],
    );
    w.write_block(&defs, &again).unwrap();
    let below = chunk_block(
        ww_tape::BlockId::new(w.session(), w.next_seq() - 1),
        2,
        &[100, 101],
        &[2, 2],
    );
    assert!(decrease(w.write_block(&defs, &below), 2));
}

/// A whole chunk block of track 4 of 600 items with raw starts: longer than any gap before an aligned payload.
fn long_block() -> Vec<u8> {
    let starts: Vec<u8> = (0..600i64).flat_map(|i| i.to_le_bytes()).collect();
    let head = ww_tape::ChunkHead {
        kind: BlockKind::Events,
        track_id: 0x4444,
        track_number: 4,
        first_item: 0,
        count: 600,
        first_tick: 0,
        last_tick: 599,
        latest_end: 599,
        time_mode: 0,
        end_mode: ww_tape::EndMode::Absent,
        grid: None,
    };
    let cols = [(
        ww_tape::ColumnSpec {
            kind: ww_tape::ColumnKind::Starts,
            codec: ww_tape::chunk::Codec::RAW,
            param: 0,
            len: starts.len(),
        },
        &starts[..],
    )];
    let mut block = vec![0u8; ww_tape::chunk::chunk_len(&head, &[cols[0].0], 0)];
    let n = ww_tape::chunk::write_chunk(&mut block, crate::id(0), &head, &cols, 0).unwrap();
    block.truncate(n);
    block
}

/// A deferred chunk is placed before any later block of its track, even when it waits behind a block too long for
/// the gap before an aligned payload: the values of its monotone relation count before the chunks after it, and
/// the file holds them in that order.
#[test]
fn a_deferred_chunk_counts_before_the_chunks_after_it() {
    let mut defs = Definitions::new();
    defs.define_all(&[clock(CLOCK), decoded(), track(4, CLOCK)])
        .unwrap();
    let mut w = TapeWriter::create_on(MemSink::new(Path::new("d")), SyncPolicy::None);
    w.set_layout(ww_tape::Layout::ALIGNED).unwrap();
    let tape = w.sink().tape();
    w.defer_block(&defs, long_block()).unwrap();
    w.defer_block(&defs, chunk_block(w.next_id(), 0, &[100, 101], &[1, 1]))
        .unwrap();
    assert!(decrease(write_chunk(&mut w, &defs, 2, &[5, 6], &[1, 1]), 2));
    write_chunk(&mut w, &defs, 2, &[101, 102], &[1, 1]).unwrap();
    w.finish().unwrap();
    let bytes = tape.to_vec().unwrap();
    let firsts: Vec<_> = TapeReader::open(&bytes[..])
        .unwrap()
        .entries(0)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == BlockKind::Events && e.track_number == 3)
        .map(|e| (e.first_item, e.range))
        .collect();
    assert_eq!(firsts, [(0, Some((100, 101))), (2, Some((101, 102)))]);
}

#[test]
fn a_monotone_relation_never_decreases_across_cuts_and_resumes() {
    use ww_tape::{ChainOptions, CutPolicy, MemTapes, OwnerId, PieceChain, PieceFiles};
    let mut defs = Definitions::new();
    defs.define_all(&[clock(CLOCK), decoded()]).unwrap();
    let write = |c: &mut PieceChain<MemTapes>, first: u64, sources: &[u64]| {
        let (head, cols) = chunk_of(first, sources, &vec![1; sources.len()]);
        let cols: Vec<_> = cols.iter().map(|(s, b)| (*s, &b[..])).collect();
        c.write_chunk(&defs, &head, &cols)
    };
    for files in [PieceFiles::Each, PieceFiles::One] {
        let options = || ChainOptions {
            files,
            cut: CutPolicy {
                chunks: 1,
                ..CutPolicy::default()
            },
            ..ChainOptions::default()
        };
        let mem = MemTapes::new();
        let at = Path::new("c");
        let owner = OwnerId([9; 32]);
        let mut c = PieceChain::create_on(mem.clone(), at, owner, 0, &[], options()).unwrap();
        write(&mut c, 0, &[100, 101]).unwrap();
        // the next chunk goes into a piece of its own
        assert!(decrease(write(&mut c, 2, &[5, 6]), 2), "{files:?}: cut");
        write(&mut c, 2, &[101, 102]).unwrap();
        // a crash in the open piece: its chunk is read back
        drop(c);
        let (mut c, _) = PieceChain::resume_on(mem.clone(), at, owner, 0, options()).unwrap();
        assert!(decrease(write(&mut c, 4, &[5, 6]), 4), "{files:?}: resume");
        // a crash right after a cut: the pieces finished before hold the newest values
        c.cut_now().unwrap();
        drop(c);
        let (mut c, _) = PieceChain::resume_on(mem.clone(), at, owner, 0, options()).unwrap();
        assert!(
            decrease(write(&mut c, 4, &[5, 6]), 4),
            "{files:?}: resume after a cut"
        );
        write(&mut c, 4, &[102, 103]).unwrap();
        c.finish().unwrap();
    }
}

/// Track 3 with two monotone relations: `source` feeds the range index, `seq` has none.
const TWO_MONOTONE: &[RelationDef<'static>] = &[
    rel("source", RelationValues::Stored, true, RelationIndex::Range),
    rel("seq", RelationValues::Stored, true, RelationIndex::None),
];

fn two_monotone() -> Describe<'static> {
    let Describe::Track(t) = track(3, CLOCK) else {
        unreachable!("track() gives a track")
    };
    Describe::Track(TrackDef {
        relations: RefVec::Slice {
            slice: TWO_MONOTONE,
        },
        ..t
    })
}

/// A chunk of track 3 of [two_monotone] from item `first`: its head and columns.
fn two_chunk(
    first: u64,
    source: &[u64],
    seq: &[u64],
) -> (ww_tape::ChunkHead, Vec<(ww_tape::ColumnSpec, Vec<u8>)>) {
    let starts: Vec<i64> = (0..source.len() as i64)
        .map(|i| first as i64 * 10 + i * 10)
        .collect();
    let times = EncodedTimes::encode(&starts, None, None).unwrap();
    let a = EncodedRelation::encode(0, source, true).unwrap();
    let b = EncodedRelation::encode(1, seq, true).unwrap();
    let mut cols: Vec<_> = times.columns().map(|(s, b)| (s, b.to_vec())).collect();
    cols.extend([a.column(), b.column()].map(|(s, b)| (s, b.to_vec())));
    (times.head.chunk_head(track_id(), 3, first), cols)
}

/// Flip a byte in the payload of the last events chunk of the piece in `bytes`: its CRC fails, its head and its index
/// entry stay.
fn damage_last_chunk(bytes: &mut [u8]) {
    let e = TapeReader::open(&bytes[..])
        .unwrap()
        .entries(0)
        .unwrap()
        .into_iter()
        .rfind(|e| e.kind == BlockKind::Events)
        .unwrap();
    bytes[(e.offset + u64::from(e.len)) as usize - 8] ^= 1;
}

/// Whether `r` refuses track 3's chunk for monotone relation `seq`, whose newest value is in a chunk that doesn't read.
fn unread(r: &Result<(), WriteError>) -> bool {
    matches!(
        r,
        Err(WriteError::Describe(DescribeError::RelationUnread {
            track: 3,
            relation: 1
        }))
    )
}

/// Every monotone relation keeps its newest value across a resume of a chain, the one without an index too: a resume
/// reads the last chunk of the track. Where that chunk doesn't read, a chunk of the track is refused rather than
/// written unchecked, in the pieces finished before and in the piece resumed.
#[test]
fn every_monotone_relation_keeps_its_newest_value_across_resumes() {
    use ww_tape::index::IndexPolicy;
    use ww_tape::{ChainOptions, CutPolicy, OwnerId, PieceChain, PieceFiles, WriterOptions};
    let mut defs = Definitions::new();
    defs.define_all(&[clock(CLOCK), two_monotone(), track(9, CLOCK)])
        .unwrap();
    fn write<S: TapeSink>(
        w: &mut TapeWriter<S>,
        defs: &Definitions,
        first: u64,
        source: &[u64],
        seq: &[u64],
    ) -> Result<(), WriteError> {
        let (h, cols) = two_chunk(first, source, seq);
        let cols: Vec<_> = cols.iter().map(|(s, b)| (*s, &b[..])).collect();
        w.write_chunk(defs, &h, &cols).map(|_| ())
    }
    let seq_decrease = |r: Result<(), WriteError>, item: u64| {
        matches!(
            r,
            Err(WriteError::Describe(DescribeError::RelationDecrease { track: 3, relation: 1, item: i })) if i == item
        )
    };
    let dir = crate::pieces::scratch("two_monotone");
    let owner = OwnerId([9; 32]);
    for files in [PieceFiles::Each, PieceFiles::One] {
        let at = dir.join(format!("{files:?}"));
        if files == PieceFiles::Each {
            std::fs::create_dir(&at).unwrap();
        }
        let options = || ChainOptions {
            files,
            cut: CutPolicy::default(),
            ..ChainOptions::default()
        };
        let mut c = PieceChain::create(&at, owner, 0, &[], options()).unwrap();
        write(c.writer().unwrap(), &defs, 0, &[1, 2], &[100, 101]).unwrap();
        c.cut_now().unwrap();
        drop(c);
        let (mut c, _) = PieceChain::resume(&at, owner, 0, options()).unwrap();
        let w = c.writer().unwrap();
        assert!(
            seq_decrease(write(w, &defs, 2, &[3, 4], &[5, 6]), 2),
            "{files:?}"
        );
        write(w, &defs, 2, &[3, 4], &[102, 103]).unwrap();
        c.cut_now().unwrap();
        drop(c);
        // the last chunk of the track, in the piece finished last, doesn't read
        let first = match files {
            PieceFiles::Each => at.join(ww_tape::piece::piece_name(1)),
            PieceFiles::One => at.clone(),
        };
        let mut bytes = std::fs::read(&first).unwrap();
        let start = match files {
            PieceFiles::Each => 0,
            PieceFiles::One => TapeReader::open(&bytes[..]).unwrap().pieces()[1].start as usize,
        };
        damage_last_chunk(&mut bytes[start..]);
        std::fs::write(&first, &bytes).unwrap();
        let (mut c, _) = PieceChain::resume(&at, owner, 0, options()).unwrap();
        let r = write(c.writer().unwrap(), &defs, 4, &[5, 6], &[200, 201]);
        assert!(unread(&r), "{files:?}: {r:?}");
    }
    // one piece, its last chunk damaged where a checkpoint lists it
    let options = WriterOptions {
        index: IndexPolicy {
            bytes: 256,
            interval: None,
            ..IndexPolicy::default()
        },
        ..WriterOptions::default()
    };
    let mut w = TapeWriter::create_on(MemSink::new(Path::new("one")), SyncPolicy::None);
    w.set_index_policy(options.index).unwrap();
    let tape = w.sink().tape();
    write(&mut w, &defs, 0, &[1, 2], &[100, 101]).unwrap();
    write(&mut w, &defs, 2, &[3, 4], &[102, 103]).unwrap();
    for n in 0..8 {
        let b = crate::pieces::records(w.next_id(), n);
        w.write_block(&defs, &b).unwrap();
    }
    drop(w);
    let mut bytes = tape.to_vec().unwrap();
    damage_last_chunk(&mut bytes);
    let mut sink = MemSink::new(Path::new("two"));
    sink.write_all(&bytes).unwrap();
    let (mut w, _) = TapeWriter::resume_on(sink, options).unwrap();
    let r = write(&mut w, &defs, 4, &[5, 6], &[200, 201]);
    assert!(unread(&r), "{r:?}");
    std::fs::remove_dir_all(&dir).unwrap();
}
