//! Tracks added one by one: the application defines each new track in its `Definitions`, writes one records block
//! on it (the writer puts the definition before it), and closes the piece; a reader then opens it and takes its definitions.
//! Prints a Markdown table of the time to add the first 1 000, 5 000 and all tracks, the close, and the read, with
//! the load average before and after each table.
//!
//! Then the bytes a writer puts and copies (blocks marked FLAG_REPEAT) in one piece of `--piece-mib` with all the
//! tracks defined, records blocks of `--block-kib` on the tracks in use in turn: all of them, and 1 % of them, with
//! the default policies (definitions put again 32 MiB on, one copy 1 MiB after a first write).
//!
//! Last, the bytes a writer hashes and copies per block of data when nothing needs writing: a track whose
//! definition is 60 KiB long, written to again and again within its near.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::indexing_slicing,
    clippy::print_stdout,
    reason = "a bench reads no file of others: it computes sizes and rates freely, fails by panicking and prints its table"
)]

use clap::Parser;
use shrink_wrap::prelude::*;
use std::io;
use std::time::Instant;
use ww_tape::piece::{OwnerId, PieceHead, PieceId, PieceIdent};
use ww_tape::{
    BlockId, BlockKind, BlockWriter, ClockOrigin, ClockSpec, ClockTime, Definitions, EventsDef,
    FLAG_REPEAT, FullVersion, Magnitude, MemSink, Segment, SessionTag, Slot, SyncPolicy,
    TapeReader, TapeSink, TapeWriter, TrackKind, TrackSpec, Version, clock_key, track_key,
};
use ww_tape::{ClockNum, TrackNum};
use ww_tape_bench::{Bench, Cell, Common};

#[derive(Parser)]
#[command(
    name = "tracks",
    about = "ww_tape tracks added one by one: define, a records block, close and read",
    version = vc_build::version!(),
    after_long_help = "Examples:
  just bench-tracks
  just bench-tracks --tracks 100000
  tracks --json docs/bench/tracks.json"
)]
struct Args {
    #[command(flatten)]
    common: Common,
    /// Tracks to add.
    #[arg(long, default_value_t = 10_000)]
    tracks: u32,
    /// Size of the piece the put and copied bytes are counted in, MiB.
    #[arg(long, default_value_t = 256)]
    piece_mib: u64,
    /// Size of each records block of data, KiB.
    #[arg(long, default_value_t = 4)]
    block_kib: usize,
}

/// A sink that keeps no bytes: it counts them, and those of blocks marked FLAG_REPEAT.
struct Counting {
    bytes: u64,
    repeat: u64,
}

impl TapeSink for Counting {
    type Error = io::Error;

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        let len = bytes.len() as u64;
        self.bytes += len;
        // the writer hands over one whole block per call: its flags are header bytes 6 and 7
        if let [_, _, _, _, _, _, lo, hi, ..] = bytes
            && u16::from_le_bytes([*lo, *hi]) & FLAG_REPEAT != 0
        {
            self.repeat += len;
        }
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
            "a counting sink keeps no bytes",
        ))
    }

    fn truncate(&mut self, _: u64) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "a counting sink keeps no bytes",
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

/// Bytes put or copied, and bytes in all, in a piece of `piece` bytes with `tracks` tracks defined and records
/// blocks of `block` bytes on the first `in_use` of them in turn.
fn repeat_bytes(tracks: u32, in_use: u32, piece: u64, block: usize) -> anyhow::Result<(u64, u64)> {
    let e = |e: ww_tape::WriteError<_>| anyhow::anyhow!("{e:?}");
    let sink = Counting {
        bytes: 0,
        repeat: 0,
    };
    let mut w = TapeWriter::create_on(sink, SyncPolicy::None);
    let mut defs = Definitions::new();
    let clock = defs.define_clock(clock()).map_err(d)?;
    let numbers = (1..=tracks)
        .map(|n| defs.define_track(track(n, clock)))
        .collect::<Result<Vec<TrackNum>, _>>()
        .map_err(d)?;
    let item = vec![0x5a; block.saturating_sub(64)];
    let mut n = 0u32;
    while w.sink().bytes < piece {
        let t = numbers[(n % in_use) as usize];
        let mut buf = vec![0u8; block];
        let mut b = BlockWriter::new(&mut buf, BlockKind::Records, w.next_id(), &[clock])
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        b.push(t, ClockTime::new(clock, i64::from(n)), &[&item])
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let len = b.finish();
        w.write_block(&defs, &buf[..len]).map_err(e)?;
        n = n.wrapping_add(1);
    }
    Ok((w.sink().repeat, w.sink().bytes))
}

