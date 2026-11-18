//! Track relations in a written tape: relation columns in events chunks and the range relation's range in each
//! chunk's index entry, against the values written.

use std::path::Path;

use shrink_wrap::prelude::*;
use ww_tape::times::{EncodedRelation, EncodedTimes, RelationColumn};
use ww_tape::{
    BlockKind, Definitions, Describe, DescribeError, MemSink, RelationDef, RelationIndex,
    RelationTarget, RelationValues, SyncPolicy, TapeReader, TapeWriter, TrackDef, WriteError,
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

fn decoded() -> Describe<'static> {
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
        let source = RelationColumn::read(&chunk, 0).unwrap().unwrap();
        let key = RelationColumn::read(&chunk, 2).unwrap().unwrap();
        assert!(
            RelationColumn::read(&chunk, 1).unwrap().is_none(),
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
