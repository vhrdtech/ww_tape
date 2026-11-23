//! A flagged series written by a direct writer: each chunk carries its flag runs, so a writer resumed after a crash
//! rebuilds the summaries from the kept chunks alone and ends with the bytes of the run without the crash.

use std::collections::BTreeSet;

use exact_units::Bytes;
use ww_tape::flag_runs::FlagRunsBuilder;
use ww_tape::index::{IndexEntry, IndexPolicy};
use ww_tape::{
    Block, BlockKind, ChunkHead, ClockNum, Codec, ColumnKind, ColumnSpec, Definitions, EndMode,
    FlagRuns, ItemNum, Layout, MemSink, Ratio64, SampleShape, SeriesGrid, SyncPolicy, TapeReader,
    TapeSink, TapeWriter, TrackId, TrackNum, WriterOptions,
};
use ww_tape_summary::{SeriesStart, SeriesSummaries};

use crate::mux_handles::clock;
use crate::mux_resume::{sample, sample_flags, series_spec};
use crate::resume::{cut_points, head};

/// Samples per chunk.
const CHUNK: u64 = 256;
/// Chunks of the run.
const CHUNKS: u64 = 48;
/// Bytes of a summary block at most: small, so blocks are written mid-piece.
const BLOCK_BYTES: u32 = 768;

fn options(aligned: bool) -> WriterOptions {
    WriterOptions {
        sync: SyncPolicy::None,
        index: IndexPolicy {
            interval: None,
            ..IndexPolicy::default()
        },
        layout: if aligned {
            Layout::ALIGNED
        } else {
            Layout::UNALIGNED
        },
        producer: "flags".to_owned(),
        ..WriterOptions::default()
    }
}

/// The definitions of the run: a clock and the flagged series, and the series' number and id.
fn defs() -> (Definitions, TrackNum, TrackId) {
    let mut d = Definitions::new();
    let c = d.define_clock(clock()).unwrap();
    let spec = series_spec(c, true);
    let id = spec.key.id();
    let n = d.define_track(spec).unwrap();
    (d, n, id)
}

fn period() -> Ratio64 {
    Ratio64::int(1000).unwrap()
}

/// The summaries of the series from sample 0 on.
fn summaries(track: TrackNum, id: TrackId) -> SeriesSummaries<i16> {
    let ww_tape::TrackKind::Series(s) = series_spec(ClockNum::new(1), true).kind else {
        unreachable!("a series")
    };
    let start = SeriesStart {
        track_id: id,
        track_number: track,
        shape: SampleShape::new(ww_tape::ElementDesc::of(ww_tape::Element::I16), 2),
        first_item: ItemNum::new(0),
        grid: SeriesGrid::start(0, 0, exact_units::Magnitude::ONE),
        period: period(),
    };
    let mut out = SeriesSummaries::new(start, s.summaries.unwrap()).unwrap();
    out.set_max_block_bytes(Bytes::new(BLOCK_BYTES));
    out
}

/// A writer of a new piece.
fn begin(aligned: bool) -> TapeWriter<MemSink> {
    let o = options(aligned);
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    w.set_layout(o.layout).unwrap();
    w.set_producer(&o.producer);
    w.set_index_policy(o.index).unwrap();
    w.begin_piece(&head(), &[]).unwrap();
    w
}

