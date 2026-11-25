//! Grid start marks (FLAG_GRID_START): a series' timeline from its index entries, reading a chunk head only where
//! a grid or scale starts, a piece starts or samples are lost; a relay that restamps the marks; marks a hostile
//! writer left out or added.

use std::cell::RefCell;
use std::collections::BTreeMap;

use ww_tape::chunk::{CHUNK_HEAD_PREFIX, ChunkHead};
use ww_tape::index::IndexEntry;
use ww_tape::reader::ByteSource;
use ww_tape::samples::{encode, max_encoded_len};
use ww_tape::{
    Block, BlockKind, ChainOptions, ClockOrigin, ClockSpec, Codec, ColumnKind, ColumnSpec,
    CutPolicy, Definitions, EndMode, FileBlocks, GridDef, ItemNum, Magnitude, MemPieces, MemSink,
    OwnerId, PieceChain, Ratio64, ReadError, SampleShape, Segment, SeriesGrid, SeriesTimeline,
    SeriesValue, TapeReader, TapeWriter, TileCoords, TrackId, TrackKind, TrackNum, TrackSpec,
    clock_key, track_key,
};

/// Samples per second: 48 kHz.
const RATE: u64 = 48_000;
/// Samples of a full chunk: 1 MiB of i16.
const CHUNK: u64 = 1 << 19;
/// Ticks of the gap before a new grid.
const GAP: i64 = 5_000_000;

/// 48 kHz on a 1 ns clock.
fn period() -> Ratio64 {
    Ratio64::new(62_500, 3).unwrap()
}

/// The definitions of one i16 series track at 48 kHz, its number and id.
fn defs() -> (Definitions, TrackNum, TrackId) {
    let mut defs = Definitions::new();
    let clock = defs
        .define_clock(ClockSpec {
            key: clock_key(0x9a, &[], 1),
            rate: Magnitude::pow10(9).into(),
            resolution: Magnitude::pow10(-9).into(),
            origin: ClockOrigin::Boot,
            label: "grid starts",
        })
        .unwrap();
    let key = track_key(0x9a, &[Segment::Name("samples")]);
    let track = defs
        .define_track(TrackSpec {
            key,
            clock,
            dimensions: shrink_wrap::prelude::RefVec::Slice { slice: &[] },
            items: ww_tape::Items::numbers::<i16>(None),
            label: "grid starts",
            repeat: ww_tape::TrackRepeat::Stream,
            kind: TrackKind::Series(GridDef::of::<i16>(1, period())),
            relations: shrink_wrap::prelude::RefVec::Slice { slice: &[] },
            owner: [0; 32],
            defined_twice: false,
        })
        .unwrap();
    (defs, track, key.id())
}

/// One chunk to write: its first sample, its count and its grid.
#[derive(Copy, Clone, Debug)]
struct Planned {
    first: u64,
    count: u32,
    grid: SeriesGrid,
}

impl Planned {
    fn head(&self, track: TrackNum, id: TrackId) -> ChunkHead {
        let (first_tick, last_tick) = self
            .grid
            .chunk_ticks(period(), ItemNum::new(self.first), self.count)
            .unwrap();
        ChunkHead {
            kind: BlockKind::Series,
            track_id: id,
            track_number: track,
            first_item: ItemNum::new(self.first),
            count: self.count,
            first_tick,
            last_tick,
            latest_end: last_tick,
            time_mode: 0,
            end_mode: EndMode::Absent,
            grid: Some(self.grid),
            tile: TileCoords::NONE,
            level: 0,
        }
    }
}

/// Stretches `stretch` samples long, `n` of them, in chunks of at most `chunk`: the scale changes at each stretch,
/// and a new grid starts after a gap at each stretch `gaps` names. Whether each chunk starts a grid or scale too.
fn plan(n: u64, stretch: u64, chunk: u64, gaps: &[u64]) -> Vec<(Planned, bool)> {
    let mut out = vec![];
    let mut grid = SeriesGrid::start(0, 1_000, Magnitude::ONE);
    for s in 0..n {
        let from = s * stretch;
        let scale = if s % 2 == 0 {
            Magnitude::ONE
        } else {
            Magnitude::pow10(-3)
        };
        if gaps.contains(&s) {
            let last = grid.tick(period(), ItemNum::new(from - 1)).unwrap();
            grid = SeriesGrid::start(from, last + GAP, scale);
        } else {
            grid = grid.rescaled(scale);
        }
        let mut first = from;
        while first < from + stretch {
            let count = chunk.min(from + stretch - first) as u32;
            out.push((Planned { first, count, grid }, first == from));
            first += u64::from(count);
        }
    }
    out
}

