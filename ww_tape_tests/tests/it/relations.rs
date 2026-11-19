//! Track relations in a written tape: relation columns in events chunks and the range relation's range in each
//! chunk's index entry, against the values written.

use shrink_wrap::prelude::*;
use ww_tape::times::{EncodedRelation, EncodedTimes, RelationColumn, StoredRelations};
use ww_tape::{
    BlockKind, Definitions, Describe, DescribeError, InputError, MemSink, RelationDef,
    RelationIndex, RelationTarget, RelationValues, SyncPolicy, TapeReader, TapeSink, TapeWriter,
    TrackDef, WriteError,
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
) -> Result<(), WriteError<ww_tape::MemError>> {
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
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
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
    w.close().unwrap();
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
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
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
    let write = |c: &mut PieceChain<MemTapes>, k: u64| {
        let (head, cols) = chunk_of(k * 3, &[10 * k, 10 * k + 1, 10 * k + 2], &[5, 4, 3]);
        let cols: Vec<_> = cols.iter().map(|(s, b)| (*s, &b[..])).collect();
        c.write_chunk(&defs, &head, &cols).unwrap();
    };
    let options = ChainOptions::default;
    let mut c = PieceChain::create_on(mem.clone(), OwnerId([9; 32]), 0, &[], options()).unwrap();
    for k in 0..4 {
        write(&mut c, k);
    }
    c.writer().unwrap().flush_deferred().unwrap();
    // a crash: the piece is left unclosed
    drop(c);
    let (mut c, _) = PieceChain::resume_on(mem.clone(), OwnerId([9; 32]), 0, options()).unwrap();
    write(&mut c, 4);
    let done = c.close().unwrap();
    let bytes = mem.get(&done[0].tape).unwrap().unwrap().to_vec().unwrap();
    let want: Vec<_> = (0..5).map(|k| Some((10 * k, 10 * k + 2))).collect();
    assert_eq!(ranges(&bytes, 0), want);
}

