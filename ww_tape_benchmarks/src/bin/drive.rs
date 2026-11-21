//! The book's running example at full scale: the drive motor controller's hour ([ww_tape_example::Drive::full]),
//! recorded into a chain of pieces in memory. Prints what the recording holds by block kind, the bytes per item of its
//! streams, and the time it takes to record the hour with and without summaries (the example makes the signals as it
//! goes, so this is not the writer's cost alone; `chunks` measures that), the best of `--rounds` rounds, with the load average before and after each table.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::print_stdout,
    reason = "a bench reads no file of others: it computes sizes and rates freely and prints its table"
)]

use clap::Parser;
use std::time::Instant;
use ww_tape::{BlockKind, FLAG_REPEAT, FileBlocks};
use ww_tape_bench::{Bench, Cell, Common, Measure};
use ww_tape_example::{CAN0, Drive, PHASE_A, Recording, frame};

#[derive(Parser)]
#[command(
    name = "drive",
    about = "the drive's hour at full scale: its bytes by block kind and the writer's time",
    version = vc_build::version!(),
    after_long_help = "Examples:
  just bench-drive
  just bench-drive --rounds 1
  drive --json docs/bench/drive.json"
)]
struct Args {
    #[command(flatten)]
    common: Common,
    /// Rounds of each recording; the fastest counts.
    #[arg(long, default_value_t = 3)]
    rounds: u32,
}

/// The parts of a recording the sizes table names, in its order.
const PARTS: [&str; 8] = [
    "series chunks of `phase_a`",
    "events chunks of `can0`",
    "summaries of `phase_a`",
    "records blocks of `log`, `temp` and `mode`",
    "puts and copies",
    "checkpoints and final indexes",
    "piece heads, closes and tails",
    "other blocks",
];

/// The part block `kind` with `flags` belongs to: an index into [PARTS].
fn part(kind: BlockKind, flags: u16) -> usize {
    match kind {
        BlockKind::Series => 0,
        BlockKind::Events => 1,
        BlockKind::Summary => 2,
        BlockKind::Records if flags & FLAG_REPEAT != 0 => 4,
        BlockKind::Records => 3,
        BlockKind::Index => 5,
        BlockKind::PieceHead | BlockKind::Close | BlockKind::Tail => 6,
        _ => 7,
    }
}

/// Bytes of each part of [PARTS] in the recording.
fn parts(rec: &Recording) -> anyhow::Result<[u64; PARTS.len()]> {
    let mut bytes = [0u64; PARTS.len()];
    for piece in &rec.pieces {
        let mut walk = FileBlocks::new(&piece[..]);
        while let Some(b) = walk.next_block() {
            let b = b.map_err(|e| anyhow::anyhow!("{e:?}"))?;
            if let Some(slot) = bytes.get_mut(part(b.kind, b.flags)) {
                *slot += b.len() as u64;
            }
        }
    }
    Ok(bytes)
}

/// The fastest of `rounds` recordings of `drive`, and the last one.
fn record(drive: &Drive, rounds: u32) -> anyhow::Result<(f64, Recording)> {
    let mut best = f64::MAX;
    let mut last = None;
    for _ in 0..rounds {
        let t = Instant::now();
        let rec = drive.record()?;
        best = best.min(t.elapsed().as_secs_f64());
        last = Some(rec);
    }
    Ok((best, last.ok_or_else(|| anyhow::anyhow!("rounds is 0"))?))
}

fn sizes(bench: &Bench, rec: &Recording) -> anyhow::Result<Measure> {
    let total = rec.len();
    let seconds = f64::from(rec.drive.minutes) * 60.0;
    let mut t = bench.table(
        "sizes",
        &format!(
            "The drive's hour at full scale: {} pieces, {total} bytes.",
            rec.pieces.len()
        ),
        &[
            "Part",
            "Bytes",
            "Of the recording, %",
            "Bytes per second of the run",
        ],
    );
    let parts = parts(rec)?;
    let walked: u64 = parts.iter().sum();
    for (name, bytes) in PARTS.iter().zip(parts) {
        t.row(vec![
            Cell::text(*name),
            Cell::count(bytes),
            Cell::num(bytes as f64 * 100.0 / total as f64, 2),
            Cell::num(bytes as f64 / seconds, 0),
        ]);
    }
    // zeros between blocks: the 4 KiB end of each piece, and gaps before aligned payloads
    let gaps = total - walked;
    t.row(vec![
        Cell::text("zeros between blocks"),
        Cell::count(gaps),
        Cell::num(gaps as f64 * 100.0 / total as f64, 2),
        Cell::num(gaps as f64 / seconds, 0),
    ]);
    t.row(vec![
        Cell::text("the recording"),
        Cell::count(total),
        Cell::num(100.0, 2),
        Cell::num(total as f64 / seconds, 0),
    ]);
    Ok(t)
}

fn items(bench: &Bench, rec: &Recording) -> Measure {
    let mut t = bench.table(
        "items",
        "Bytes per item of the drive's streams, their chunks whole.",
        &[
            "Stream",
            "Items",
            "Items per second",
            "Chunk bytes",
            "Bytes per item",
            "Raw bytes per item",
        ],
    );
    // raw: a frame's 64-bit time and its payload, a sample's 16 bits
    let (_, frames) = rec.chunk_bytes(CAN0);
    let period_us = 1_000_000 / u64::from(rec.drive.can_hz);
    let payloads: u64 = (0..frames)
        .map(|k| frame(k, period_us).1.len() as u64)
        .sum();
    let raw_frame = (8 * frames + payloads) as f64 / frames as f64;
    let seconds = f64::from(rec.drive.minutes) * 60.0;
    for (name, track, raw) in [
        ("`can0` frames", CAN0, raw_frame),
        ("`phase_a` samples", PHASE_A, size_of::<i16>() as f64),
    ] {
        let (bytes, n) = rec.chunk_bytes(track);
        t.row(vec![
            Cell::text(name),
            Cell::count(n),
            Cell::num(n as f64 / seconds, 0),
            Cell::count(bytes),
            Cell::num(bytes as f64 / n as f64, 3),
            Cell::num(raw, 3),
        ]);
    }
    t
}

fn main() -> anyhow::Result<()> {
    let a: Args = ww_tape_bench::args();
    anyhow::ensure!(a.rounds > 0, "rounds must be positive");
    let mut bench = Bench::start(
        "drive",
        vc_build::build_info!(),
        &a.common,
        vec![("rounds", a.rounds.to_string())],
    )?;
    println!("{}", bench.header());
    let drive = Drive::full();
    let mut time = bench.table(
        "write",
        &format!(
            "The drive's hour recorded into memory, its signals made by the example as it goes, the best of {} rounds.",
            a.rounds
        ),
        &["Recording", "Seconds", "MB/s of the recording", "Times faster than the run"],
    );
    let (secs, rec) = record(&drive, a.rounds)?;
    let (plain_secs, plain) = record(
        &Drive {
            summaries: false,
            ..drive
        },
        a.rounds,
    )?;
    let run = f64::from(drive.minutes) * 60.0;
    for (name, s, r) in [
        ("with summaries of `phase_a`", secs, &rec),
        ("without summaries", plain_secs, &plain),
    ] {
        time.row(vec![
            Cell::text(name),
            Cell::num(s, 2),
            Cell::num(r.len() as f64 / s / 1e6, 0),
            Cell::num(run / s, 0),
        ]);
    }
    println!("{}", bench.push(time));
    let t = sizes(&bench, &rec)?;
    println!("{}", bench.push(t));
    let t = items(&bench, &rec);
    println!("{}", bench.push(t));
    bench.finish()
}