/// A samples column of `count` zeros in DELTA, made once per count.
fn column(cache: &mut BTreeMap<u32, Vec<u8>>, count: u32) -> (ColumnSpec, &[u8]) {
    let bytes = cache.entry(count).or_insert_with(|| {
        let shape = SampleShape::new(i16::ELEMENT, 1);
        let zeros = vec![0i16; count as usize];
        let mut out = vec![0u8; max_encoded_len(Codec::DELTA, shape, count as usize).unwrap()];
        let n = encode(Codec::DELTA, shape, &zeros, &mut out).unwrap();
        out.truncate(n);
        out
    });
    let spec = ColumnSpec {
        kind: ColumnKind::Samples,
        codec: Codec::DELTA,
        param: 0,
        len: bytes.len(),
    };
    (spec, bytes)
}

/// Bytes whose reads are noted: offset and length.
struct Noted<'a> {
    bytes: &'a [u8],
    reads: RefCell<Vec<(u64, usize)>>,
}

impl ByteSource for Noted<'_> {
    type Error = ();

    fn size(&self) -> Result<u64, ()> {
        Ok(self.bytes.len() as u64)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), ()> {
        self.reads.borrow_mut().push((offset, buf.len()));
        self.bytes.read_at(offset, buf).map_err(drop)
    }
}

/// The level 0 series entries of track `id` in piece 0 of `r`, in sample order.
fn entries<S: ByteSource>(r: &TapeReader<S>, id: TrackId) -> Vec<IndexEntry> {
    let mut e: Vec<_> = r
        .track_entries(0, id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == BlockKind::Series && e.level == 0)
        .collect();
    e.sort_by_key(|e| e.first_item);
    e
}

/// The timeline of track `id` in piece 0 of `r` from every chunk head.
fn from_every_head<S: ByteSource>(r: &TapeReader<S>, id: TrackId) -> SeriesTimeline
where
    S::Error: core::fmt::Debug,
{
    let mut buf = vec![];
    let heads: Vec<ChunkHead> = entries(r, id)
        .iter()
        .map(|e| r.read_chunk_head(0, e, &mut buf).unwrap())
        .collect();
    let (t, errors) = SeriesTimeline::from_chunks(period(), &heads);
    assert!(errors.is_empty(), "{errors:?}");
    t
}

