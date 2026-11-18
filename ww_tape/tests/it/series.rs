//! A series track written and read back with ww_tape alone: its definition, its chunks' grids (a start, an
//! autorange prefix change, a gap) and its samples, from `Series<Fixed<..>>` batches to chunk columns without a float.

use std::path::PathBuf;

use exact_units::prefix::{Micro, Milli};
use exact_units::units::{Ampere, Second, Tick};
use exact_units::{Fixed, MagnitudeOf, units::Per};
use shrink_wrap::prelude::*;
use shrink_wrap::series::Series;
use ww_tape::chunk::ChunkHead;
use ww_tape::samples::{Samples, encode, max_encoded_len};
use ww_tape::{
    BlockKind, ClockDef, ClockOrigin, Codec, ColumnKind, ColumnSpec, Describe, DescribeOwned,
    EndMode, FileBlocks, FullVersion, Layout, Magnitude, Producer, Ratio64, Recovery, SampleShape,
    Segment, SeriesDef, SeriesGrid, SeriesTimeline, SeriesValue, SyncPolicy, TapeReader,
    TapeWriter, TrackDef, TrackKind, TypeRef, Version, clock_key, track_key,
};

const CLOCK: u32 = 1;
const TRACK: u32 = 1;
const CHANNELS: u32 = 2;
/// Samples per chunk at most.
const CHUNK_MAX: u64 = 128;
/// Samples per received batch.
const BATCH: usize = 32;
/// First tick of the series.
const START: i64 = 1_000;
/// Ticks between the last sample before the gap and the first after it.
const GAP: i64 = 5_000_000;

type MicroAmps = Fixed<Ampere, Micro, i32>;
type MilliAmps = Fixed<Ampere, Milli, i32>;

/// 48 kHz on a 1 ns clock.
fn period() -> Ratio64 {
    Ratio64::new(62_500, 3).unwrap()
}

/// Stored number of channel `c` of sample `i`.
fn value(i: u64, c: u32) -> i32 {
    ((i as i32 * 37) % 1000) - 500 + c as i32 * 10_000
}

/// A stretch of the recording: samples `from..to` as the device sends them.
#[derive(Copy, Clone)]
enum Unit {
    Micro,
    Milli,
}

/// Samples 0..320 in µA, 320..480 in mA (the device autoranged), 480..640 in µA after a gap.
const STRETCHES: [(u64, u64, Unit, bool); 3] = [
    (0, 320, Unit::Micro, false),
    (320, 480, Unit::Milli, false),
    (480, 640, Unit::Micro, true),
];

/// One received batch of samples `from..to`, its bytes as they came from the wire.
fn batch(from: u64, to: u64, unit: Unit) -> Vec<u8> {
    let mut buf = vec![0u8; 4096];
    let len = match unit {
        Unit::Micro => {
            let items: Vec<[MicroAmps; 2]> = (from..to)
                .map(|i| [MicroAmps::new(value(i, 0)), MicroAmps::new(value(i, 1))])
                .collect();
            Series::new(&items).to_ww_bytes(&mut buf).unwrap().len()
        }
        Unit::Milli => {
            let items: Vec<[MilliAmps; 2]> = (from..to)
                .map(|i| [MilliAmps::new(value(i, 0)), MilliAmps::new(value(i, 1))])
                .collect();
            Series::new(&items).to_ww_bytes(&mut buf).unwrap().len()
        }
    };
    buf.truncate(len);
    buf
}

/// A received batch decoded and put into `column` as stored numbers, channels interleaved: no float on the way.
/// Its scale is the type's.
fn ingest<V>(bytes: &[u8], column: &mut Vec<i32>) -> Magnitude
where
    V: SeriesValue<Scalar = i32> + for<'i> DeserializeShrinkWrap<'i>,
    [V; 2]: for<'i> shrink_wrap::series::SeriesColumns<'i>,
{
    let s = Series::<[V; 2]>::from_ww_bytes(bytes).unwrap();
    for item in s.iter() {
        for v in item {
            let mut n = [0i32; 1];
            assert_eq!(v.write_parts(&mut n), 1);
            column.push(n[0]);
        }
    }
    V::SCALE
}

