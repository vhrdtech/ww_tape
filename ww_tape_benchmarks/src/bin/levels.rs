//! What a writer costs per chunk at each integrity level (`WriterOptions::integrity`), and what sealing a piece of a
//! lower level costs (`MemTapes::seal`): i16 series chunks of 1 and 4 MiB and events chunks of 16 KiB, into a memory
//! tape reserved before the timer starts. Prints Markdown tables of ns per chunk, ns per byte and GB/s, the best of
//! `--rounds` rounds, with the load average around them. The book's "When bytes go bad" draws from the report
//! (`--json`).
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::indexing_slicing,
    clippy::print_stdout,
    reason = "a bench reads no file of others: it computes sizes and rates freely, fails by panicking and prints its table"
)]

use clap::Parser;
use shrink_wrap::prelude::*;
use std::time::Instant;
use ww_tape::chunk::ChunkHead;
use ww_tape::piece::{OwnerId, PieceHead, PieceId, PieceIdent};
use ww_tape::{
    BlockKind, Codec, ColumnKind, ColumnSpec, Definitions, EndMode, Integrity, ItemNum, MemSink,
    MemTapes, Segment, SessionTag, SyncPolicy, TapeSink, TapeWriter, TrackKind, TrackNum,
    TrackSpec, track_key,
};
use ww_tape_bench::{Bench, Cell, Common, Measure};
use ww_tape_benchmarks::series::{definitions, head};

#[derive(Parser)]
#[command(
    name = "levels",
    about = "ww_tape integrity levels: ns per chunk written at Full, CrcOnly and None, and of a seal to Full",
    version = vc_build::version!(),
    after_long_help = "Examples:
  just bench-levels
  just bench-levels --mib 16 --rounds 3
  levels --json docs/bench/levels.json"
)]
struct Args {
    #[command(flatten)]
    common: Common,
    /// MiB of chunks per piece, each shape.
    #[arg(long, default_value_t = 64)]
    mib: usize,
    /// Rounds; the best is kept.
    #[arg(long, default_value_t = 5)]
    rounds: u32,
}

/// The levels, by the names the tables give them.
const LEVELS: [(Integrity, &str); 3] = [
    (Integrity::Full, "Full"),
    (Integrity::CrcOnly, "CrcOnly"),
    (Integrity::None, "None"),
];

/// A shape of chunk: its name, the column's kind and bytes, and the head of chunk `k`.
struct Shape {
    name: &'static str,
    kind: ColumnKind,
    bytes: Vec<u8>,
    head: Box<dyn Fn(u64) -> anyhow::Result<ChunkHead>>,
}

/// The shapes measured, with the definitions of their tracks.
fn shapes() -> anyhow::Result<(Definitions, Vec<Shape>)> {
    let (mut defs, series) = definitions()?;
    let events = defs
        .define_track(TrackSpec {
            key: track_key(0xc4, &[Segment::Name("events")]),
            clock: ww_tape::ClockNum::new(1),
            dimensions: RefVec::Slice { slice: &[] },
            items: ww_tape::Items::numbers::<i16>(None),
            label: "bench",
            repeat: ww_tape::TrackRepeat::Stream,
            kind: TrackKind::Events(ww_tape::EventsDef::INSTANTS),
            relations: RefVec::Slice { slice: &[] },
            owner: [0; 32],
        })
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let samples = |kib: usize| (0..kib * 1024).map(|i| (i * 31 % 251) as u8).collect();
    let series_shape = |name, kib: usize| Shape {
        name,
        kind: ColumnKind::Samples,
        bytes: samples(kib),
        head: Box::new(move |k| head(series, k, (kib * 512) as u32)),
    };
    // 2,048 events, 8 bytes of start each
    let starts: Vec<u8> = (0..2048i64).flat_map(|i| (i * 10).to_le_bytes()).collect();
    let events_shape = Shape {
        name: "events 16 KiB",
        kind: ColumnKind::Starts,
        bytes: starts,
        head: Box::new(move |k| Ok(events_head(events, k))),
    };
    Ok((
        defs,
        vec![
            series_shape("series 1 MiB", 1024),
            series_shape("series 4 MiB", 4096),
            events_shape,
        ],
    ))
}

/// The head of events chunk `k` of 2,048 instants 10 ticks apart on track `track`.
fn events_head(track: TrackNum, k: u64) -> ChunkHead {
    let first = k * 2048;
    let tick = |i: u64| (i * 10).cast_signed();
    ChunkHead {
        kind: BlockKind::Events,
        track_id: track_key(0xc4, &[Segment::Name("events")]).id(),
        track_number: track,
        first_item: ItemNum::new(first),
        count: 2048,
        first_tick: tick(first),
        last_tick: tick(first + 2047),
        latest_end: tick(first + 2047),
        time_mode: 0,
        end_mode: EndMode::Absent,
        grid: None,
    }
}