/// Ten hours of 48 kHz in 1 MiB chunks, the scale changing every hour and a new grid after a gap at hours 3 and 7,
/// two chunks lost, in a chain cut every 1000 chunks: the timeline of each piece from its index reads the heads of
/// the chunks where a grid or scale starts, where the piece starts and after the lost ones, and no other; it is the
/// one from every head.
#[test]
fn a_ten_hour_series_reads_a_head_per_grid_not_per_chunk() {
    let (defs, track, id) = defs();
    let hour = 3600 * RATE;
    let planned = plan(10, hour, CHUNK, &[3, 7]);
    let lost = [500, 2000];
    assert!(planned.len() > 3000, "{} chunks", planned.len());
    for k in lost {
        assert!(!planned[k].1 && !planned[k + 1].1);
    }
    let mem = MemPieces::new();
    let options = ChainOptions {
        cut: CutPolicy {
            chunks: exact_units::Count::new(1000),
            ..CutPolicy::default()
        },
        ..ChainOptions::default()
    };
    let mut chain =
        PieceChain::create_on(mem.clone(), OwnerId([0x9a; 32]), 0, &[], options).unwrap();
    let mut cache = BTreeMap::new();
    for (k, (p, _)) in planned.iter().enumerate() {
        if lost.contains(&k) {
            continue;
        }
        let col = column(&mut cache, p.count);
        chain
            .write_chunk(&defs, &p.head(track, id), &[col])
            .unwrap();
    }
    let pieces = chain.close().unwrap();
    assert!(pieces.len() >= 3, "{} pieces", pieces.len());
    // where a grid or scale starts, and the chunk after each lost one
    let starts: Vec<u64> = planned
        .iter()
        .filter(|(_, s)| *s)
        .map(|(p, _)| p.first)
        .collect();
    let after_lost: Vec<u64> = lost.iter().map(|k| planned[k + 1].0.first).collect();
    let (mut heads, mut chunks) = (0, 0);
    for key in mem.keys().unwrap() {
        let bytes = mem.closed(&key).unwrap().unwrap().to_vec().unwrap();
        let src = Noted {
            bytes: &bytes,
            reads: RefCell::new(vec![]),
        };
        let r = TapeReader::open(&src).unwrap();
        assert!(r.verify(0).unwrap().holds);
        let es = entries(&r, id);
        let first = es[0].first_item.get();
        let mut want: Vec<u64> = starts
            .iter()
            .chain(&after_lost)
            .copied()
            .filter(|s| es.iter().any(|e| e.first_item.get() == *s))
            .chain([first])
            .collect();
        want.sort_unstable();
        want.dedup();
        // the writer marked exactly the starts of a grid or scale
        let marked: Vec<u64> = es
            .iter()
            .filter(|e| e.grid_start)
            .map(|e| e.first_item.get())
            .collect();
        let starts_here: Vec<u64> = starts
            .iter()
            .copied()
            .filter(|s| es.iter().any(|e| e.first_item.get() == *s))
            .collect();
        assert!(
            marked
                .iter()
                .all(|m| starts_here.contains(m) || *m == first),
            "{marked:?}"
        );
        assert!(starts_here.iter().all(|s| marked.contains(s)));
        let start = r.pieces()[0].start;
        src.reads.borrow_mut().clear();
        let (t, errors) = r.series_timeline(0, id, period()).unwrap();
        assert!(errors.is_empty(), "{errors:?}");
        let at: BTreeMap<u64, u64> = es
            .iter()
            .map(|e| (start + e.offset, e.first_item.get()))
            .collect();
        let mut read: Vec<u64> = src
            .reads
            .borrow()
            .iter()
            .filter(|(o, len)| *len == CHUNK_HEAD_PREFIX && at.contains_key(o))
            .map(|(o, _)| at[o])
            .collect();
        read.sort_unstable();
        assert_eq!(read, want);
        assert_eq!(t, from_every_head(&r, id));
        heads += read.len();
        chunks += es.len();
    }
    // 10 grids or scales, a start per piece, a chunk after each lost one, some of them the same chunk
    assert!(
        (10..=10 + pieces.len() + lost.len()).contains(&heads),
        "{heads} heads of {chunks} chunks"
    );
    assert_eq!(chunks, planned.len() - lost.len());
}

/// Write `planned` but the chunks `skip` names into one piece; its bytes.
fn one_piece(planned: &[(Planned, bool)], skip: &[usize]) -> Vec<u8> {
    let (defs, track, id) = defs();
    let mut w = crate::resume::begin(0);
    let mut cache = BTreeMap::new();
    for (k, (p, _)) in planned.iter().enumerate() {
        if !skip.contains(&k) {
            let col = column(&mut cache, p.count);
            w.write_chunk(&defs, &p.head(track, id), &[col]).unwrap();
        }
    }
    let tape = w.sink().tape();
    w.close().unwrap();
    tape.to_vec().unwrap()
}

/// A relay passes on another tape's chunks, one marked chunk lost on the way: it marks the chunk after it, which
/// starts a scale here. The timeline from the index is the one from every head, and the piece holds.
#[test]
fn a_relay_marks_the_chunks_after_a_lost_one() {
    let (mut defs, _, id) = defs();
    let planned = plan(4, 40, 8, &[2]);
    let src = one_piece(&planned, &[]);
    let mut blocks = vec![];
    let mut rd = FileBlocks::new(&src[..]);
    while let Some(b) = rd.next_block() {
        let b = b.unwrap();
        if b.chunk().is_some() {
            blocks.push(b.bytes().to_vec());
        }
    }
    assert_eq!(blocks.len(), planned.len());
    // the second stretch's first chunk, marked; the one after it isn't
    let lost = 5;
    assert!(planned[lost].1);
    assert!(Block::read(&blocks[lost]).unwrap().is_grid_start());
    assert!(!Block::read(&blocks[lost + 1]).unwrap().is_grid_start());
    let mut w = crate::resume::begin(0);
    for (k, b) in blocks.iter().enumerate() {
        if k != lost {
            w.relay_block(&mut defs, b).unwrap();
        }
    }
    let tape = w.sink().tape();
    w.close().unwrap();
    let bytes = tape.to_vec().unwrap();
    let r = TapeReader::open(&bytes[..]).unwrap();
    assert!(r.verify(0).unwrap().holds);
    let es = entries(&r, id);
    let after = es
        .iter()
        .find(|e| e.first_item.get() == planned[lost + 1].0.first)
        .unwrap();
    assert!(after.grid_start, "restamped");
    let (t, errors) = r.series_timeline(0, id, period()).unwrap();
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(t, from_every_head(&r, id));
}