fn sample_type() -> TypeRef<'static> {
    TypeRef {
        bundle: RefVec::Slice { slice: &[] },
        krate: FullVersion::new("exact_units", Version::new(0, 1, 0)),
        path: "Fixed<Ampere, Micro, i32>",
    }
}

fn series_def() -> SeriesDef<'static> {
    SeriesDef::of::<MicroAmps>(Some(sample_type()), CHANNELS, period())
}

/// The definitions of the recording.
fn definitions() -> Vec<Describe<'static>> {
    vec![
        Describe::Producer(Producer { name: "series" }),
        Describe::Layout(Layout::ALIGNED),
        Describe::Clock(ClockDef {
            number: UNib32(CLOCK),
            key: clock_key(0x5e, &[], 1),
            rate: MagnitudeOf::<Per<Tick, Second>>::from(Magnitude::pow10(9)),
            resolution: Magnitude::pow10(-9).into(),
            origin: ClockOrigin::Boot,
            label: "dev",
        }),
        Describe::Track(TrackDef {
            number: UNib32(TRACK),
            key: track_key(0x5e, &[Segment::Name("phase")]),
            clock: UNib32(CLOCK),
            dimensions: RefVec::Slice { slice: &[] },
            api: FullVersion::new("motor_api", Version::new(0, 1, 0)),
            bundle: RefVec::Slice { slice: &[] },
            path: RefVec::Slice {
                slice: &[UNib32(3)],
            },
            keys: RefVec::Slice { slice: &[] },
            item: None,
            label: "phase.current",
            repeat: ww_tape::TrackRepeat::Stream,
            kind: TrackKind::Series(series_def()),
            parent: None,
            context: RefVec::Slice { slice: &[] },
            entity_key: None,
        }),
    ]
}

/// The definitions of [definitions] a [ww_tape::Definitions] holds, made once.
fn series_defs() -> &'static ww_tape::Definitions {
    static D: std::sync::OnceLock<ww_tape::Definitions> = std::sync::OnceLock::new();
    D.get_or_init(|| {
        let held: Vec<_> = definitions()
            .into_iter()
            .filter(|d| !matches!(d, Describe::Producer(_) | Describe::Layout(_)))
            .collect();
        let mut d = ww_tape::Definitions::new();
        d.define_all(&held).unwrap();
        d
    })
}

/// Write the open column as chunks of at most [CHUNK_MAX] samples on `grid`.
fn seal(w: &mut TapeWriter, grid: SeriesGrid, first: &mut u64, column: &mut Vec<i32>) {
    let shape = SampleShape::new(i32::ELEMENT, CHANNELS);
    let lanes = CHANNELS as usize;
    for part in column.chunks(CHUNK_MAX as usize * lanes) {
        let count = (part.len() / lanes) as u32;
        let (first_tick, last_tick) = grid.chunk_ticks(period(), *first, count).unwrap();
        let mut bytes = vec![0u8; max_encoded_len(Codec::DELTA, shape, count as usize).unwrap()];
        let n = encode(Codec::DELTA, shape, part, &mut bytes).unwrap();
        bytes.truncate(n);
        let head = ChunkHead {
            kind: BlockKind::Series,
            track_id: 7,
            track_number: TRACK,
            first_item: *first,
            count,
            first_tick,
            last_tick,
            latest_end: last_tick,
            time_mode: 0,
            end_mode: EndMode::Absent,
            grid: Some(grid),
        };
        let spec = ColumnSpec {
            kind: ColumnKind::Samples,
            codec: Codec::DELTA,
            param: 0,
            len: bytes.len(),
        };
        w.write_chunk(series_defs(), &head, &[(spec, &bytes)])
            .unwrap();
        *first += u64::from(count);
    }
    column.clear();
}