/// Bytes hashed and copied per records block on a track whose definition is `label_len` long, over `blocks` blocks
/// after the first one (which puts the definition): what a block costs when nothing needs writing.
fn idle_work(blocks: u32, label_len: usize) -> anyhow::Result<(f64, f64)> {
    let e = |e: ww_tape::WriteError<_>| anyhow::anyhow!("{e:?}");
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    let label = "x".repeat(label_len);
    let mut defs = Definitions::new();
    let clock = defs.define_clock(clock()).map_err(d)?;
    let t = defs
        .define_track(TrackSpec {
            label: &label,
            ..track(1, clock)
        })
        .map_err(d)?;
    w.write_block(&defs, &records(w.next_id(), clock, t, 1)?)
        .map_err(e)?;
    let first = w.put_work();
    for _ in 0..blocks {
        w.write_block(&defs, &records(w.next_id(), clock, t, 1)?)
            .map_err(e)?;
    }
    let work = w.put_work();
    let per = |a: u64, b: u64| a.saturating_sub(b) as f64 / f64::from(blocks);
    Ok((
        per(work.hashed.get(), first.hashed.get()),
        per(work.copied.get(), first.copied.get()),
    ))
}

/// The clock of every track.
fn clock() -> ClockSpec<'static> {
    ClockSpec {
        key: clock_key(0xb7, &[], 1),
        rate: Magnitude::pow10(9).into(),
        resolution: Magnitude::pow10(-9).into(),
        origin: ClockOrigin::Boot,
        label: "bench",
    }
}

fn d(e: ww_tape::DescribeError) -> anyhow::Error {
    anyhow::anyhow!("{e:?}")
}

/// Track `index` on `clock`, keyed by its index.
fn track(index: u32, clock: ClockNum) -> TrackSpec<'static> {
    TrackSpec {
        key: track_key(0xb7, &[Segment::Index(index)]),
        clock,
        dimensions: RefVec::Slice { slice: &[] },
        items: ww_tape::Items::WireWeaver {
            api: FullVersion::new("bench_api", Version::new(0, 1, 0)),
            bundle: RefVec::Slice { slice: &[] },
            path: RefVec::Slice {
                slice: &[UNib32(1)],
            },
            keys: RefVec::Slice { slice: &[] },
            item: None,
        },
        label: "bench",
        repeat: ww_tape::TrackRepeat::Stream,
        kind: TrackKind::Events(EventsDef::INSTANTS),
        relations: RefVec::Slice { slice: &[] },
        owner: [0; 32],
    }
}

/// A records block of one record at tick `tick` of `clock` on `track`.
fn records(id: BlockId, clock: ClockNum, track: TrackNum, tick: u32) -> anyhow::Result<Vec<u8>> {
    let mut buf = [0u8; 128];
    let mut b = BlockWriter::new(&mut buf, BlockKind::Records, id, &[clock])
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    b.push(track, ClockTime::new(clock, i64::from(tick)), &[b"x"])
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let len = b.finish();
    Ok(buf[..len].to_vec())
}

