//! The wideband receiver of `ww_tape_example::wideband` at full speed: 1 GS/s of IQ in channels, each a producer
//! thread, over one or several muxes, into a store that keeps no bytes and into chains of piece files.
//!
//! Each channel's thread pushes chunks made before the run (`--replay-chunks` of them, again and again), so the run
//! times the recorder, not the signal's maths. With `--dir` the most channels and muxes also write chains of piece
//! files there, and a reader then draws one channel's overview from its summaries. Each table carries the load
//! average before and after it.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::print_stdout,
    reason = "a bench reads no file of others: it computes sizes and rates freely and prints its table"
)]

use clap::Parser;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use ww_tape::{Codec, OwnerId, Tapes};
use ww_tape_bench::{Bench, Cell, Common, Discard};
use ww_tape_example::wideband::{RATE_HZ, Source, Wideband, overview};
use ww_tape_fs::FileSource;
use ww_tape_mux::{MuxOptions, TapeMux};

#[derive(Parser)]
#[command(
    name = "wideband",
    about = "A wideband receiver at 1 GS/s in channels over one or several muxes, at full speed",
    version = vc_build::version!(),
    after_long_help = "Examples:
  just bench-wideband --dir target/bench-wideband
  wideband --ms 500 --channels 8,16 --muxes 1,2,4,8"
)]
struct Args {
    #[command(flatten)]
    common: Common,
    /// The directory of the chains of piece files, removed after the run; none: the store that keeps no bytes only.
    #[arg(long)]
    dir: Option<PathBuf>,
    /// Milliseconds of the capture a run records.
    #[arg(long, default_value_t = 1_000)]
    ms: u64,
    /// The channels the band is split into, each a producer thread, a run per number.
    #[arg(long, value_delimiter = ',', default_values_t = [4, 8, 16])]
    channels: Vec<u32>,
    /// The muxes, each a chain and a writing thread, a run per number up to the channels.
    #[arg(long, value_delimiter = ',', default_values_t = [1, 2, 4])]
    muxes: Vec<usize>,
    /// Chunks of each channel made before the run and pushed again and again.
    #[arg(long, default_value_t = 16)]
    replay_chunks: u64,
}

/// An error of the library, kept as its debug text.
fn err<E: core::fmt::Debug>(e: E) -> anyhow::Error {
    anyhow::anyhow!("{e:?}")
}

const OWNER: OwnerId = OwnerId([0x5d; 32]);

/// What one run took.
struct Run {
    samples: u64,
    wall: Duration,
    written: u64,
    waits: u64,
    pieces: Vec<Vec<PathBuf>>,
}

/// Record `wb` from `source` through `muxes` and close them: what the run took since `start`.
fn run<T>(
    wb: &Wideband,
    source: &Source,
    muxes: Vec<TapeMux<T>>,
    start: Instant,
) -> anyhow::Result<Run>
where
    T: Tapes + Send + 'static,
    T::Sink: Send,
    T::Key: Send,
    T::Error: Send + Sync + core::fmt::Debug,
{
    let refs: Vec<&TapeMux<T>> = muxes.iter().collect();
    let recorded = wb.record(&refs, source).map_err(err)?;
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
        samples: recorded.samples.iter().sum(),
        wall: start.elapsed(),
        written,
        waits,
        pieces: Vec::new(),
    })
}