/// The recording: batches as they come, a grid at the start, the same grid at a new scale at the prefix change and
/// a new grid after the gap.
fn write(path: &std::path::Path) {
    let p = period();
    let mut w = TapeWriter::create(path, SyncPolicy::None).unwrap();
    w.set_alignment(ww_tape::ALIGNMENT).unwrap();
    let mut grid = series_def().first_grid(START);
    w.set_session(crate::id(0).session).unwrap();
    for d in definitions() {
        match d {
            Describe::Producer(p) => w.set_producer(p.name),
            Describe::Layout(l) => w.set_layout(l).unwrap(),
            _ => {}
        }
    }
    let mut next = 0u64;
    let mut column = Vec::new();
    for (from, to, unit, gap) in STRETCHES {
        for b in (from..to).step_by(BATCH) {
            let bytes = batch(b, (b + BATCH as u64).min(to), unit);
            let mut numbers = Vec::new();
            let scale = match unit {
                Unit::Micro => ingest::<MicroAmps>(&bytes, &mut numbers),
                Unit::Milli => ingest::<MilliAmps>(&bytes, &mut numbers),
            };
            let at = next + column.len() as u64 / u64::from(CHANNELS);
            let new = if gap && b == from {
                let last = grid.tick(p, at - 1).unwrap();
                Some(SeriesGrid::start(at, last + GAP, scale))
            } else if scale != grid.scale {
                Some(grid.rescaled(scale))
            } else {
                None
            };
            if let Some(g) = new {
                seal(&mut w, grid, &mut next, &mut column);
                grid = g;
            }
            column.extend(numbers);
            if column.len() as u64 >= CHUNK_MAX * u64::from(CHANNELS) {
                seal(&mut w, grid, &mut next, &mut column);
            }
        }
    }
    seal(&mut w, grid, &mut next, &mut column);
    w.finish().unwrap();
}

/// What a reader with ww_tape alone gets: the definition, the timeline of its chunk heads, every chunk's head and
/// numbers.
struct Read {
    shape: SampleShape,
    period: Ratio64,
    scale: Magnitude,
    timeline: SeriesTimeline,
    chunks: Vec<(ChunkHead, Vec<i32>)>,
}

