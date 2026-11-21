//! The heap a `TapeWriter` holds while it writes one piece, counted by a global allocator: the writer on a sink that
//! keeps no bytes, its blocks and columns made by the caller in buffers the bench allocates before it starts counting.
//! Prints a Markdown table of the heap of a new writer, its peak while it writes and with the close, and the heap per block
//! of the piece, for records blocks on 1 to `--tracks` tracks and for series chunks the writer encodes from columns.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::print_stdout,
    reason = "a bench reads no file of others: it computes sizes freely, fails by panicking and prints its table"
)]

use clap::Parser;
use shrink_wrap::prelude::*;
use std::alloc::{GlobalAlloc, Layout, System};
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use ww_tape::chunk::ChunkHead;
use ww_tape::{
    ALIGNMENT, BlockKind, BlockWriter, ClockDef, ClockOrigin, ClockTime, Codec, ColumnKind,
    ColumnSpec, Definitions, Describe, EndMode, EventsDef, FullVersion, Magnitude, Ratio64,
    Segment, SeriesDef, SeriesGrid, SyncPolicy, TapeSink, TapeWriter, TrackDef, TrackKind, Version,
    clock_key, track_key,
};

/// The system allocator, counting the bytes live and their peak.
struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every call goes to the system allocator with the same layout; the counters only observe.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract for `alloc` holds for `System` too.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from `alloc` with this layout.
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// Starts counting from the bytes live now: returns them, and sets the peak to them.
fn baseline() -> usize {
    let live = LIVE.load(Ordering::Relaxed);
    PEAK.store(live, Ordering::Relaxed);
    live
}

#[derive(Parser)]
#[command(about = "ww_tape: the heap a writer holds while it writes one piece")]
struct Args {
    /// Size of the piece, MiB.
    #[arg(long, default_value_t = 256)]
    piece_mib: u64,
    /// Size of each records block, KiB.
    #[arg(long, default_value_t = 4)]
    block_kib: usize,
    /// Size of the samples of each series chunk, KiB.
    #[arg(long, default_value_t = 1024)]
    chunk_kib: usize,
    /// Most tracks of records blocks; the bench runs 1, 10, 100... up to it.
    #[arg(long, default_value_t = 10_000)]
    tracks: u32,
}

/// A sink that keeps no bytes: the heap counted is the writer's own.
struct Discard {
    bytes: u64,
}

impl TapeSink for Discard {
    type Error = io::Error;

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.bytes += bytes.len() as u64;
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn close(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn size(&self) -> io::Result<u64> {
        Ok(self.bytes)
    }

    fn read_at(&self, _: u64, _: &mut [u8]) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "a discarding sink keeps no bytes",
        ))
    }

    fn truncate(&mut self, _: u64) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "a discarding sink keeps no bytes",
        ))
    }

    fn rebase(&mut self, base: u64) -> io::Result<()> {
        if base == 0 {
            Ok(())
        } else {
            Err(io::ErrorKind::Unsupported.into())
        }
    }
}

const CLOCK: u32 = 1;

fn clock() -> Describe<'static> {
    Describe::Clock(ClockDef {
        number: UNib32(CLOCK),
        key: clock_key(0xd3, &[], 1),
        rate: Magnitude::pow10(9).into(),
        resolution: Magnitude::pow10(-9).into(),
        origin: ClockOrigin::Boot,
        label: "bench",
    })
}