/// Set or clear the grid start mark of the chunk at `at` in `bytes`, in its header and in every index entry naming
/// it, each index block's CRCs made good: what a hostile writer does.
fn forge_mark(bytes: &mut [u8], at: usize, on: bool) {
    let len = ww_tape::block_len(&bytes[at..]).unwrap();
    ww_tape::mark_grid_start(&mut bytes[at..at + len], on).unwrap();
    let mut index = vec![];
    let mut rd = FileBlocks::new(&bytes[..]);
    let mut pos = 0;
    while let Some(b) = rd.next_block() {
        let b = b.unwrap();
        if b.kind == BlockKind::Index {
            index.push(pos);
        }
        pos += b.len();
    }
    for i in index {
        let n = ww_tape::block_len(&bytes[i..]).unwrap();
        let b = &mut bytes[i..i + n];
        let entries_at = u32::from_le_bytes(b[32..36].try_into().unwrap()) as usize;
        let tracks = u32::from_le_bytes(b[36..40].try_into().unwrap()) as usize;
        let entry_len = u16::from_le_bytes(b[58..60].try_into().unwrap()) as usize;
        let count = u32::from_le_bytes(b[12..16].try_into().unwrap()) as usize;
        for k in 0..count {
            let e = entries_at + k * entry_len;
            if u64::from_le_bytes(b[e..e + 8].try_into().unwrap()) == at as u64 {
                b[e + 15] = if on { b[e + 15] | 8 } else { b[e + 15] & !8 };
            }
        }
        for t in 0..tracks {
            let d = 60 + t * 24;
            let first = u32::from_le_bytes(b[d + 8..d + 12].try_into().unwrap()) as usize;
            let n = u32::from_le_bytes(b[d + 12..d + 16].try_into().unwrap()) as usize;
            let from = entries_at + first * entry_len;
            let crc = crate::range_hash::crc32c(&[&b[from..from + n * entry_len]]);
            b[d + 16..d + 20].copy_from_slice(&crc.to_le_bytes());
        }
        let mut head = b[..28].to_vec();
        head.extend_from_slice(&b[32..entries_at]);
        b[28..32].copy_from_slice(&crate::range_hash::crc32c(&[&head]).to_le_bytes());
        let crc = crate::range_hash::crc32c(&[&b[..n - 4]]);
        b[n - 4..].copy_from_slice(&crc.to_le_bytes());
    }
}

/// Where the chunk holding sample `first` starts in `bytes`.
fn chunk_at(bytes: &[u8], first: u64) -> usize {
    let mut rd = FileBlocks::new(bytes);
    let mut pos = 0;
    while let Some(b) = rd.next_block() {
        let b = b.unwrap();
        if b.chunk()
            .is_some_and(|c| c.head().first_item.get() == first)
        {
            return pos;
        }
        pos += b.len();
    }
    panic!("no chunk at sample {first}")
}