#[test]
fn a_whole_chunk_block_written_or_adopted_keeps_its_range() {
    let mut defs = Definitions::new();
    defs.define_all(&[clock(CLOCK), decoded()]).unwrap();
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    let tape = w.sink().tape();
    write_chunk(&mut w, &defs, 0, &[1, 2, 3], &[9, 8, 7]).unwrap();
    w.close().unwrap();
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
        let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
        let tape = w.sink().tape();
        if adopt {
            w.adopt_block(&defs, block).unwrap();
        } else {
            let mut own = block.to_vec();
            ww_tape::set_id(&mut own, w.next_id()).unwrap();
            w.write_block(&defs, &own).unwrap();
        }
        w.close().unwrap();
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
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
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
    let mut other = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
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
    w.close().unwrap();
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
        let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
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
fn decrease<T: std::fmt::Debug, E>(r: Result<T, WriteError<E>>, item: u64) -> bool {
    matches!(
        r,
        Err(WriteError::Describe(DescribeError::RelationDecrease { track: 3, relation: 0, item: i })) if i == item
    )
}

#[test]
fn a_monotone_relation_never_decreases_across_chunks() {
    let mut defs = Definitions::new();
    defs.define_all(&[clock(CLOCK), decoded()]).unwrap();
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
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
        track_id: crate::definitions::track_id(4),
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
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    w.set_layout(ww_tape::Layout::ALIGNED).unwrap();
    let tape = w.sink().tape();
    w.defer_block(&defs, long_block()).unwrap();
    w.defer_block(&defs, chunk_block(w.next_id(), 0, &[100, 101], &[1, 1]))
        .unwrap();
    assert!(decrease(write_chunk(&mut w, &defs, 2, &[5, 6], &[1, 1]), 2));
    write_chunk(&mut w, &defs, 2, &[101, 102], &[1, 1]).unwrap();
    w.close().unwrap();
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

/// A chunk or summary whose head names its track's number under another id than the track's definition is refused
/// by every call taking one: its index entry would list the track under two ids.
#[test]
fn a_chunk_under_another_track_id_is_refused() {
    let mut defs = Definitions::new();
    defs.define_all(&[clock(CLOCK), decoded()]).unwrap();
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    let tape = w.sink().tape();
    write_chunk(&mut w, &defs, 0, &[100, 101], &[1, 1]).unwrap();
    let len = tape.to_vec().unwrap().len();
    let (mut head, cols) = chunk_of(2, &[101, 102], &[1, 1]);
    head.track_id = 0xdead;
    let wrong = |r: Result<(), WriteError<_>>| {
        matches!(
            r,
            Err(WriteError::Describe(DescribeError::TrackId { track: 3, id: 0xdead, defined })) if defined == track_id()
        )
    };
    let refs: Vec<_> = cols.iter().map(|(s, b)| (*s, &b[..])).collect();
    assert!(wrong(w.write_chunk(&defs, &head, &refs).map(|_| ())));
    let specs: Vec<_> = refs.iter().map(|c| c.0).collect();
    let mut block = vec![0u8; ww_tape::chunk::chunk_len(&head, &specs, 0)];
    let n = ww_tape::chunk::write_chunk(&mut block, w.next_id(), &head, &refs, 0).unwrap();
    block.truncate(n);
    assert!(wrong(w.write_block(&defs, &block).map(|_| ())));
    assert!(wrong(w.adopt_block(&defs, &block).map(|_| ())));
    assert!(wrong(w.defer_block(&defs, block)));
    assert_eq!(tape.to_vec().unwrap().len(), len, "nothing written");
    w.close().unwrap();
    let bytes = tape.to_vec().unwrap();
    let ids: Vec<_> = TapeReader::open(&bytes[..])
        .unwrap()
        .entries(0)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == BlockKind::Events)
        .map(|e| e.track_id)
        .collect();
    assert_eq!(ids, [track_id()]);
}

/// Two deferred chunks of a track whose definition names an owner: the second's values below the first's are refused,
/// as for a track of the piece's owner.
#[test]
fn a_deferred_chunk_of_an_owned_track_never_decreases() {
    for owner in [[0u8; 32], [9u8; 32]] {
        let Describe::Track(t) = decoded() else {
            unreachable!()
        };
        let mut defs = Definitions::new();
        defs.define_all(&[clock(CLOCK), Describe::Track(TrackDef { owner, ..t })])
            .unwrap();
        let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
        w.set_layout(ww_tape::Layout::ALIGNED).unwrap();
        w.defer_block(&defs, chunk_block(w.next_id(), 0, &[100, 101], &[1, 1]))
            .unwrap();
        let second = w.defer_block(&defs, chunk_block(w.next_id(), 2, &[5, 6], &[1, 1]));
        assert!(decrease(second, 2), "owner {owner:?}");
        w.close().unwrap();
    }
}

#[test]
fn a_monotone_relation_never_decreases_across_cuts_and_resumes() {
    use ww_tape::{ChainOptions, CutPolicy, MemTapes, OwnerId, PieceChain, PieceTapes};
    let mut defs = Definitions::new();
    defs.define_all(&[clock(CLOCK), decoded()]).unwrap();
    let write = |c: &mut PieceChain<MemTapes>, first: u64, sources: &[u64]| {
        let (head, cols) = chunk_of(first, sources, &vec![1; sources.len()]);
        let cols: Vec<_> = cols.iter().map(|(s, b)| (*s, &b[..])).collect();
        c.write_chunk(&defs, &head, &cols)
    };
    for files in [PieceTapes::Each, PieceTapes::One] {
        let options = || ChainOptions {
            tapes: files,
            cut: CutPolicy {
                chunks: 1,
                ..CutPolicy::default()
            },
            ..ChainOptions::default()
        };
        let mem = MemTapes::new();
        let owner = OwnerId([9; 32]);
        let mut c = PieceChain::create_on(mem.clone(), owner, 0, &[], options()).unwrap();
        write(&mut c, 0, &[100, 101]).unwrap();
        // the next chunk goes into a piece of its own
        assert!(decrease(write(&mut c, 2, &[5, 6]), 2), "{files:?}: cut");
        write(&mut c, 2, &[101, 102]).unwrap();
        // a crash in the open piece: its chunk is read back
        drop(c);
        let (mut c, _) = PieceChain::resume_on(mem.clone(), owner, 0, options()).unwrap();
        assert!(decrease(write(&mut c, 4, &[5, 6]), 4), "{files:?}: resume");
        // a crash right after a cut: the pieces closed before hold the newest values
        c.cut_now().unwrap();
        drop(c);
        let (mut c, _) = PieceChain::resume_on(mem.clone(), owner, 0, options()).unwrap();
        assert!(
            decrease(write(&mut c, 4, &[5, 6]), 4),
            "{files:?}: resume after a cut"
        );
        write(&mut c, 4, &[102, 103]).unwrap();
        c.close().unwrap();
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
fn unread<E>(r: &Result<(), WriteError<E>>) -> bool {
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
/// written unchecked, in the pieces closed before and in the piece resumed.
#[test]
fn every_monotone_relation_keeps_its_newest_value_across_resumes() {
    use ww_tape::index::IndexPolicy;
    use ww_tape::{ChainOptions, CutPolicy, OwnerId, PieceTapes, WriterOptions};
    let mut defs = Definitions::new();
    defs.define_all(&[clock(CLOCK), two_monotone(), track(9, CLOCK)])
        .unwrap();
    fn write<S: TapeSink>(
        w: &mut TapeWriter<S>,
        defs: &Definitions,
        first: u64,
        source: &[u64],
        seq: &[u64],
    ) -> Result<(), WriteError<S::Error>> {
        let (h, cols) = two_chunk(first, source, seq);
        let cols: Vec<_> = cols.iter().map(|(s, b)| (*s, &b[..])).collect();
        w.write_chunk(defs, &h, &cols).map(|_| ())
    }
    let seq_decrease = |r: Result<(), WriteError<_>>, item: u64| {
        matches!(
            r,
            Err(WriteError::Describe(DescribeError::RelationDecrease { track: 3, relation: 1, item: i })) if i == item
        )
    };
    let dir = crate::pieces::scratch("two_monotone");
    let owner = OwnerId([9; 32]);
    for files in [PieceTapes::Each, PieceTapes::One] {
        let at = dir.join(format!("{files:?}"));
        if files == PieceTapes::Each {
            std::fs::create_dir(&at).unwrap();
        }
        let options = || ChainOptions {
            tapes: files,
            cut: CutPolicy::default(),
            ..ChainOptions::default()
        };
        let mut c = ww_tape_fs::chain::create(&at, owner, 0, &[], options()).unwrap();
        write(c.writer().unwrap(), &defs, 0, &[1, 2], &[100, 101]).unwrap();
        c.cut_now().unwrap();
        drop(c);
        let (mut c, _) = ww_tape_fs::chain::resume(&at, owner, 0, options()).unwrap();
        let w = c.writer().unwrap();
        assert!(
            seq_decrease(write(w, &defs, 2, &[3, 4], &[5, 6]), 2),
            "{files:?}"
        );
        write(w, &defs, 2, &[3, 4], &[102, 103]).unwrap();
        c.cut_now().unwrap();
        drop(c);
        // the last chunk of the track, in the piece closed last, doesn't read
        let first = match files {
            PieceTapes::Each => at.join(ww_tape_fs::piece_name(1)),
            PieceTapes::One => at.clone(),
        };
        let mut bytes = std::fs::read(&first).unwrap();
        let start = match files {
            PieceTapes::Each => 0,
            PieceTapes::One => TapeReader::open(&bytes[..]).unwrap().pieces()[1].start as usize,
        };
        damage_last_chunk(&mut bytes[start..]);
        std::fs::write(&first, &bytes).unwrap();
        let (mut c, _) = ww_tape_fs::chain::resume(&at, owner, 0, options()).unwrap();
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
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
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
    let mut sink = MemSink::new();
    sink.write_all(&bytes).unwrap();
    let (mut w, _) = TapeWriter::resume_on(sink, options).unwrap();
    let r = write(&mut w, &defs, 4, &[5, 6], &[200, 201]);
    assert!(unread(&r), "{r:?}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A [MemTapes](ww_tape::MemTapes) that records each read of a closed piece: its key and offset.
#[derive(Clone)]
struct Recorded {
    tapes: ww_tape::MemTapes,
    reads: std::sync::Arc<std::sync::Mutex<Vec<(u64, u64)>>>,
}

struct RecordedTape {
    tape: ww_tape::MemTape,
    key: u64,
    reads: std::sync::Arc<std::sync::Mutex<Vec<(u64, u64)>>>,
}

impl ww_tape::ByteSource for RecordedTape {
    type Error = ww_tape::MemError;

    fn size(&self) -> Result<u64, ww_tape::MemError> {
        ww_tape::ByteSource::size(&self.tape)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), ww_tape::MemError> {
        self.reads.lock().unwrap().push((self.key, offset));
        ww_tape::ByteSource::read_at(&self.tape, offset, buf)
    }
}

impl ww_tape::Tapes for Recorded {
    type Key = u64;
    type Error = ww_tape::MemError;
    type Sink = MemSink;
    type Source = RecordedTape;

    fn key_of(&self, seq: u64) -> u64 {
        seq
    }

    fn create(&mut self, key: &u64) -> Result<MemSink, ww_tape::MemError> {
        self.tapes.create(key)
    }

    fn reopen(&mut self, key: &u64) -> Result<Option<MemSink>, ww_tape::MemError> {
        self.tapes.reopen(key)
    }

    fn closed(&self, key: &u64) -> Result<Option<RecordedTape>, ww_tape::MemError> {
        Ok(self.tapes.closed(key)?.map(|tape| RecordedTape {
            tape,
            key: *key,
            reads: self.reads.clone(),
        }))
    }

    fn create_whole(&mut self, key: &u64) -> Result<MemSink, ww_tape::MemError> {
        self.tapes.create_whole(key)
    }

    fn open_whole(&mut self, key: &u64) -> Result<Option<MemSink>, ww_tape::MemError> {
        self.tapes.open_whole(key)
    }
}

/// A resume of a chain reads a track's last chunk only in the newest piece that holds a chunk of the track: piece 0's
/// last chunk of track 3 is never read when piece 1 holds one, and its damage doesn't refuse the track.
#[test]
fn a_chain_resume_reads_only_the_newest_last_chunk_of_a_track() {
    use ww_tape::{ChainOptions, CutPolicy, OwnerId, PieceChain, PieceTapes};
    let mut defs = Definitions::new();
    defs.define_all(&[clock(CLOCK), two_monotone()]).unwrap();
    let write = |c: &mut PieceChain<Recorded>, first: u64, source: &[u64], seq: &[u64]| {
        let (h, cols) = two_chunk(first, source, seq);
        let cols: Vec<_> = cols.iter().map(|(s, b)| (*s, &b[..])).collect();
        c.writer()
            .unwrap()
            .write_chunk(&defs, &h, &cols)
            .map(|_| ())
    };
    let f = Recorded {
        tapes: ww_tape::MemTapes::new(),
        reads: Default::default(),
    };
    let owner = OwnerId([9; 32]);
    let options = || ChainOptions {
        tapes: PieceTapes::Each,
        cut: CutPolicy::default(),
        ..ChainOptions::default()
    };
    let mut c = PieceChain::create_on(f.clone(), owner, 0, &[], options()).unwrap();
    write(&mut c, 0, &[1, 2], &[100, 101]).unwrap();
    c.cut_now().unwrap();
    write(&mut c, 2, &[3, 4], &[102, 103]).unwrap();
    c.cut_now().unwrap();
    drop(c);
    let first = 0;
    let mut bytes = f.tapes.get(&first).unwrap().unwrap().to_vec().unwrap();
    let chunk = TapeReader::open(&bytes[..])
        .unwrap()
        .entries(0)
        .unwrap()
        .into_iter()
        .rfind(|e| e.kind == BlockKind::Events)
        .unwrap()
        .offset;
    damage_last_chunk(&mut bytes);
    f.tapes.remove(&first).unwrap();
    let mut sink = f.tapes.create(&first).unwrap();
    sink.write_all(&bytes).unwrap();
    sink.close().unwrap();
    drop(sink);
    let (mut c, _) = PieceChain::resume_on(f.clone(), owner, 0, options()).unwrap();
    let reads = f.reads.lock().unwrap().clone();
    assert!(
        !reads.contains(&(first, chunk)),
        "piece 0's last chunk read: {reads:?}"
    );
    assert!(reads.iter().any(|(k, _)| *k == 1), "{reads:?}");
    write(&mut c, 4, &[5, 6], &[104, 105]).unwrap();
    c.close().unwrap();
}

/// Replace closed piece `key` of `f` with `bytes`.
fn replace_piece(f: &ww_tape::MemTapes, key: &u64, bytes: &[u8]) {
    f.remove(key).unwrap();
    let mut sink = f.create(key).unwrap();
    sink.write_all(bytes).unwrap();
    sink.close().unwrap();
}

/// The events entries of track 3 in piece `key` of `f`, and whether each block is marked a relation break.
fn breaks_in(f: &ww_tape::MemTapes, key: &u64) -> Vec<(bool, bool)> {
    let bytes = f.get(key).unwrap().unwrap().to_vec().unwrap();
    let r = TapeReader::open(&bytes[..]).unwrap();
    assert!(r.verify(0).unwrap(), "the piece verifies");
    r.entries(0)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == BlockKind::Events && e.track_number == 3)
        .map(|e| {
            let at = e.offset as usize;
            let b = ww_tape::Block::read(&bytes[at..at + e.len as usize]).unwrap();
            (e.relation_break, b.is_relation_break())
        })
        .collect()
}

/// A track left unread by a damaged last chunk goes on only by the caller's call: the break is accepted for each
/// unread relation from the caller's values, the next chunk is checked against them and marked in its header and its
/// entry, and from it on the order holds again, across a resume too, with nothing taken from before the break.
#[test]
fn a_caller_accepts_a_relation_break_and_the_tape_marks_it() {
    use ww_tape::{ChainOptions, CutPolicy, OwnerId, PieceChain, PieceTapes};
    let mut defs = Definitions::new();
    defs.define_all(&[clock(CLOCK), two_monotone(), track(9, CLOCK)])
        .unwrap();
    let write = |c: &mut PieceChain<ww_tape::MemTapes>, first: u64, source: &[u64], seq: &[u64]| {
        let (h, cols) = two_chunk(first, source, seq);
        let cols: Vec<_> = cols.iter().map(|(s, b)| (*s, &b[..])).collect();
        c.writer()
            .unwrap()
            .write_chunk(&defs, &h, &cols)
            .map(|_| ())
    };
    let f = ww_tape::MemTapes::new();
    let owner = OwnerId([9; 32]);
    let options = || ChainOptions {
        tapes: PieceTapes::Each,
        cut: CutPolicy::default(),
        ..ChainOptions::default()
    };
    let piece = |n: u64| n;
    let mut c = PieceChain::create_on(f.clone(), owner, 0, &[], options()).unwrap();
    write(&mut c, 0, &[10, 20], &[100, 101]).unwrap();
    c.cut_now().unwrap();
    drop(c);
    let mut bytes = f.get(&piece(0)).unwrap().unwrap().to_vec().unwrap();
    damage_last_chunk(&mut bytes);
    replace_piece(&f, &piece(0), &bytes);

    let (mut c, _) = PieceChain::resume_on(f.clone(), owner, 0, options()).unwrap();
    assert!(unread(&write(&mut c, 2, &[30, 31], &[200, 201])));
    let w = c.writer().unwrap();
    assert!(matches!(
        w.accept_relation_break(&defs, 9, 0, 0),
        Err(WriteError::Describe(DescribeError::NotMonotone {
            track: 9,
            relation: 0
        }))
    ));
    assert!(matches!(
        w.accept_relation_break(&defs, 4, 0, 0),
        Err(WriteError::Describe(DescribeError::UnknownTrack {
            track: 4
        }))
    ));
    w.accept_relation_break(&defs, 3, 1, 50).unwrap();
    // the range relation's newest value is in the damaged chunk's entry: 20, until a break goes below it
    let r = write(&mut c, 2, &[0, 1], &[50, 51]);
    assert!(
        matches!(
            r,
            Err(WriteError::Describe(DescribeError::RelationDecrease {
                track: 3,
                relation: 0,
                ..
            }))
        ),
        "{r:?}"
    );
    c.writer()
        .unwrap()
        .accept_relation_break(&defs, 3, 0, 0)
        .unwrap();
    let r = write(&mut c, 2, &[0, 1], &[49, 51]);
    assert!(
        matches!(
            r,
            Err(WriteError::Describe(DescribeError::RelationDecrease {
                track: 3,
                relation: 1,
                ..
            }))
        ),
        "below the caller's value: {r:?}"
    );
    // a cut before the chunk keeps the breaks waiting
    c.cut_now().unwrap();
    write(&mut c, 2, &[0, 1], &[50, 51]).unwrap();
    write(&mut c, 4, &[2, 3], &[52, 53]).unwrap();
    let r = write(&mut c, 6, &[3, 4], &[52, 60]);
    assert!(
        matches!(
            r,
            Err(WriteError::Describe(DescribeError::RelationDecrease {
                track: 3,
                relation: 1,
                ..
            }))
        ),
        "the order holds again: {r:?}"
    );
    c.cut_now().unwrap();
    drop(c);
    assert_eq!(breaks_in(&f, &piece(1)), vec![]);
    assert_eq!(
        breaks_in(&f, &piece(2)),
        vec![(true, true), (false, false)],
        "the chunk after the break is marked, in its entry and its header"
    );
    // a resume takes the values from the break on: piece 0's 20 and 101 don't count
    let (mut c, _) = PieceChain::resume_on(f.clone(), owner, 0, options()).unwrap();
    let r = write(&mut c, 6, &[2, 3], &[53, 54]);
    assert!(
        matches!(
            r,
            Err(WriteError::Describe(DescribeError::RelationDecrease {
                track: 3,
                relation: 0,
                ..
            }))
        ),
        "{r:?}"
    );
    write(&mut c, 6, &[3, 4], &[53, 54]).unwrap();
    c.close().unwrap();
}

/// A caller's chunk block carries the break mark exactly when a break waits for its track.
#[test]
fn a_callers_chunk_carries_the_break_mark_exactly_when_one_waits() {
    let mut defs = Definitions::new();
    defs.define_all(&[clock(CLOCK), two_monotone()]).unwrap();
    let block = |w: &TapeWriter<MemSink>, first: u64, seq: &[u64], mark: bool| {
        let source = [first + 1, first + 2];
        let (h, cols) = two_chunk(first, &source, seq);
        let cols: Vec<_> = cols.iter().map(|(s, b)| (*s, &b[..])).collect();
        let specs: Vec<_> = cols.iter().map(|c| c.0).collect();
        let mut buf = vec![0u8; ww_tape::chunk::chunk_len(&h, &specs, 0)];
        let n = ww_tape::chunk::write_chunk(&mut buf, w.next_id(), &h, &cols, 0).unwrap();
        buf.truncate(n);
        if mark {
            ww_tape::mark_relation_break(&mut buf).unwrap();
        }
        buf
    };
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    let flags = |r: Result<Option<ww_tape::Placed>, WriteError<_>>| {
        matches!(r, Err(WriteError::Input(InputError::Flags { .. })))
    };
    let b = block(&w, 0, &[100, 101], true);
    assert!(flags(w.write_block(&defs, &b)), "a mark without a break");
    let b = block(&w, 0, &[100, 101], false);
    w.write_block(&defs, &b).unwrap();
    w.accept_relation_break(&defs, 3, 1, 0).unwrap();
    let b = block(&w, 2, &[10, 11], false);
    assert!(flags(w.write_block(&defs, &b)), "a break without its mark");
    let b = block(&w, 2, &[10, 11], true);
    w.write_block(&defs, &b).unwrap();
    let b = block(&w, 4, &[12, 13], true);
    assert!(flags(w.write_block(&defs, &b)), "the break is taken");
}

/// A break placed in a piece a crash cut off restarts the track's values when the resume replays it: the values of
/// the pieces before don't come back.
#[test]
fn a_resume_replays_a_relation_break() {
    use ww_tape::{ChainOptions, CutPolicy, OwnerId, PieceChain, PieceTapes};
    let mut defs = Definitions::new();
    defs.define_all(&[clock(CLOCK), two_monotone()]).unwrap();
    let write = |c: &mut PieceChain<ww_tape::MemTapes>, first: u64, source: &[u64], seq: &[u64]| {
        let (h, cols) = two_chunk(first, source, seq);
        let cols: Vec<_> = cols.iter().map(|(s, b)| (*s, &b[..])).collect();
        c.writer()
            .unwrap()
            .write_chunk(&defs, &h, &cols)
            .map(|_| ())
    };
    let owner = OwnerId([9; 32]);
    for files in [PieceTapes::Each, PieceTapes::One] {
        let f = ww_tape::MemTapes::new();
        let options = || ChainOptions {
            tapes: files,
            cut: CutPolicy::default(),
            ..ChainOptions::default()
        };
        let mut c = PieceChain::create_on(f.clone(), owner, 0, &[], options()).unwrap();
        write(&mut c, 0, &[10, 20], &[100, 101]).unwrap();
        c.cut_now().unwrap();
        let w = c.writer().unwrap();
        w.accept_relation_break(&defs, 3, 0, 1).unwrap();
        w.accept_relation_break(&defs, 3, 1, 5).unwrap();
        write(&mut c, 2, &[1, 2], &[5, 6]).unwrap();
        c.writer().unwrap().sync().unwrap();
        drop(c);
        let (mut c, _) = PieceChain::resume_on(f.clone(), owner, 0, options()).unwrap();
        let r = write(&mut c, 4, &[2, 3], &[5, 7]);
        assert!(
            matches!(
                r,
                Err(WriteError::Describe(DescribeError::RelationDecrease {
                    track: 3,
                    relation: 1,
                    ..
                }))
            ),
            "{files:?}: {r:?}"
        );
        write(&mut c, 4, &[2, 3], &[7, 8]).unwrap();
        c.close().unwrap();
    }
}

/// Op `k` on a chain: even a chunk of track 3 of [two_monotone] (3 items, both relations rising), odd a records
/// block on track 9. One counted block each.
fn chain_op(
    c: &mut ww_tape::PieceChain<ww_tape::MemTapes>,
    defs: &Definitions,
    k: u64,
) -> Result<(), WriteError<ww_tape::MemError>> {
    if k.is_multiple_of(2) {
        let f = (k / 2) * 3;
        let (h, cols) = two_chunk(f, &[f, f + 1, f + 2], &[100 + f, 101 + f, 102 + f]);
        let cols: Vec<_> = cols.iter().map(|(s, b)| (*s, &b[..])).collect();
        c.write_chunk(defs, &h, &cols).map(|_| ())
    } else {
        let b = crate::pieces::records(c.writer()?.next_id(), k as i64);
        c.write_block(defs, &b)
    }
}

/// Power loss anywhere in the second piece of a chain, in both layouts ([sweep_cuts]: every byte of block heads and
/// CRCs, a stride through bodies and alignment padding): its first piece has an incomplete row (a damaged record
/// block a resume kept) and a track of two monotone relations. Each cut resumes, writes the rest and closes to the
/// bytes of the piece written without the loss.
#[test]
fn a_chain_piece_with_an_incomplete_row_and_two_monotone_relations_resumes_anywhere() {
    use ww_tape::index::IndexPolicy;
    use ww_tape::{
        ChainOptions, CutPolicy, Layout, MemTapes, OwnerId, PieceChain, PieceTapes, WriterOptions,
    };
    let mut defs = Definitions::new();
    defs.define_all(&[clock(CLOCK), two_monotone(), track(9, CLOCK)])
        .unwrap();
    let owner = OwnerId([4; 32]);
    let piece = |n: u64| n;
    for layout in [Layout::UNALIGNED, Layout::ALIGNED] {
        let options = || ChainOptions {
            tapes: PieceTapes::Each,
            writer: WriterOptions {
                sync: SyncPolicy::None,
                index: IndexPolicy {
                    bytes: 512,
                    interval: None,
                    ..IndexPolicy::default()
                },
                layout,
                producer: "sweep".to_owned(),
                ..WriterOptions::default()
            },
            cut: CutPolicy::default(),
        };
        let f = MemTapes::new();
        let mut c = PieceChain::create_on(f.clone(), owner, 0, &[], options()).unwrap();
        for k in 0..30 {
            chain_op(&mut c, &defs, k).unwrap();
        }
        drop(c);
        // the fourth record block of data damaged: the resume keeps it, its track's row is incomplete
        let mut bytes = f.get(&piece(0)).unwrap().unwrap().to_vec().unwrap();
        let e = TapeReader::open(&bytes[..])
            .unwrap()
            .entries(0)
            .unwrap()
            .into_iter()
            .filter(|e| {
                e.kind == BlockKind::Records
                    && !e.repeat
                    && e.track_id != ww_tape::index::STATE_TRACK_ID
            })
            .nth(3)
            .unwrap();
        bytes[(e.offset + u64::from(e.len)) as usize - 8] ^= 1;
        {
            let mut sink = f.reopen(&piece(0)).unwrap().unwrap();
            sink.truncate(0).unwrap();
            sink.write_all(&bytes).unwrap();
        }
        let (mut c, _) = PieceChain::resume_on(f.clone(), owner, 0, options()).unwrap();
        let n = c.writer().unwrap().next_seq();
        for k in n..32 {
            chain_op(&mut c, &defs, k).unwrap();
        }
        c.cut_now().unwrap();
        let seq0 = c.writer().unwrap().next_seq();
        for k in 32..44 {
            chain_op(&mut c, &defs, k).unwrap();
        }
        let open = f.get(&piece(1)).unwrap().unwrap().to_vec().unwrap();
        c.close().unwrap();
        let fin = f.get(&piece(1)).unwrap().unwrap().to_vec().unwrap();
        let first = f.get(&piece(0)).unwrap().unwrap().to_vec().unwrap();
        let rows = TapeReader::open(&first[..]).unwrap().pieces()[0]
            .close
            .clone()
            .unwrap()
            .1;
        assert!(
            rows.iter().any(|r| r.incomplete),
            "{layout:?}: piece 0 has an incomplete row"
        );
        for (src, from) in [(&open, 0usize), (&fin, open.len())] {
            for cut in sweep_cuts(src, from) {
                let w = MemTapes::new();
                let mut s = w.create(&piece(0)).unwrap();
                s.write_all(&first).unwrap();
                s.close().unwrap();
                drop(s);
                let mut s = w.create(&piece(1)).unwrap();
                s.write_all(&src[..cut]).unwrap();
                drop(s);
                let r = PieceChain::resume_on(w.clone(), owner, 0, options());
                if cut == fin.len() {
                    // the whole closed piece: named closed, the chain ended
                    assert!(
                        matches!(
                            r,
                            Err(WriteError::Ended(ww_tape::piece::CloseReason::Closed))
                        ),
                        "{layout:?}: {:?}",
                        r.err()
                    );
                    assert!(w.get(&piece(1)).unwrap().unwrap().to_vec().unwrap() == fin);
                    continue;
                }
                let (mut c, _) = r.unwrap_or_else(|e| panic!("{layout:?}, cut at {cut}: {e:?}"));
                let n = c.writer().unwrap().next_seq();
                for k in (32 + n - seq0)..44 {
                    chain_op(&mut c, &defs, k)
                        .unwrap_or_else(|e| panic!("{layout:?}, cut at {cut}, op {k}: {e:?}"));
                }
                c.close().unwrap();
                let got = w.get(&piece(1)).unwrap().unwrap().to_vec().unwrap();
                assert!(
                    got == fin,
                    "{layout:?}, cut at {cut} of {}: other bytes",
                    src.len()
                );
            }
        }
    }
}

/// Where a power-loss sweep of `src` from `from` (a block boundary) cuts. Inside a block's body every cut leaves the
/// same truncated block, and inside a run of zeros (a gap between blocks, a chunk's alignment padding) the same
/// zeros: so it cuts at every byte of each block's head (the chunk head whole) and CRC and at the first and last
/// [CUT_EDGE] bytes of each run of zeros, and only every [CUT_STRIDE]th byte of a body or a run between.
fn sweep_cuts(src: &[u8], from: usize) -> Vec<usize> {
    let mut cuts = Vec::new();
    let mut i = from;
    while i < src.len() {
        let len = ww_tape::block_len(&src[i..])
            .ok()
            .filter(|l| i + l <= src.len());
        let Some(len) = len else {
            // a gap: zero bytes up to the next block or the end, thinned below
            let run = src[i..].iter().take_while(|b| **b == 0).count();
            assert!(run > 0, "a block or a gap at {i}");
            cuts.extend(i..i + run);
            i += run;
            continue;
        };
        let block = ww_tape::Block::read(&src[i..i + len]).unwrap();
        let head = block
            .chunk()
            .map_or(ww_tape::HEADER_LEN, |c| c.payload_offset());
        let body = i + head..i + len - ww_tape::CRC_LEN;
        cuts.extend(i..body.start);
        cuts.extend(body.clone().step_by(CUT_STRIDE));
        cuts.extend(body.end..i + len);
        i += len;
    }
    cuts.push(src.len());
    // the inside of a run of zeros longer than twice CUT_EDGE: every CUT_STRIDEth byte
    let zero = |q: usize| src.get(q) == Some(&0);
    cuts.into_iter()
        .filter(|&p| {
            if !zero(p) {
                return true;
            }
            let into = (from..p).rev().take_while(|&q| zero(q)).count();
            let left = (p..src.len()).take_while(|&q| zero(q)).count();
            into < CUT_EDGE || left <= CUT_EDGE || (into - CUT_EDGE).is_multiple_of(CUT_STRIDE)
        })
        .collect()
}

/// Cuts at each end of a run of zeros in [sweep_cuts].
const CUT_EDGE: usize = 32;
/// Every how many bytes [sweep_cuts] cuts inside a block's body or a run of zeros: odd, so the cuts fall at every
/// offset modulo the powers of two a layout aligns to.
const CUT_STRIDE: usize = 97;

/// Clear the break mark in the header of the chunk block at `at` in `bytes`, its head CRC and block CRC made good.
fn clear_break_mark(bytes: &mut [u8], at: usize) {
    use crate::range_hash::crc32c;
    let len = ww_tape::block_len(&bytes[at..]).unwrap();
    let po = ww_tape::Block::read(&bytes[at..at + len])
        .unwrap()
        .chunk()
        .unwrap()
        .payload_offset();
    let flags = u16::from_le_bytes([bytes[at + 6], bytes[at + 7]]) & !ww_tape::FLAG_RELATION_BREAK;
    bytes[at + 6..at + 8].copy_from_slice(&flags.to_le_bytes());
    let head = crc32c(&[&bytes[at..at + 28], &bytes[at + 32..at + po]]);
    bytes[at + 28..at + 32].copy_from_slice(&head.to_le_bytes());
    let crc = crc32c(&[&bytes[at..at + len - 4]]);
    bytes[at + len - 4..at + len].copy_from_slice(&crc.to_le_bytes());
}

/// A reader holds a track's monotone relations to their order across chunks: a chunk below the one before is taken
/// only where it is marked a break, and the breaks are reported. With the mark cleared from the chunk and its entry,
/// the content hash resealed, verify refuses the chunk by its columns and the index by its ranges; a relation without
/// a range index only verify can see.
#[test]
fn a_reader_refuses_a_decrease_across_chunks_that_isnt_marked_a_break() {
    use ww_tape::{OwnerId, ReadError};
    let mut defs = Definitions::new();
    defs.define_all(&[clock(CLOCK), two_monotone()]).unwrap();
    let write = |w: &mut TapeWriter<MemSink>, first: u64, source: &[u64], seq: &[u64]| {
        let (h, cols) = two_chunk(first, source, seq);
        let cols: Vec<_> = cols.iter().map(|(s, b)| (*s, &b[..])).collect();
        w.write_chunk(&defs, &h, &cols).unwrap().offset
    };
    // (the second chunk's source, which relations break, the error verify gives with the mark cleared)
    for (source, breaks, relation) in [
        ([1, 2], &[0u16, 1][..], Some(0)),
        ([21, 22], &[1u16][..], Some(1)),
    ] {
        let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
        let tape = w.sink().tape();
        write(&mut w, 0, &[10, 20], &[100, 101]);
        for r in breaks {
            w.accept_relation_break(&defs, 3, *r, 0).unwrap();
        }
        let at = write(&mut w, 2, &source, &[5, 6]);
        w.close().unwrap();
        let mut bytes = tape.to_vec().unwrap();
        let r = TapeReader::open(&bytes[..]).unwrap();
        assert!(r.verify(0).unwrap(), "marked: holds");
        assert_eq!(r.track_entries(0, track_id()).unwrap().len(), 2);
        let marked = r.relation_breaks(track_id(), OwnerId::NONE).unwrap();
        assert_eq!(
            marked
                .iter()
                .map(|(i, e)| (*i, e.offset, e.first_item))
                .collect::<Vec<_>>(),
            vec![(0, at, 2)],
            "the break is reported"
        );
        drop(r);
        clear_break_mark(&mut bytes, at as usize);
        crate::range_hash::rewrite_final_entries_of(&mut bytes, 0, track_id(), |_, e| e[15] &= !8);
        crate::range_hash::reseal(&mut bytes, 0);
        let r = TapeReader::open(&bytes[..]).unwrap();
        let breaks = r.relation_breaks(track_id(), OwnerId::NONE);
        match relation {
            Some(0) => assert!(
                matches!(breaks, Err(ReadError::RelationOrder { .. })),
                "{breaks:?}"
            ),
            _ => assert!(breaks.is_ok_and(|b| b.is_empty())),
        }
        match r.verify(0) {
            Err(ReadError::RelationOrder {
                offset,
                track_id: id,
                relation: got,
            }) => {
                assert_eq!((offset, id, got), (at, track_id(), relation));
            }
            other => panic!("{relation:?}: {other:?}"),
        }
        let entries = r.track_entries(0, track_id());
        match relation {
            Some(0) => assert!(
                matches!(entries, Err(ReadError::RelationOrder { offset, relation: None, .. }) if offset == at),
                "{entries:?}"
            ),
            _ => assert!(entries.is_ok(), "{entries:?}"),
        }
    }
}
