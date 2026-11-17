//! Tracks added one by one: the application defines each new track in its `Definitions`, writes one records block
//! on it (the writer puts the definition before it), and finishes the piece; a reader then opens it and takes its definitions.
//! Prints a Markdown table of the time to add the first 1 000, 5 000 and all tracks, the finish, and the read, with
//! the load average at the start.
//!
//! Then the bytes a writer puts and copies (blocks marked FLAG_REPEAT) in one piece of `--piece-mib` with all the
//! tracks defined, records blocks of `--block-kib` on the tracks in use in turn: all of them, and 1 % of them, with
//! the default policies (definitions put again 32 MiB on, one copy 1 MiB after a first write).
//!
//! Last, the bytes a writer hashes and copies per block of data when nothing needs writing: a track whose
//! definition is 60 KiB long, written to again and again within its near.

use clap::Parser;
use shrink_wrap::prelude::*;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;
use ww_tape::piece::{OwnerId, PieceHead, PieceId, PieceIdent};
use ww_tape::{
    BlockId, BlockKind, BlockWriter, ClockDef, ClockOrigin, ClockTime, Definitions, Describe,
    EventsDef, FLAG_REPEAT, FullVersion, Magnitude, MemSink, Segment, SessionTag, Slot, SyncPolicy,
    TapeReader, TapeSink, TapeWriter, TrackDef, TrackKind, Version, clock_key, track_key,
};

#[derive(Parser)]
#[command(about = "ww_tape tracks added one by one: define, a records block, finish and read")]
struct Args {
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
    path: PathBuf,
    bytes: u64,
    repeat: u64,
}

impl TapeSink for Counting {
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

    fn finish(&mut self) -> io::Result<()> {
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

    fn path(&self) -> &Path {
        &self.path
    }
}

/// Bytes put or copied, and bytes in all, in a piece of `piece` bytes with `tracks` tracks defined and records
/// blocks of `block` bytes on the first `in_use` of them in turn.
fn repeat_bytes(tracks: u32, in_use: u32, piece: u64, block: usize) -> anyhow::Result<(u64, u64)> {
    let e = |e: ww_tape::WriteError| anyhow::anyhow!("{e:?}");
    let sink = Counting {
        path: PathBuf::from("repeats"),
        bytes: 0,
        repeat: 0,
    };
    let mut w = TapeWriter::create_on(sink, SyncPolicy::None);
    let mut all = vec![clock()];
    all.extend((1..=tracks).map(track));
    let mut defs = Definitions::new();
    defs.define_all(&all).map_err(d)?;
    let item = vec![0x5a; block.saturating_sub(64)];
    let mut n = 0u32;
    while w.sink().bytes < piece {
        let t = 1 + n % in_use;
        let mut buf = vec![0u8; block];
        let mut b = BlockWriter::new(&mut buf, BlockKind::Records, w.next_id(), &[CLOCK])
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        b.push(t, ClockTime::new(CLOCK, i64::from(n)), &[&item])
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
    let e = |e: ww_tape::WriteError| anyhow::anyhow!("{e:?}");
    let mut w = TapeWriter::create_on(MemSink::new(Path::new("idle")), SyncPolicy::None);
    let label = "x".repeat(label_len);
    let Describe::Track(t) = track(1) else {
        anyhow::bail!("track() gives a track");
    };
    let mut defs = Definitions::new();
    defs.define_all(&[clock(), Describe::Track(TrackDef { label: &label, ..t })])
        .map_err(d)?;
    w.write_block(&defs, &records(w.next_id(), 1)?).map_err(e)?;
    let first = w.put_work();
    for _ in 0..blocks {
        w.write_block(&defs, &records(w.next_id(), 1)?).map_err(e)?;
    }
    let work = w.put_work();
    let per = |a: u64, b: u64| a.saturating_sub(b) as f64 / f64::from(blocks);
    Ok((
        per(work.hashed.get(), first.hashed.get()),
        per(work.copied.get(), first.copied.get()),
    ))
}

/// The clock of every track.
fn clock() -> Describe<'static> {
    Describe::Clock(ClockDef {
        number: UNib32(CLOCK),
        key: clock_key(0xb7, &[], 1),
        rate: Magnitude::pow10(9).into(),
        resolution: Magnitude::pow10(-9).into(),
        origin: ClockOrigin::Boot,
        label: "bench",
    })
}

fn d(e: ww_tape::DescribeError) -> anyhow::Error {
    anyhow::anyhow!("{e:?}")
}

const CLOCK: u32 = 1;

fn track(number: u32) -> Describe<'static> {
    Describe::Track(TrackDef {
        number: UNib32(number),
        key: track_key(0xb7, &[Segment::Index(number)]),
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
        kind: TrackKind::Events(EventsDef::INSTANTS),
    })
}

