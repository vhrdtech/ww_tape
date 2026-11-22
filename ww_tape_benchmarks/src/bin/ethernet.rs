//! The protocol analyser of `ww_tape_example::ethernet` at full speed: both directions of a 10 Gbps link through
//! one `TapeMux`, into a store that keeps no bytes and into a chain of piece files.
//!
//! Per port a capture thread makes the frames and pushes them, and a decoder thread pushes their IPv4 and TCP or UDP
//! headers: four producer threads and the mux's writing thread. The frames come as fast as the threads take them,
//! not at the link's pace, so a run says how fast the analyser could record against the link it watches. Each table
//! carries the load average before and after it.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::print_stdout,
    reason = "a bench reads no file of others: it computes sizes and rates freely and prints its table"
)]

use clap::Parser;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use ww_tape::{ChainOptions, CutPolicy, OwnerId, Tapes};
use ww_tape_bench::{Bench, Cell, Common, Discard};
use ww_tape_example::ethernet::{Captured, LINE_BPS, Link, chain_options};
use ww_tape_mux::{MuxOptions, TapeMux};

#[derive(Parser)]
#[command(
    name = "ethernet",
    about = "A protocol analyser on both directions of a 10 Gbps link, through one mux or a mux per port, at full speed",
    version = vc_build::version!(),
    after_long_help = "Examples:
  just bench-ethernet --dir target/bench-ethernet
  ethernet --ms 500 --queues 1,2,4,8 --muxes 1,2"
)]
struct Args {
    #[command(flatten)]
    common: Common,
    /// The directory of the chains of piece files, removed after each run; none: the store that keeps no bytes only.
    #[arg(long)]
    dir: Option<PathBuf>,
    /// Milliseconds of the link a run records.
    #[arg(long, default_value_t = 2_000)]
    ms: u64,
    /// The link's load, percent of the line rate in each direction.
    #[arg(long, default_value_t = 100)]
    load: u64,
    /// The receive queues of a port, each a capture and a decoder thread, a run per number.
    #[arg(long, value_delimiter = ',', default_values_t = [1, 2, 4])]
    queues: Vec<u32>,
    /// The muxes, one chain each: 1 for both ports, 2 for one per port; a run per number.
    #[arg(long, value_delimiter = ',', default_values_t = [1, 2])]
    muxes: Vec<usize>,
    /// MiB of a piece of a chain of files.
    #[arg(long, default_value_t = 1_024)]
    piece_mib: u64,
}

/// An error of the library, kept as its debug text.
fn err<E: core::fmt::Debug>(e: E) -> anyhow::Error {
    anyhow::anyhow!("{e:?}")
}

const OWNER: OwnerId = OwnerId([0xe7; 32]);

/// What one run took.
struct Run {
    captured: Captured,
    wall: Duration,
    written: u64,
    waits: u64,
}

/// Record `link` through `muxes` and close them: what the run took since `start`.
fn run<T>(link: Link, muxes: Vec<TapeMux<T>>, start: Instant) -> anyhow::Result<Run>
where
    T: Tapes + Send + 'static,
    T::Sink: Send,
    T::Key: Send,
    T::Error: Send + Sync + core::fmt::Debug,
{
    let refs: Vec<&TapeMux<T>> = muxes.iter().collect();
    let captured = link.record(&refs).map_err(err)?;
    let (mut written, mut waits) = (0, 0);
    for mux in muxes {
        let stats = mux.stats();
        let closed = mux.close().map_err(err)?;
        anyhow::ensure!(
            stats.drops == 0 && stats.refused == 0 && stats.lost == 0,
            "a mux lost units: {stats:?}"
        );
        written += closed.iter().map(|p| p.piece.len).sum::<u64>();
        waits += stats.waits;
    }
    Ok(Run {
        captured,
        wall: start.elapsed(),
        written,
        waits,
    })
}