fn main() -> anyhow::Result<()> {
    let args: Args = ww_tape_bench::args();
    let e = |e: ww_tape::WriteError<_>| anyhow::anyhow!("{e:?}");
    let mut bench = Bench::start(
        "tracks",
        vc_build::build_info!(),
        &args.common,
        vec![
            ("tracks", args.tracks.to_string()),
            ("piece MiB", args.piece_mib.to_string()),
            ("block KiB", args.block_kib.to_string()),
        ],
    )?;
    println!("{}", bench.header());
    let mut add = bench.table(
        "add",
        &format!(
            "{} tracks added one by one, a records block each, into a piece in memory.",
            args.tracks
        ),
        &["What", "s"],
    );
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    let tape = w.sink().tape();
    let owner = OwnerId([7; 32]);
    let head = PieceHead {
        ident: PieceIdent {
            id: PieceId::first(&owner),
            owner,
            seq: 0,
        },
        prev: None,
        prev_session: SessionTag::NONE,
        integrity: ww_tape::Integrity::Full,
    };
    w.set_producer("bench");
    w.begin_piece(&head, &[]).map_err(e)?;
    let mut defs = Definitions::new();
    let clock = defs.define_clock(clock()).map_err(d)?;
    let marks = [1_000, 5_000, args.tracks];
    let mut at = vec![];
    let t = Instant::now();
    for n in 1..=args.tracks {
        let number = defs.define_track(track(n, clock)).map_err(d)?;
        w.write_block(&defs, &records(w.next_id(), clock, number, n)?)
            .map_err(e)?;
        if marks.contains(&n) {
            at.push((n, t.elapsed().as_secs_f64()));
        }
    }
    let t = Instant::now();
    w.close().map_err(e)?;
    let close = t.elapsed().as_secs_f64();
    let bytes = tape.to_vec().map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let t = Instant::now();
    let r = TapeReader::open(&bytes[..]).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let read_defs = r
        .definitions_at(0, u64::MAX)
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let read = t.elapsed().as_secs_f64();
    let tracks = read_defs
        .items
        .keys()
        .filter(|s| matches!(s, Slot::Track { .. }))
        .count();
    anyhow::ensure!(tracks == args.tracks as usize, "{tracks} tracks read back");
    add.table.title = format!(
        "{} tracks added one by one, a records block each, into a piece in memory of {} bytes.",
        args.tracks,
        bytes.len()
    );
    for (n, secs) in at {
        add.row(vec![
            Cell::text(format!("add {n} tracks")),
            Cell::num(secs, 3),
        ]);
    }
    add.row(vec![Cell::text("close the piece"), Cell::num(close, 3)]);
    add.row(vec![
        Cell::text("open it and read the definitions"),
        Cell::num(read, 3),
    ]);
    println!("{}", bench.push(add));
    let piece = args.piece_mib << 20;
    let block = args.block_kib << 10;
    let mut puts = bench.table(
        "puts",
        &format!(
            "Put and copied bytes in a piece of {} MiB, {} tracks defined, records blocks of {} KiB on the tracks in use in turn; the default policies: a near of {} MiB, a copy {} MiB after the newest record.",
            args.piece_mib,
            args.tracks,
            args.block_kib,
            ww_tape::NEAR_DEFAULT >> 20,
            ww_tape::NearPolicy::default().copy_spacing.get() >> 20
        ),
        &["Tracks in use", "Put and copied bytes", "Of the piece, %"],
    );
    for in_use in [args.tracks, (args.tracks / 100).max(1)] {
        let (repeat, all) = repeat_bytes(args.tracks, in_use, piece, block)?;
        puts.row(vec![
            Cell::text(in_use.to_string()),
            Cell::count(repeat),
            Cell::num(repeat as f64 * 100.0 / all as f64, 3),
        ]);
    }
    println!("{}", bench.push(puts));
    let (blocks, label) = (10_000, 60 << 10);
    let mut idle = bench.table(
        "idle",
        &format!(
            "Work per records block when nothing needs writing: {blocks} blocks on a track whose definition is {} KiB.",
            label >> 10
        ),
        &["What", "Bytes per block"],
    );
    let (hashed, copied) = idle_work(blocks, label)?;
    idle.row(vec![Cell::text("hashed"), Cell::num(hashed, 1)]);
    idle.row(vec![Cell::text("copied"), Cell::num(copied, 1)]);
    println!("{}", bench.push(idle));
    bench.finish()
}