fn read(bytes: &[u8]) -> Read {
    let r = TapeReader::open(bytes).unwrap();
    assert_eq!(r.pieces().len(), 1);
    let mut def = None;
    let mut chunks = vec![];
    let mut buf = vec![];
    for e in r.entries(0).unwrap() {
        let b = r.read_block(0, &e, &mut buf).unwrap();
        match b.kind {
            BlockKind::Records => {
                for rec in b.records() {
                    let rec = rec.unwrap();
                    if rec.track == 0 {
                        let d = DescribeOwned::from_ww_bytes_owned(rec.item).unwrap();
                        if let DescribeOwned::Track(t) = d
                            && let ww_tape::TrackKindOwned::Series(s) = t.kind
                        {
                            def = Some(s);
                        }
                    } else {
                        assert_ne!(rec.track, TRACK, "a series track has no records");
                    }
                }
            }
            BlockKind::Series => {
                let c = b.chunk().unwrap();
                let head = c.head();
                let def = def.as_ref().expect("describe before chunks");
                let col = c.column(0).unwrap();
                assert_eq!(col.kind, ColumnKind::Samples);
                let s = Samples::of(&col, def.shape, head.count as usize).unwrap();
                let mut out = vec![0i32; head.count as usize * s.lanes()];
                s.decode(0, &mut out).unwrap();
                chunks.push((head, out));
            }
            _ => {}
        }
    }
    let def = def.expect("a series track");
    let (timeline, errors) = SeriesTimeline::from_chunks(def.period, chunks.iter().map(|(h, _)| h));
    assert!(errors.is_empty(), "{errors:?}");
    Read {
        shape: def.shape,
        period: def.period,
        scale: def.scale,
        timeline,
        chunks,
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ww_tape_series_{name}_{}", std::process::id()));
    _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn series_reads_back_to_samples_ticks_and_scales() {
    let dir = scratch("read");
    let path = dir.join("s.wwt");
    write(&path);
    let bytes = std::fs::read(&path).unwrap();
    let got = read(&bytes);
    assert_eq!(got.shape, SampleShape::new(i32::ELEMENT, CHANNELS));
    assert_eq!(got.period, period());
    assert_eq!(got.scale, Magnitude::pow10(-6));

    let micro = Magnitude::pow10(-6);
    let milli = Magnitude::pow10(-3);
    let p = period();
    let t = &got.timeline;
    // the three stretches: start, prefix change on the same grid, gap
    let s = t.stretches();
    assert_eq!(s.len(), 3);
    let first = SeriesGrid::start(0, START, micro);
    assert_eq!((s[0].first_item, s[0].end_item, s[0].grid), (0, 320, first));
    assert_eq!(
        (s[1].first_item, s[1].end_item, s[1].grid),
        (320, 480, first.rescaled(milli))
    );
    let after_gap = first.tick(p, 479).unwrap() + GAP;
    assert_eq!(s[2].grid, SeriesGrid::start(480, after_gap, micro));
    assert!(!t.gap_before(1) && t.gap_before(2));

    let mut index = 0u64;
    for (head, numbers) in &got.chunks {
        assert_eq!(head.first_item, index, "chunks follow each other");
        // each chunk times and scales its samples alone
        let grid = SeriesGrid::of_chunk(head, p).unwrap();
        assert_eq!(Some(grid.scale), t.scale(index));
        for (k, sample) in numbers.chunks(CHANNELS as usize).enumerate() {
            let i = index + k as u64;
            for (c, n) in sample.iter().enumerate() {
                assert_eq!(*n, value(i, c as u32), "sample {i} channel {c}");
            }
            let (grid, scale) = match i {
                0..320 => (
                    START + p.apply_rounded(i as i64, ww_tape::Rounding::Floor).unwrap(),
                    micro,
                ),
                320..480 => (
                    START + p.apply_rounded(i as i64, ww_tape::Rounding::Floor).unwrap(),
                    milli,
                ),
                _ => (
                    after_gap
                        + p.apply_rounded(i as i64 - 480, ww_tape::Rounding::Floor)
                            .unwrap(),
                    micro,
                ),
            };
            assert_eq!(t.tick(i), Some(grid), "tick of sample {i}");
            assert_eq!(t.scale(i), Some(scale), "scale of sample {i}");
        }
        index += u64::from(head.count);
    }
    assert_eq!(index, 640);
    _ = std::fs::remove_dir_all(&dir);
}

/// Bytes that count what is read of them.
struct Counted<'a> {
    bytes: &'a [u8],
    read: std::cell::Cell<u64>,
    reads: std::cell::Cell<u64>,
}

impl ww_tape::ByteSource for Counted<'_> {
    type Error = ();

    fn size(&self) -> Result<u64, ()> {
        Ok(self.bytes.len() as u64)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), ()> {
        let at = offset as usize;
        buf.copy_from_slice(self.bytes.get(at..at + buf.len()).ok_or(())?);
        self.read.set(self.read.get() + buf.len() as u64);
        self.reads.set(self.reads.get() + 1);
        Ok(())
    }
}