fn main() -> anyhow::Result<()> {
    let args: Args = ww_tape_bench::args();
    let mut bench = Bench::start(
        "ethernet",
        vc_build::build_info!(),
        &args.common,
        vec![
            ("ms", args.ms.to_string()),
            ("load %", args.load.to_string()),
            ("piece MiB", args.piece_mib.to_string()),
        ],
    )?;
    println!("{}", bench.header());
    let mut table = bench.table(
        "writer",
        &format!(
            "{} ms of both directions of a 10 Gbps link at {} % load: per receive queue a capture and a decoder \
             thread. The frames come as fast as the threads take them.",
            args.ms, args.load
        ),
        &[
            "Store",
            "Queues a port",
            "Muxes",
            "Frames",
            "Wall ms",
            "M frames/s",
            "Gbit/s",
            "Of the link",
            "MB/s written",
            "Waits",
        ],
    );
    for &muxes in &args.muxes {
        for &queues in &args.queues {
            let link = Link::full()
                .micros(args.ms * 1_000)
                .load(args.load)
                .queues(queues);
            let start = Instant::now();
            let mut m = Vec::new();
            for chain in 0..muxes as u64 {
                let options = options(args.piece_mib);
                m.push(
                    TapeMux::create(
                        Discard::default(),
                        OWNER,
                        chain,
                        &[],
                        options,
                        MuxOptions::default(),
                    )
                    .map_err(err)?,
                );
            }
            let r = run(link, m, start)?;
            table.row(row("no bytes kept", queues, muxes, args.load, &r));
        }
        let (Some(dir), Some(&queues)) = (&args.dir, args.queues.iter().max()) else {
            continue;
        };
        anyhow::ensure!(
            !dir.exists() || std::fs::read_dir(dir)?.next().is_none(),
            "{} is not empty",
            dir.display()
        );
        let link = Link::full()
            .micros(args.ms * 1_000)
            .load(args.load)
            .queues(queues);
        let start = Instant::now();
        let mut m = Vec::new();
        for chain in 0..muxes as u64 {
            let d = dir.join(format!("chain-{chain}"));
            std::fs::create_dir_all(&d)?;
            let options = options(args.piece_mib);
            m.push(
                ww_tape_fs::mux::create(&d, OWNER, chain, &[], options, MuxOptions::default())
                    .map_err(err)?,
            );
        }
        let r = run(link, m, start)?;
        table.row(row("piece files", queues, muxes, args.load, &r));
        std::fs::remove_dir_all(dir)?;
    }
    println!("{}", bench.push(table));
    bench.finish()
}

/// The analyser's chain options, cut into pieces of `mib` MiB.
fn options(mib: u64) -> ChainOptions {
    ChainOptions {
        cut: CutPolicy {
            bytes: exact_units::Bytes::new(mib << 20),
            ..CutPolicy::default()
        },
        ..chain_options()
    }
}

/// The row of run `r` into `store` with `queues` a port and `muxes` at `load` percent.
fn row(store: &str, queues: u32, muxes: usize, load: u64, r: &Run) -> Vec<Cell> {
    let secs = r.wall.as_secs_f64();
    let [a, b] = &r.captured.ports;
    let frames = a.frames + b.frames;
    let wire_bits = (a.wire_bytes + b.wire_bytes) as f64 * 8.0;
    // both directions at the load, as the link carries them
    let link_bits_per_s = 2.0 * LINE_BPS as f64 * load as f64 / 100.0;
    vec![
        Cell::text(store),
        Cell::count(u64::from(queues)),
        Cell::count(muxes as u64),
        Cell::count(frames),
        Cell::num(secs * 1e3, 0),
        Cell::num(frames as f64 / secs / 1e6, 2),
        Cell::num(wire_bits / secs / 1e9, 1),
        Cell::num(wire_bits / secs / link_bits_per_s, 2),
        Cell::num(r.written as f64 / secs / 1e6, 0),
        Cell::count(r.waits),
    ]
}