/// A writer of a first piece of `level` on a memory sink reserved for `bytes`.
fn writer(level: Integrity, bytes: usize) -> anyhow::Result<TapeWriter<MemSink>> {
    let mut sink = MemSink::new();
    sink.reserve(bytes as u64 + (1 << 20));
    let mut w = TapeWriter::create_on(sink, SyncPolicy::None);
    let owner = OwnerId([0xb1; 32]);
    let head = PieceHead {
        ident: PieceIdent {
            id: PieceId::first(&owner),
            owner,
            seq: 0,
        },
        prev: None,
        prev_session: SessionTag::NONE,
        integrity: level,
    };
    w.begin_piece(&head, &[])
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    Ok(w)
}

/// ns per chunk of writing `n` chunks of `shape` at each level, and of sealing the pieces of the lower levels.
fn measure(bench: &Bench, args: &Args) -> anyhow::Result<(Measure, Measure)> {
    let (defs, shapes) = shapes()?;
    let mut write = bench.table(
        "write",
        &format!(
            "`TapeWriter::write_chunk` into a reserved memory tape, {} MiB of chunks of each shape, at each level.",
            args.mib
        ),
        &["Chunk", "Level", "ns per chunk", "ns per byte", "GB/s"],
    );
    let mut seal = bench.table(
        "seal",
        &format!(
            "`MemTapes::seal` of a closed piece of {} MiB of chunks of each shape, to Full.",
            args.mib
        ),
        &["Chunk", "From", "ns per chunk", "ns per byte", "GB/s"],
    );
    let e = |e: ww_tape::WriteError<ww_tape::MemError>| anyhow::anyhow!("{e:?}");
    for shape in &shapes {
        let spec = ColumnSpec {
            kind: shape.kind,
            codec: Codec::RAW,
            param: 0,
            len: shape.bytes.len(),
        };
        let columns = [(spec, shape.bytes.as_slice())];
        let len = shape.bytes.len() as f64;
        let n = (args.mib << 20).div_ceil(shape.bytes.len()).max(1) as u64;
        let heads: Vec<ChunkHead> = (0..n).map(&shape.head).collect::<anyhow::Result<_>>()?;
        for (level, name) in LEVELS {
            let mut best = f64::MAX;
            let mut piece = Vec::new();
            for _ in 0..args.rounds {
                let mut w = writer(level, args.mib << 20)?;
                let t = Instant::now();
                for h in &heads {
                    w.write_chunk(&defs, h, &columns).map_err(e)?;
                }
                best = best.min(t.elapsed().as_nanos() as f64 / n as f64);
                let tape = w.sink().tape();
                w.close().map_err(e)?;
                piece = tape.to_vec().map_err(|e| anyhow::anyhow!("{e:?}"))?;
            }
            write.row(vec![
                Cell::text(shape.name),
                Cell::text(name),
                Cell::num(best, 0),
                Cell::num(best / len, 3),
                Cell::num(len / best, 2),
            ]);
            if level == Integrity::Full {
                continue;
            }
            let mut best = f64::MAX;
            for _ in 0..args.rounds {
                let mem = MemTapes::new();
                let mut sink = mem.create(&0).map_err(|e| anyhow::anyhow!("{e:?}"))?;
                sink.write_all(&piece)
                    .map_err(|e| anyhow::anyhow!("{e:?}"))?;
                sink.close().map_err(|e| anyhow::anyhow!("{e:?}"))?;
                drop(sink);
                let t = Instant::now();
                let sealed = mem.seal(&0, None).map_err(e)?;
                best = best.min(t.elapsed().as_nanos() as f64 / n as f64);
                anyhow::ensure!(sealed.is_some_and(|s| s.len() == 1), "one piece sealed");
            }
            seal.row(vec![
                Cell::text(shape.name),
                Cell::text(name),
                Cell::num(best, 0),
                Cell::num(best / len, 3),
                Cell::num(len / best, 2),
            ]);
        }
    }
    Ok((write, seal))
}

fn main() -> anyhow::Result<()> {
    let args: Args = ww_tape_bench::args();
    let mut bench = Bench::start(
        "levels",
        vc_build::build_info!(),
        &args.common,
        vec![
            ("MiB", args.mib.to_string()),
            ("rounds", args.rounds.to_string()),
        ],
    )?;
    println!("{}", bench.header());
    let (write, seal) = measure(&bench, &args)?;
    println!("{}", bench.push(write));
    println!("{}", bench.push(seal));
    bench.finish()
}