/// Write chunks `from..` of the run on `w`: each chunk's samples pushed into `s` a stretch of one flags word at a
/// time, the chunk written with its flag runs, then the summary blocks those samples finish deferred; the last
/// summaries at the end. Then close.
fn write_from(
    mut w: TapeWriter<MemSink>,
    defs: &Definitions,
    track: (TrackNum, TrackId),
    s: &mut SeriesSummaries<i16>,
    from: u64,
    kept: &mut BTreeSet<(u8, u64, u64)>,
) -> Vec<u8> {
    let shape = SampleShape::new(ww_tape::ElementDesc::of(ww_tape::Element::I16), 2);
    for c in from..CHUNKS {
        let first = c * CHUNK;
        let mut runs = FlagRunsBuilder::new();
        let mut i = first;
        while i < first + CHUNK {
            let flags = sample_flags(i);
            let end = (i..first + CHUNK)
                .find(|j| sample_flags(*j) != flags)
                .unwrap_or(first + CHUNK);
            let part: Vec<i16> = (i..end).flat_map(sample).collect();
            s.push(&part, flags).unwrap();
            runs.push((end - i) as u32, flags).unwrap();
            i = end;
        }
        let values: Vec<i16> = (first..first + CHUNK).flat_map(sample).collect();
        let mut bytes = vec![
            0u8;
            ww_tape::samples::max_encoded_len(Codec::RAW, shape, CHUNK as usize)
                .unwrap()
        ];
        let n = ww_tape::samples::encode(Codec::RAW, shape, &values, &mut bytes).unwrap();
        bytes.truncate(n);
        let grid = SeriesGrid::start(0, 0, exact_units::Magnitude::ONE);
        let (first_tick, last_tick) = grid
            .chunk_ticks(period(), ItemNum::new(first), CHUNK as u32)
            .unwrap();
        let head = ChunkHead {
            kind: BlockKind::Series,
            track_id: track.1,
            track_number: track.0,
            first_item: ItemNum::new(first),
            count: CHUNK as u32,
            first_tick,
            last_tick,
            latest_end: last_tick,
            time_mode: 0,
            end_mode: EndMode::Absent,
            grid: Some(grid),
            tile: ww_tape::TileCoords::NONE,
        };
        let mut flags = vec![];
        runs.write(&mut flags);
        let mut columns = vec![(
            ColumnSpec {
                kind: ColumnKind::Samples,
                codec: Codec::RAW,
                param: 0,
                len: bytes.len(),
            },
            &bytes[..],
        )];
        if !runs.is_empty() {
            columns.push((
                ColumnSpec {
                    kind: ColumnKind::Flags,
                    codec: Codec::FLAG_RUNS,
                    param: 0,
                    len: flags.len(),
                },
                &flags[..],
            ));
        }
        w.write_chunk(defs, &head, &columns).unwrap();
        defer(&mut w, defs, s.take_blocks(), kept);
    }
    s.finish().unwrap();
    defer(&mut w, defs, s.take_blocks(), kept);
    let tape = w.sink().tape();
    w.close().unwrap();
    tape.to_vec().unwrap()
}

/// What a summary block covers: level, first item and items.
fn covers(e: &IndexEntry) -> (u8, u64, u64) {
    (e.level, e.first_item.get(), e.count)
}

/// Defer `blocks`, but those of `kept`: placed before a crash.
fn defer<S: TapeSink>(
    w: &mut TapeWriter<S>,
    defs: &Definitions,
    blocks: Vec<Vec<u8>>,
    kept: &mut BTreeSet<(u8, u64, u64)>,
) where
    S::Error: core::fmt::Debug,
{
    for b in blocks {
        let block = Block::read(&b).unwrap();
        let c = covers(&IndexEntry::of_block(0, &block).unwrap());
        if !kept.remove(&c) {
            w.defer_block(defs, b).unwrap();
        }
    }
}

/// The whole run.
fn whole(aligned: bool) -> Vec<u8> {
    let (d, n, id) = defs();
    let mut s = summaries(n, id);
    write_from(begin(aligned), &d, (n, id), &mut s, 0, &mut BTreeSet::new())
}

/// Crash the run at every block boundary and bytes in between: a writer resumes the piece, rebuilds the summaries
/// from the chunks it kept (their samples and flag runs), writes the chunks after them and closes. The piece holds
/// the bytes of the run without the crash.
fn crash_anywhere(aligned: bool) {
    let want = whole(aligned);
    let (_, _, id) = defs();
    assert_eq!(
        crate::mux_resume::summary_flags(std::slice::from_ref(&want), id),
        7,
        "every word in the summaries"
    );
    let r = TapeReader::open(&want[..]).unwrap();
    let entries = r.entries(0).unwrap();
    let last_chunk = entries
        .iter()
        .filter(|e| e.kind == BlockKind::Series)
        .map(|e| e.offset)
        .max()
        .unwrap();
    assert!(
        entries
            .iter()
            .any(|e| e.kind == BlockKind::Summary && e.offset < last_chunk),
        "summary blocks mid-piece"
    );
    let mut crashes = 0;
    for at in cut_points(&want, 40) {
        if at == want.len() {
            continue;
        }
        let mut sink = MemSink::new();
        sink.write_all(&want[..at]).unwrap();
        let (mut w, resumed) = TapeWriter::resume_on(sink, options(aligned)).unwrap();
        if w.bytes() == 0 {
            w.begin_piece(&head(), &[]).unwrap();
        }
        let (d, n, id) = defs();
        let mut s = summaries(n, id);
        let mut kept: BTreeSet<_> = resumed
            .entries
            .iter()
            .filter(|e| e.kind == BlockKind::Summary)
            .map(covers)
            .collect();
        let mut next = 0;
        for e in resumed
            .entries
            .iter()
            .filter(|e| e.kind == BlockKind::Series)
        {
            let bytes = &want[e.offset as usize..(e.offset + u64::from(e.len)) as usize];
            let block = Block::read(bytes).unwrap();
            let chunk = block.chunk().unwrap();
            let count = chunk.head().count as usize;
            let column = chunk.find(ColumnKind::Samples, 0).unwrap();
            let samples = ww_tape::Samples::of(
                &column,
                SampleShape::new(ww_tape::ElementDesc::of(ww_tape::Element::I16), 2),
                count,
            )
            .unwrap();
            let mut values = vec![0i16; count * 2];
            samples.decode(0, &mut values).unwrap();
            let runs: FlagRuns<'_> = chunk.flag_runs().unwrap();
            s.push_runs(&values, &runs).unwrap();
            defer(&mut w, &d, s.take_blocks(), &mut kept);
            next = e.first_item.get() + e.count;
        }
        let got = write_from(w, &d, (n, id), &mut s, next / CHUNK, &mut kept);
        assert!(
            got == want,
            "aligned {aligned}: a crash at {at} of {}, kept {}: {} bytes",
            want.len(),
            resumed.kept_bytes,
            got.len()
        );
        crashes += 1;
    }
    assert!(crashes > 50, "{crashes} crashes");
}