/// The timeline from the chunk heads alone is the one from whole blocks: two reads per chunk, its header and its
/// head (up to the payload, past the alignment padding), none of its samples. A damaged payload doesn't change it; a
/// damaged head loses its chunk, and the samples around it keep their grid.
#[test]
fn series_timeline_reads_chunk_heads_alone() {
    let dir = scratch("heads");
    let path = dir.join("s.wwt");
    write(&path);
    let bytes = std::fs::read(&path).unwrap();
    let want = read(&bytes);
    let track_id = want.chunks[0].0.track_id;
    let src = Counted {
        bytes: &bytes,
        read: 0.into(),
        reads: 0.into(),
    };
    let r = TapeReader::open(&src).unwrap();
    let entries = r.track_entries(0, track_id).unwrap();
    let chunks: Vec<_> = entries
        .iter()
        .filter(|e| e.kind == BlockKind::Series)
        .collect();
    assert_eq!(chunks.len(), want.chunks.len());
    let (read0, reads0) = (src.read.get(), src.reads.get());
    let (timeline, errors) = r.series_timeline(0, track_id, period()).unwrap();
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(timeline, want.timeline);
    let head_bytes = src.read.get() - read0;
    let block_bytes: u64 = chunks.iter().map(|e| u64::from(e.len)).sum();
    // the track's index entries again, then two reads per chunk
    assert!(src.reads.get() - reads0 <= 2 * chunks.len() as u64 + 4);
    assert!(head_bytes < block_bytes);

    // a flipped bit in a payload: the head still reads; in a head: that chunk is lost, with its error
    let mut buf = Vec::new();
    let e = chunks[1];
    let payload_at = {
        let b = r.read_block(0, e, &mut buf).unwrap();
        b.chunk().unwrap().payload_offset() as u64
    };
    let mut bad = bytes.clone();
    bad[(e.offset + payload_at) as usize] ^= 1;
    let r = TapeReader::open(&bad[..]).unwrap();
    assert!(r.read_block(0, e, &mut buf).is_err());
    assert_eq!(r.read_chunk_head(0, e, &mut buf).unwrap(), want.chunks[1].0);
    let (timeline, errors) = r.series_timeline(0, track_id, period()).unwrap();
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(timeline, want.timeline);
    let mut bad = bytes.clone();
    bad[(e.offset + 60) as usize] ^= 1;
    let r = TapeReader::open(&bad[..]).unwrap();
    let (timeline, errors) = r.series_timeline(0, track_id, period()).unwrap();
    assert_eq!(errors.len(), 1, "{errors:?}");
    let lost = &want.chunks[1].0;
    assert_eq!(timeline.stretch_of(lost.first_item), None);
    for i in lost.first_item..lost.first_item + u64::from(lost.count) {
        assert_eq!(
            timeline.tick(i),
            want.timeline.tick(i),
            "tick of lost sample {i}"
        );
    }
    _ = std::fs::remove_dir_all(&dir);
}

/// The same recording gives the same bytes, and they are golden/v1-series.hex's (`WW_TAPE_GOLDEN=update` rewrites
/// it): one block per line, in hex.
#[test]
fn series_file_is_deterministic_and_golden() {
    let dir = scratch("golden");
    let (a, b) = (dir.join("a.wwt"), dir.join("b.wwt"));
    write(&a);
    write(&b);
    let bytes = std::fs::read(&a).unwrap();
    assert_eq!(
        bytes,
        std::fs::read(&b).unwrap(),
        "two writes of the same series"
    );
    let mut hex = String::new();
    let mut rd = FileBlocks::with_recovery(&bytes[..], Recovery::Prefix);
    while let Some(block) = rd.next_block() {
        for x in block.unwrap().bytes() {
            hex.push_str(&format!("{x:02x}"));
        }
        hex.push('\n');
    }
    let golden = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("golden/v1-series.hex");
    if std::env::var("WW_TAPE_GOLDEN").as_deref() == Ok("update") {
        std::fs::write(&golden, &hex).unwrap();
    }
    let want = std::fs::read_to_string(&golden)
        .expect("golden/v1-series.hex: run with WW_TAPE_GOLDEN=update");
    assert_eq!(
        hex, want,
        "the format changed: bytes differ from golden/v1-series.hex"
    );
    let want: Vec<u8> = want
        .lines()
        .flat_map(|l| {
            (0..l.len())
                .step_by(2)
                .map(move |i| u8::from_str_radix(&l[i..i + 2], 16).unwrap())
        })
        .collect();
    assert_eq!(read(&want).chunks.len(), read(&bytes).chunks.len());
    _ = std::fs::remove_dir_all(&dir);
}

