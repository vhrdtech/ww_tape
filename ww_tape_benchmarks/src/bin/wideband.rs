//! The wideband receiver of `example_wideband` at full speed: 1 GS/s of IQ in channels, each a producer
//! thread, over one or several muxes, into a store that keeps no bytes and into chains of piece files.
//!
//! Each channel's thread pushes chunks made before the run (`--replay-chunks` of them, again and again), so the run
//! times the recorder, not the signal's maths. With `--dir` the most channels and muxes also write chains of piece
//! files there, and a reader then draws one channel's overview from its summaries. Each table carries the load
//! average before and after it.
//!
//! The tables `window` and `triage` capture a 250 MS/s receiver into a window in memory (`example_wideband::ram`) while the
//! triage detects energy in each piece's summary nodes and keeps the pieces around a burst. One run replays its
//! chunks, for speed; one makes the signal as it goes for `--live-ms`, bursts and all, and keeps them in piece files
//! in `--dir`.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::print_stdout,
    reason = "a bench reads no file of others: it computes sizes and rates freely and prints its table"
)]

use clap::Parser;
use example_capture::chain_options;
use example_wideband::ram::RfCapture;
use example_wideband::{RATE_HZ, Source, Wideband, overview};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use ww_tape::{Codec, MemTapes, OwnerId, Tapes};
use ww_tape_bench::{Bench, Cell, Common, Discard, cpu_time};
use ww_tape_benchmarks::{TRIAGE_COLUMNS, WINDOW_COLUMNS, WindowRun, triage_row, window_row};
use ww_tape_fs::{FileSource, Files};
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
    /// Milliseconds of the 250 MS/s capture into memory made live, its bursts included.
    #[arg(long, default_value_t = 60_000)]
    live_ms: u64,
}

/// An error of the library, kept as its debug text.
fn err<E: core::fmt::Debug>(e: E) -> anyhow::Error {
    anyhow::anyhow!("{e:?}")
}

const OWNER: OwnerId = OwnerId([0x5d; 32]);
/// The owner of the analyser's results.
const RESULTS_OWNER: OwnerId = OwnerId([0x5e; 32]);

/// What one run took.
struct Run {
    samples: u64,
    wall: Duration,
    /// CPU time of every thread of the process, None where the system doesn't say.
    cpu: Option<Duration>,
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
    let cpu_start = cpu_time();
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
        cpu: cpu_time().zip(cpu_start).map(|(end, start)| end - start),
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
            ("live ms", args.live_ms.to_string()),
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
            "Bytes per sample",
            "CPU ns per sample",
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
    match files {
        Some((wb, r)) => overview_table(&args, &mut bench, wb, &r)?,
        None => println!("{}", bench.skipped("overview", "no --dir")),
    }
    if let Some(dir) = &args.dir {
        std::fs::remove_dir_all(dir)?;
    }
    windows(&args, &mut bench)?;
    bench.finish()
}

/// The table `overview`: channel 0's overview read from the chain of piece files of run `r` of `wb`.
fn overview_table(args: &Args, bench: &mut Bench, wb: Wideband, r: &Run) -> anyhow::Result<()> {
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
    Ok(())
}

/// The tables `window` and `triage`: 250 MS/s into a window in memory, replayed into memory, then made live for
/// `--live-ms`, its kept pieces in piece files in `--dir`.
fn windows(args: &Args, bench: &mut Bench) -> anyhow::Result<()> {
    let full = RfCapture::full();
    let title = |what: &str| {
        format!(
            "{} MS/s of IQ in {} channels, `PACKED12_LSB`, into a window in memory, pieces of {} MiB; the triage \
             keeps the piece before and after each burst. {what}",
            full.wb.rate_hz / 1_000_000,
            full.wb.channels,
            full.wb.piece_bytes >> 20
        )
    };
    let columns = ["Kept to", "Source", "Signal ms"];
    let mut window = bench.table(
        "window",
        &title(
            "The samples come as fast as the threads take them; a window takes half the machine's memory.",
        ),
        &[columns.as_slice(), &WINDOW_COLUMNS].concat(),
    );
    let mut triage = bench.table(
        "triage",
        &title("The analyser reads the level-0 summary nodes of each piece on one thread."),
        &[columns.as_slice(), &TRIAGE_COLUMNS].concat(),
    );
    let memory = bench.report().machine.memory_bytes;
    let mut rows = |kept: &str, source: &str, ms: u64, r: &WindowRun| {
        let head = [Cell::text(kept), Cell::text(source), Cell::count(ms)];
        window.row([head.to_vec(), window_row(r, memory)].concat());
        triage.row([head.to_vec(), triage_row(r)].concat());
    };
    let replayed = RfCapture {
        wb: full.wb.ms(args.ms),
        ..full
    };
    let source = replayed.wb.replay(args.replay_chunks);
    let r = window_run(&replayed, source, MemTapes::new(), mem_results()?)?;
    rows("memory", "replayed", args.ms, &r);
    let live = RfCapture {
        wb: full.wb.ms(args.live_ms),
        ..full
    };
    match &args.dir {
        Some(dir) => {
            anyhow::ensure!(
                !dir.exists() || std::fs::read_dir(dir)?.next().is_none(),
                "{} is not empty",
                dir.display()
            );
            let (kept_dir, results_dir) = (dir.join("kept"), dir.join("results"));
            std::fs::create_dir_all(&kept_dir)?;
            std::fs::create_dir_all(&results_dir)?;
            let results = ww_tape_fs::mux::create(
                &results_dir,
                RESULTS_OWNER,
                0,
                &[],
                chain_options(),
                MuxOptions::default(),
            )
            .map_err(err)?;
            let r = window_run(&live, Source::Live, Files::new(&kept_dir), results)?;
            rows("piece files", "live", args.live_ms, &r);
            std::fs::remove_dir_all(dir)?;
        }
        None => {
            let r = window_run(&live, Source::Live, MemTapes::new(), mem_results()?)?;
            rows("memory", "live", args.live_ms, &r);
        }
    }
    println!("{}", bench.push(window));
    println!("{}", bench.push(triage));
    Ok(())
}

/// A mux of the analyser's results in memory.
fn mem_results() -> anyhow::Result<TapeMux<MemTapes>> {
    TapeMux::create(
        MemTapes::new(),
        RESULTS_OWNER,
        0,
        &[],
        chain_options(),
        MuxOptions::default(),
    )
    .map_err(err)
}

/// Run capture `c` from `source`, kept pieces into `kept`, results through `results`, closed after.
fn window_run<K, R>(
    c: &RfCapture,
    source: Source,
    kept: K,
    results: TapeMux<R>,
) -> anyhow::Result<WindowRun>
where
    K: Tapes + Send + 'static,
    K::Error: Send + core::fmt::Debug,
    R: Tapes + Send + 'static,
    R::Sink: Send,
    R::Key: Send,
    R::Error: Send + Sync + core::fmt::Debug,
{
    let start = Instant::now();
    let run = c.run(source, kept, &results).map_err(err)?;
    let wall = start.elapsed();
    let closed = results.close().map_err(err)?;
    Ok(WindowRun {
        signal_s: c.wb.ms as f64 / 1e3,
        capture: run.capture_time,
        wall,
        triaged: vec![run.triaged],
        results_bytes: closed.iter().map(|p| p.piece.len).sum(),
    })
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
        Cell::num(r.written as f64 / r.samples as f64, 2),
        r.cpu.map_or_else(
            || Cell::text("unknown"),
            |cpu| Cell::num(cpu.as_secs_f64() * 1e9 / r.samples as f64, 1),
        ),
        Cell::count(r.waits),
    ]
}