fn main() -> anyhow::Result<()> {
    let args: Args = ww_tape_bench::args();
    let mut bench = Bench::start(
        "wideband",
        vc_build::build_info!(),
        &args.common,
        vec![
            ("ms", args.ms.to_string()),
            ("replay chunks", args.replay_chunks.to_string()),
        ],
    )?;
    println!("{}", bench.header());
    let mut table = bench.table(
        "writer",
        &format!(
            "{} ms of 1 GS/s of IQ in channels, a producer thread each, over one or several muxes. The samples come \
             as fast as the threads take them.",
            args.ms
        ),
        &[
            "Store",
            "Channels",
            "Muxes",
            "Codec",
            "Wall ms",
            "MS/s",
            "Of 1 GS/s",
            "MB/s written",
            "Waits",
        ],
    );
    for &channels in &args.channels {
        let wb = Wideband::full().ms(args.ms).channels(channels);
        for codec in [Codec::PACKED12_LSB, Codec::RAW] {
            let wb = wb.codec(codec);
            let source = wb.replay(args.replay_chunks);
            for &muxes in args.muxes.iter().filter(|&&m| m <= channels as usize) {
                let start = Instant::now();
                let mut m = Vec::new();
                for chain in 0..muxes as u64 {
                    let options = wb.chain_options();
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
                let r = run(&wb, &source, m, start)?;
                table.row(row("no bytes kept", &wb, muxes, &r));
            }
        }
    }
    let files = match (
        &args.dir,
        args.channels.iter().max(),
        args.muxes.iter().max(),
    ) {
        (Some(dir), Some(&channels), Some(&muxes)) => {
            let wb = Wideband::full().ms(args.ms).channels(channels);
            let muxes = muxes.min(channels as usize);
            let r = to_files(dir, &wb, muxes, &wb.replay(args.replay_chunks))?;
            table.row(row("piece files", &wb, muxes, &r));
            Some((wb, r))
        }
        _ => None,
    };
    println!("{}", bench.push(table));
    let Some((wb, r)) = files else {
        println!("{}", bench.skipped("overview", "no --dir"));
        return bench.finish();
    };
    let mut table = bench.table(
        "overview",
        &format!(
            "Channel 0's overview from its chain of piece files: every piece opened and one level of summary nodes \
             read, {} ms of {} MS/s.",
            args.ms,
            wb.channel_rate() / 1_000_000
        ),
        &["Level", "Samples a node", "Nodes", "ms", "Bytes read", "Reads", "Chain bytes"],
    );
    let chain = r.pieces.first().map(Vec::as_slice).unwrap_or_default();
    let chain_bytes: u64 = chain
        .iter()
        .map(|p| std::fs::metadata(p).map(|m| m.len()))
        .sum::<std::io::Result<u64>>()?;
    for level in [2, 3] {
        let t = Instant::now();
        let sources = chain
            .iter()
            .map(|p| FileSource::open(p).map_err(err))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let o = overview(&wb, sources, 0, level).map_err(err)?;
        let ms = t.elapsed().as_secs_f64() * 1e3;
        table.row(vec![
            Cell::count(u64::from(level)),
            Cell::count(1 << (10 + 4 * u32::from(level))),
            Cell::count(o.nodes.len() as u64),
            Cell::num(ms, 1),
            Cell::count(o.bytes_read),
            Cell::count(o.reads),
            Cell::count(chain_bytes),
        ]);
    }
    println!("{}", bench.push(table));
    if let Some(dir) = &args.dir {
        std::fs::remove_dir_all(dir)?;
    }
    bench.finish()
}

/// Record `wb` into `muxes` chains of piece files under `dir`: what the run took, and each chain's pieces.
fn to_files(dir: &Path, wb: &Wideband, muxes: usize, source: &Source) -> anyhow::Result<Run> {
    anyhow::ensure!(
        !dir.exists() || std::fs::read_dir(dir)?.next().is_none(),
        "{} is not empty",
        dir.display()
    );
    let dirs: Vec<PathBuf> = (0..muxes).map(|c| dir.join(format!("chain-{c}"))).collect();
    let start = Instant::now();
    let mut m = Vec::new();
    for (chain, d) in (0..).zip(&dirs) {
        std::fs::create_dir_all(d)?;
        m.push(
            ww_tape_fs::mux::create(
                d,
                OWNER,
                chain,
                &[],
                wb.chain_options(),
                MuxOptions::default(),
            )
            .map_err(err)?,
        );
    }
    let mut r = run(wb, source, m, start)?;
    for d in &dirs {
        let mut pieces: Vec<PathBuf> = std::fs::read_dir(d)?
            .map(|e| e.map(|e| e.path()))
            .collect::<Result<_, _>>()?;
        pieces.retain(|p| p.extension().is_some_and(|x| x == "wwt"));
        pieces.sort();
        r.pieces.push(pieces);
    }
    Ok(r)
}

/// The row of run `r` of `wb` into `store` through `muxes` muxes.
fn row(store: &str, wb: &Wideband, muxes: usize, r: &Run) -> Vec<Cell> {
    let secs = r.wall.as_secs_f64();
    let msps = r.samples as f64 / secs / 1e6;
    let codec = if wb.codec == Codec::RAW {
        "`RAW`"
    } else {
        "`PACKED12_LSB`"
    };
    vec![
        Cell::text(store),
        Cell::count(u64::from(wb.channels)),
        Cell::count(muxes as u64),
        Cell::text(codec),
        Cell::num(secs * 1e3, 0),
        Cell::num(msps, 0),
        Cell::num(msps * 1e6 / f64::from(RATE_HZ), 2),
        Cell::num(r.written as f64 / secs / 1e6, 0),
        Cell::count(r.waits),
    ]
}