/// A records block of one record on `track`.
fn records(id: BlockId, track: u32) -> anyhow::Result<Vec<u8>> {
    let mut buf = [0u8; 128];
    let mut b = BlockWriter::new(&mut buf, BlockKind::Records, id, &[CLOCK])
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    b.push(track, ClockTime::new(CLOCK, i64::from(track)), &[b"x"])
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let len = b.finish();
    Ok(buf[..len].to_vec())
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let e = |e: ww_tape::WriteError| anyhow::anyhow!("{e:?}");
    let load = std::fs::read_to_string("/proc/loadavg")
        .map(|l| l.split_whitespace().take(3).collect::<Vec<_>>().join(" "))
        .unwrap_or_else(|_| "unknown".to_owned());
    let mut w = TapeWriter::create_on(MemSink::new(Path::new("tracks")), SyncPolicy::None);
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
    };
    w.set_producer("bench");
    w.begin_piece(&head, &[]).map_err(e)?;
    let mut defs = Definitions::new();
    defs.define(&clock()).map_err(d)?;
    let marks = [1_000, 5_000, args.tracks];
    let mut at = vec![];
    let t = Instant::now();
    for n in 1..=args.tracks {
        defs.define(&track(n)).map_err(d)?;
        w.write_block(&defs, &records(w.next_id(), n)?).map_err(e)?;
        if marks.contains(&n) {
            at.push((n, t.elapsed().as_secs_f64()));
        }
    }
    let t = Instant::now();
    w.finish().map_err(e)?;
    let finish = t.elapsed().as_secs_f64();
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
    println!(
        "{} tracks added one by one, a records block each; a piece of {} bytes. Load at start: {load}.",
        args.tracks,
        bytes.len()
    );
    println!();
    println!("| tracks | add (s) |");
    println!("|---|---|");
    for (n, s) in at {
        println!("| {n} | {s:.3} |");
    }
    println!();
    println!("Finish: {finish:.3} s. Open and read the definitions: {read:.3} s.");
    let piece = args.piece_mib << 20;
    let block = args.block_kib << 10;
    println!();
    println!(
        "Put and copied bytes in a piece of {} MiB, {} tracks defined, records blocks of {} KiB on the tracks in use in turn:",
        args.piece_mib, args.tracks, args.block_kib
    );
    println!();
    println!("| tracks in use | put and copied (bytes) | of the piece |");
    println!("|---|---|---|");
    for in_use in [args.tracks, (args.tracks / 100).max(1)] {
        let (repeat, all) = repeat_bytes(args.tracks, in_use, piece, block)?;
        println!(
            "| {in_use} | {repeat} | {:.3} % |",
            repeat as f64 * 100.0 / all as f64
        );
    }
    let (blocks, label) = (10_000, 60 << 10);
    let (hashed, copied) = idle_work(blocks, label)?;
    println!();
    println!(
        "Work per records block when nothing needs writing ({blocks} blocks on a track whose definition is {} KiB):",
        label >> 10
    );
    println!();
    println!("| hashed (bytes) | copied (bytes) |");
    println!("|---|---|");
    println!("| {hashed:.1} | {copied:.1} |");
    Ok(())
}