/// A series track has no records and holds only series chunks whose ticks are on their grid.
#[test]
fn writer_refuses_what_a_series_track_does_not_hold() {
    let dir = scratch("refuse");
    let mut w = TapeWriter::create(&dir.join("r.wwt"), SyncPolicy::None).unwrap();
    w.set_session(crate::id(0).session).unwrap();
    let defs = series_defs();
    let grid = series_def().first_grid(START);
    let (first_tick, last_tick) = grid.chunk_ticks(period(), 0, 4).unwrap();
    let head = ChunkHead {
        kind: BlockKind::Series,
        track_id: 7,
        track_number: TRACK,
        first_item: 0,
        count: 4,
        first_tick,
        last_tick,
        latest_end: last_tick,
        time_mode: 0,
        end_mode: EndMode::Absent,
        grid: Some(grid),
    };
    let bytes = [0u8; 32];
    let spec = ColumnSpec {
        kind: ColumnKind::Samples,
        codec: Codec::RAW,
        param: 0,
        len: bytes.len(),
    };
    let off_grid = ChunkHead {
        last_tick: last_tick + 1,
        latest_end: last_tick + 1,
        ..head
    };
    assert!(matches!(
        w.write_chunk(defs, &off_grid, &[(spec, &bytes)]),
        Err(ww_tape::WriteError::Describe(
            ww_tape::DescribeError::Kind {
                track: TRACK,
                error: ww_tape::SeriesError::ChunkTicks { first_item: 0 }
            }
        ))
    ));
    // an events chunk on the series track
    let events = ChunkHead {
        kind: BlockKind::Events,
        grid: None,
        ..head
    };
    let starts = ColumnSpec {
        kind: ColumnKind::Starts,
        ..spec
    };
    assert!(matches!(
        w.write_chunk(defs, &events, &[(starts, &bytes)]),
        Err(ww_tape::WriteError::Describe(
            ww_tape::DescribeError::WrongKind { track: TRACK }
        ))
    ));
    // a put: the track is not a state track
    assert!(w.put(defs, TRACK, START, &[1, 2, 3]).is_err());
    w.write_chunk(defs, &head, &[(spec, &bytes)]).unwrap();
    // a record in a caller's block
    let mut buf = [0u8; 128];
    let mut b =
        ww_tape::BlockWriter::new(&mut buf, BlockKind::Records, w.next_id(), &[CLOCK]).unwrap();
    b.push(TRACK, ww_tape::ClockTime::new(CLOCK, START), &[b"x"])
        .unwrap();
    let len = b.finish();
    assert!(matches!(
        w.write_block(defs, &buf[..len]),
        Err(ww_tape::WriteError::Describe(
            ww_tape::DescribeError::WrongKind { track: TRACK }
        ))
    ));
    _ = std::fs::remove_dir_all(&dir);
}

/// A caller's block gets its CRCs checked once: a series chunk with one payload byte flipped is refused by
/// `write_block`, `adopt_block` and `defer_block`; the same chunk of another session intact is adopted, renumbered
/// into the writer's session.
#[test]
fn caller_chunks_are_checked_once() {
    let dir = scratch("caller");
    let mut w = TapeWriter::create(&dir.join("c.wwt"), SyncPolicy::None).unwrap();
    w.set_session(crate::id(0).session).unwrap();
    let defs = series_defs();
    let grid = series_def().first_grid(START);
    let (first_tick, last_tick) = grid.chunk_ticks(period(), 0, 4).unwrap();
    let head = ChunkHead {
        kind: BlockKind::Series,
        track_id: 7,
        track_number: TRACK,
        first_item: 0,
        count: 4,
        first_tick,
        last_tick,
        latest_end: last_tick,
        time_mode: 0,
        end_mode: EndMode::Absent,
        grid: Some(grid),
    };
    let samples = [3u8; 32];
    let spec = ColumnSpec {
        kind: ColumnKind::Samples,
        codec: Codec::RAW,
        param: 0,
        len: samples.len(),
    };
    let mut good = vec![0u8; ww_tape::chunk::chunk_len(&head, &[spec], 0)];
    let theirs = ww_tape::BlockId::new(ww_tape::SessionTag::writable(99), 41);
    let n = ww_tape::chunk::write_chunk(&mut good, theirs, &head, &[(spec, &samples)], 0).unwrap();
    good.truncate(n);
    let at = ww_tape::Block::read(&good)
        .unwrap()
        .chunk()
        .unwrap()
        .payload_offset();
    let mut bad = good.clone();
    bad[at] ^= 1;
    let crc = |r: Result<_, ww_tape::WriteError>| {
        matches!(
            r,
            Err(ww_tape::WriteError::Block(ww_tape::BlockError::BadCrc))
        )
    };
    assert!(crc(w.write_block(defs, &bad)));
    assert!(crc(w.adopt_block(defs, &bad)));
    assert!(crc(w.defer_block(defs, bad.clone()).map(|()| None)));
    let placed = w.adopt_block(defs, &good).unwrap().unwrap();
    w.finish().unwrap();
    let bytes = std::fs::read(dir.join("c.wwt")).unwrap();
    let r = TapeReader::open(&bytes[..]).unwrap();
    let mut buf = vec![];
    let entry = r
        .entries(0)
        .unwrap()
        .into_iter()
        .find(|e| e.offset == placed.offset)
        .unwrap();
    let b = r.read_block(0, &entry, &mut buf).unwrap();
    assert!(b.is_intact());
    assert_eq!(
        b.session,
        crate::id(0).session,
        "renumbered into the writer's session"
    );
    assert_eq!(b.chunk().unwrap().head(), head);
    _ = std::fs::remove_dir_all(&dir);
}