fn track(number: u32, kind: TrackKind<'static>) -> Describe<'static> {
    Describe::Track(TrackDef {
        number: UNib32(number),
        key: track_key(0xd3, &[Segment::Index(number)]),
        clock: UNib32(CLOCK),
        dimensions: RefVec::Slice { slice: &[] },
        api: FullVersion::new("bench_api", Version::new(0, 1, 0)),
        bundle: RefVec::Slice { slice: &[] },
        path: RefVec::Slice {
            slice: &[UNib32(1)],
        },
        keys: RefVec::Slice { slice: &[] },
        item: None,
        label: "bench",
        repeat: ww_tape::TrackRepeat::Stream,
        kind,
        relations: RefVec::Slice { slice: &[] },
        owner: [0; 32],
    })
}

fn any(e: impl core::fmt::Debug) -> anyhow::Error {
    anyhow::anyhow!("{e:?}")
}

/// One row: the heap of a new writer, its peak before the close and in all, the blocks the piece holds.
struct Row {
    new: usize,
    open: usize,
    peak: usize,
    blocks: u64,
}

/// Records blocks of `block` bytes on `tracks` tracks in turn, until the piece reaches `piece` bytes, then the close.
fn records(tracks: u32, piece: u64, block: usize) -> anyhow::Result<Row> {
    let mut all = vec![clock()];
    all.extend((1..=tracks).map(|n| track(n, TrackKind::Events(EventsDef::INSTANTS))));
    let mut defs = Definitions::new();
    defs.define_all(&all).map_err(any)?;
    let item = vec![0x5a; block.saturating_sub(64)];
    let mut buf = vec![0u8; block];
    let start = baseline();
    let mut w = TapeWriter::create_on(Discard { bytes: 0 }, SyncPolicy::None);
    let new = LIVE.load(Ordering::Relaxed) - start;
    let mut n = 0u32;
    while w.sink().bytes < piece {
        let t = 1 + n % tracks;
        let mut b =
            BlockWriter::new(&mut buf, BlockKind::Records, w.next_id(), &[CLOCK]).map_err(any)?;
        b.push(t, ClockTime::new(CLOCK, i64::from(n)), &[&item])
            .map_err(any)?;
        let len = b.finish();
        w.write_block(&defs, buf.get(..len).ok_or_else(|| any("block"))?)
            .map_err(any)?;
        n = n.wrapping_add(1);
    }
    let open = PEAK.load(Ordering::Relaxed) - start;
    w.close().map_err(any)?;
    Ok(Row {
        new,
        open,
        peak: PEAK.load(Ordering::Relaxed) - start,
        blocks: u64::from(n),
    })
}

/// Series chunks of `chunk` bytes of i16 samples on one track, encoded by the writer from a column, until the piece
/// reaches `piece` bytes, then the close.
fn series(piece: u64, chunk: usize) -> anyhow::Result<Row> {
    let period = Ratio64::new(62_500, 3).ok_or_else(|| any("48 kHz on a 1 ns clock"))?;
    let mut defs = Definitions::new();
    defs.define_all(&[
        clock(),
        track(1, TrackKind::Series(SeriesDef::of::<i16>(None, 1, period))),
    ])
    .map_err(any)?;
    let samples: Vec<u8> = (0..chunk).map(|i| (i * 31 % 251) as u8).collect();
    let spec = ColumnSpec {
        kind: ColumnKind::Samples,
        codec: Codec::RAW,
        param: 0,
        len: samples.len(),
    };
    let columns = [(spec, samples.as_slice())];
    let count = u32::try_from(chunk / 2)?;
    let grid = SeriesGrid::start(0, 0, Magnitude::ONE);
    let start = baseline();
    let mut w = TapeWriter::create_on(Discard { bytes: 0 }, SyncPolicy::None);
    w.set_alignment(ALIGNMENT).map_err(any)?;
    let new = LIVE.load(Ordering::Relaxed) - start;
    let mut k = 0u64;
    while w.sink().bytes < piece {
        let first = k * u64::from(count);
        let (first_tick, last_tick) = grid.chunk_ticks(period, first, count).map_err(any)?;
        let head = ChunkHead {
            kind: BlockKind::Series,
            track_id: track_key(0xd3, &[Segment::Index(1)]),
            track_number: 1,
            first_item: first,
            count,
            first_tick,
            last_tick,
            latest_end: last_tick,
            time_mode: 0,
            end_mode: EndMode::Absent,
            grid: Some(grid),
        };
        w.write_chunk(&defs, &head, &columns).map_err(any)?;
        k += 1;
    }
    let open = PEAK.load(Ordering::Relaxed) - start;
    w.close().map_err(any)?;
    Ok(Row {
        new,
        open,
        peak: PEAK.load(Ordering::Relaxed) - start,
        blocks: k,
    })
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let piece = args.piece_mib << 20;
    println!(
        "The heap of a writer over one piece of {} MiB on a sink that keeps no bytes.\n",
        args.piece_mib
    );
    println!(
        "| Blocks | Tracks | Blocks in the piece | New writer (bytes) | Peak before the close (bytes) | Peak with the close (bytes) | Before the close, per block (bytes) |"
    );
    println!("|---|---:|---:|---:|---:|---:|---:|");
    let print = |what: &str, tracks: u32, r: &Row| {
        println!(
            "| {what} | {tracks} | {} | {} | {} | {} | {:.1} |",
            r.blocks,
            r.new,
            r.open,
            r.peak,
            r.open as f64 / r.blocks as f64
        );
    };
    let mut tracks = 1;
    while tracks <= args.tracks {
        let r = records(tracks, piece, args.block_kib * 1024)?;
        print(&format!("records, {} KiB", args.block_kib), tracks, &r);
        tracks *= 10;
    }
    let r = series(piece, args.chunk_kib * 1024)?;
    print(&format!("series chunks, {} KiB", args.chunk_kib), 1, &r);
    Ok(())
}