/// A hostile file: a grid start left unmarked shows in its ticks and the timeline reads right; a scale start left
/// unmarked, or a mark where nothing starts, reads with no panic and verify names the chunk.
#[test]
fn a_missing_or_extra_mark_is_caught() {
    let (_, _, id) = defs();
    let planned = plan(3, 40, 8, &[2]);
    let good = one_piece(&planned, &[]);
    let want = from_every_head(&TapeReader::open(&good[..]).unwrap(), id);
    let gap = planned.iter().position(|(p, _)| p.first == 80).unwrap();
    let scale = planned.iter().position(|(p, _)| p.first == 40).unwrap();
    for (k, on, timeline_right) in [(gap, false, true), (scale, false, false), (2, true, true)] {
        let mut bad = good.clone();
        let at = chunk_at(&bad, planned[k].0.first);
        forge_mark(&mut bad, at, on);
        let r = TapeReader::open(&bad[..]).unwrap();
        assert_eq!(entries(&r, id)[k].grid_start, on);
        let (t, errors) = r.series_timeline(0, id, period()).unwrap();
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(t == want, timeline_right, "chunk {k}");
        match r.verify(0) {
            Err(ReadError::GridStart { offset, track_id }) => {
                assert_eq!((offset, track_id), (at as u64, id), "chunk {k}");
            }
            other => panic!("chunk {k}: {other:?}"),
        }
    }
}

/// Each tile of a grid track has its own grids: the first chunk of each tile is marked, the next chunks on the same
/// grid aren't.
#[test]
fn each_tile_starts_its_own_grid() {
    let (bytes, id, tiles) = crate::grids::tiles_twice();
    let r = TapeReader::open(&bytes[..]).unwrap();
    let mut seen = BTreeMap::new();
    for e in r.track_entries(0, id).unwrap() {
        if e.kind != BlockKind::Series || e.level != 0 {
            continue;
        }
        let first = !seen.contains_key(&e.tile);
        *seen.entry(e.tile).or_insert(0) += 1;
        assert_eq!(e.grid_start, first, "{e:?}");
    }
    assert_eq!(seen.len(), tiles);
    assert!(seen.values().all(|n| *n >= 2));
    assert!(r.verify(0).unwrap().holds);
}

/// The writer refuses a caller's chunk marked otherwise than due: unmarked where a grid starts, marked where it
/// doesn't.
#[test]
fn a_callers_chunk_carries_the_mark_exactly_where_due() {
    let (defs, track, id) = defs();
    let planned = plan(2, 16, 8, &[]);
    let mut cache = BTreeMap::new();
    let block =
        |w: &TapeWriter<MemSink>, p: &Planned, on: bool, cache: &mut BTreeMap<u32, Vec<u8>>| {
            let head = p.head(track, id);
            let col = column(cache, p.count);
            let mut b = vec![0u8; ww_tape::chunk::chunk_len(&head, &[col.0], 0)];
            let n = ww_tape::chunk::write_chunk(&mut b, w.next_id(), &head, &[col], 0).unwrap();
            b.truncate(n);
            ww_tape::mark_grid_start(&mut b, on).unwrap();
            b
        };
    let mut w = crate::resume::begin(0);
    let flags = |r| {
        matches!(
            r,
            Err(ww_tape::WriteError::Input(
                ww_tape::InputError::Flags { .. }
            ))
        )
    };
    for (p, due) in planned.iter().map(|(p, s)| (p, *s)) {
        let wrong = block(&w, p, !due, &mut cache);
        assert!(flags(w.write_block(&defs, &wrong)), "{p:?}");
        let right = block(&w, p, due, &mut cache);
        w.write_block(&defs, &right).unwrap();
    }
}

/// A producer resends the chunk it sent last, one that starts a scale: the relay takes it in that chunk's place, its
/// mark checked against the chunk before it, as write_block does.
#[test]
fn a_relay_takes_a_resent_marked_chunk() {
    let (mut defs, _, _) = defs();
    let planned = plan(2, 16, 8, &[]);
    let src = one_piece(&planned, &[]);
    let mut blocks = vec![];
    let mut rd = FileBlocks::new(&src[..]);
    while let Some(b) = rd.next_block() {
        let b = b.unwrap();
        if b.chunk().is_some() {
            blocks.push(b.bytes().to_vec());
        }
    }
    let k = planned.iter().position(|(p, s)| *s && p.first > 0).unwrap();
    let mut w = crate::resume::begin(0);
    for b in &blocks[..=k] {
        w.relay_block(&mut defs, b).unwrap();
    }
    w.relay_block(&mut defs, &blocks[k]).unwrap();
    let mut w = crate::resume::begin(0);
    for b in &blocks[..=k] {
        w.write_block(&defs, b).unwrap();
    }
    w.write_block(&defs, &blocks[k]).unwrap();
}