#[test]
fn a_flagged_series_written_directly_resumes_to_the_bytes_of_the_whole_run() {
    crash_anywhere(false);
    crash_anywhere(true);
}

/// The chunks carry the flags: read back, their runs give each sample the word it was pushed with.
#[test]
fn chunks_carry_their_samples_flags() {
    let bytes = whole(false);
    let r = TapeReader::open(&bytes[..]).unwrap();
    let mut buf = vec![];
    let mut chunks = 0;
    let mut flagged = 0;
    for e in r
        .entries(0)
        .unwrap()
        .iter()
        .filter(|e| e.kind == BlockKind::Series)
    {
        let b = r.read_block(0, e, &mut buf).unwrap();
        let c = b.chunk().unwrap();
        let runs = c.flag_runs().unwrap();
        let first = c.head().first_item.get();
        let want: Vec<u32> = (first..first + CHUNK).map(sample_flags).collect();
        let got: Vec<u32> = (0..CHUNK as u32).map(|i| runs.flags_at(i)).collect();
        assert_eq!(got, want, "chunk from {first}");
        assert_eq!(
            runs.is_empty(),
            c.find(ColumnKind::Flags, 0).is_none(),
            "a column only for a chunk with a flag"
        );
        chunks += 1;
        flagged += usize::from(!runs.is_empty());
    }
    assert!(
        flagged > 0 && flagged < chunks,
        "{flagged} of {chunks} chunks flagged"
    );
}

/// A series handle completes its open chunk before samples that would start a flag run past `FLAG_RUNS_MAX`: the chunk
/// holds that many runs, the next chunk starts with those samples.
#[test]
fn a_handle_completes_a_chunk_full_of_flag_runs() {
    use ww_tape::flag_runs::FLAG_RUNS_MAX;
    use ww_tape_mux::{MuxOptions, SeriesOptions, TapeMux};
    let mem = ww_tape::MemPieces::new();
    let options = ww_tape::ChainOptions {
        writer: options(false),
        ..ww_tape::ChainOptions::default()
    };
    let owner = ww_tape::OwnerId([0x46; 32]);
    let mux = TapeMux::create(mem.clone(), owner, 0, &[], options, MuxOptions::default()).unwrap();
    let c = mux.define_clock(clock()).unwrap();
    let spec = series_spec(c, true);
    let id = spec.key.id();
    let track = mux.define_track(spec).unwrap();
    let mut series = mux
        .series::<i16>(
            track,
            SeriesOptions {
                latency: None,
                ..SeriesOptions::default()
            },
        )
        .unwrap();
    let frame: Vec<i16> = sample(0).to_vec();
    for _ in 0..FLAG_RUNS_MAX {
        series.push_flagged(&frame, 1).unwrap();
        series.push(&frame).unwrap();
    }
    series.push_flagged(&frame, 1).unwrap();
    series.finish().unwrap();
    mux.close().unwrap();
    let piece = crate::mux_multi::pieces(&mem).remove(0);
    let r = TapeReader::open(&piece[..]).unwrap();
    let mut buf = vec![];
    let got: Vec<(u64, u32, usize)> = r
        .entries(0)
        .unwrap()
        .iter()
        .filter(|e| e.kind == BlockKind::Series && e.track_id == id)
        .map(|e| {
            let b = r.read_block(0, e, &mut buf).unwrap();
            let c = b.chunk().unwrap();
            (
                c.head().first_item.get(),
                c.head().count,
                c.flag_runs().unwrap().len(),
            )
        })
        .collect();
    let full = 2 * FLAG_RUNS_MAX as u32;
    assert_eq!(got, [(0, full, FLAG_RUNS_MAX), (u64::from(full), 1, 1)]);
}