/// A chunk that can't be a block is refused before anything is allocated or written: a grid whose scale doesn't
/// encode in `SCALE_LEN_MAX` bytes, and a column longer than a block.
#[test]
fn writer_refuses_chunks_too_long_without_a_panic() {
    let dir = scratch("too_long");
    let mut w = TapeWriter::create(&dir.join("t.wwt"), SyncPolicy::None).unwrap();
    w.set_session(crate::id(0).session).unwrap();
    w.set_alignment(ww_tape::ALIGNMENT).unwrap();
    let defs = series_defs();
    // a product of large-prime ratios: more factors than the scale bytes hold
    let primes = [
        (4_294_967_291, 4_294_967_279),
        (4_294_967_231, 4_294_967_197),
        (4_294_967_189, 4_294_967_161),
        (4_294_967_143, 4_294_967_111),
        (4_294_967_087, 4_294_967_029),
        (4_294_966_997, 4_294_966_981),
        (4_294_966_943, 4_294_966_927),
        (4_294_966_909, 4_294_966_877),
    ];
    let scale = primes
        .iter()
        .fold(Magnitude::ONE, |m, (n, d)| m.mul(Magnitude::ratio(*n, *d)));
    let mut bytes = [0u8; 256];
    assert!(
        scale
            .to_ww_bytes(&mut bytes)
            .map_or(true, |b| b.len() > ww_tape::series::SCALE_LEN_MAX),
        "the scale is longer than a chunk head holds"
    );
    let grid = SeriesGrid::start(0, START, scale);
    let (first_tick, last_tick) = grid.chunk_ticks(period(), 0, 4).unwrap();
    let head = ChunkHead {
        kind: BlockKind::Series,
        track_id: 7,
        track_number: TRACK,
        first_item: 0,
        count: 4,
        first_tick,
        last_tick,
        latest_end: last_tick,
        time_mode: 0,
        end_mode: EndMode::Absent,
        grid: Some(grid),
    };
    let samples = [0u8; 32];
    let spec = ColumnSpec {
        kind: ColumnKind::Samples,
        codec: Codec::RAW,
        param: 0,
        len: samples.len(),
    };
    assert!(matches!(
        w.write_chunk(defs, &head, &[(spec, &samples)]),
        Err(ww_tape::WriteError::Describe(
            ww_tape::DescribeError::Kind {
                track: TRACK,
                error: ww_tape::SeriesError::ScaleTooLong { first_item: 0 }
            }
        ))
    ));
    // a column spec longer than a block, its bytes short: refused, not allocated
    let head = ChunkHead {
        grid: Some(series_def().first_grid(START)),
        ..head
    };
    let (first_tick, last_tick) = series_def()
        .first_grid(START)
        .chunk_ticks(period(), 0, 4)
        .unwrap();
    let head = ChunkHead {
        first_tick,
        last_tick,
        latest_end: last_tick,
        ..head
    };
    let huge = ColumnSpec {
        len: usize::MAX / 2,
        ..spec
    };
    assert!(matches!(
        w.write_chunk(defs, &head, &[(huge, &samples)]),
        Err(ww_tape::WriteError::Chunk(
            ww_tape::chunk::ChunkError::TooLong { .. }
        ))
    ));
    assert_eq!(w.bytes(), 0, "nothing written");
    w.write_chunk(defs, &head, &[(spec, &samples)]).unwrap();
    _ = std::fs::remove_dir_all(&dir);
}
