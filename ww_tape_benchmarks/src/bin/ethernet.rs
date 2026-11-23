//! The protocol analyser of `example_ethernet` at full speed: both directions of a 10 Gbps link through
//! one `TapeMux`, into a store that keeps no bytes and into a chain of piece files.
//!
//! Per port a capture thread makes the frames and pushes them, and a decoder thread pushes their IPv4 and TCP or UDP
//! headers: four producer threads and the mux's writing thread. The frames come as fast as the threads take them,
//! not at the link's pace, so a run says how fast the analyser could record against the link it watches. Each table
//! carries the load average before and after it.
//!
//! The table `payload` records the link with random payloads and with a synthetic plaintext mix, each with the `frames`
//! payloads coded raw and tried with LZ4: the bytes kept and the CPU time a frame takes.
//!
//! The tables `window` and `triage` capture the link into a window in memory (`example_ethernet::ram`) while the triage reads
//! each piece and keeps those around a TCP reset: in one chain, whose mux writes both ports, and in a chain per port,
//! each with a mux and a triage thread of its own; into memory, then into piece files in `--dir`. The largest capture
//! into memory runs again for each size of the triage pool in `--triage-threads` and for the machine's cores.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::print_stdout,
    reason = "a bench reads no file of others: it computes sizes and rates freely and prints its table"
)]

use clap::Parser;
use example_capture::chain_options;
use example_ethernet::ram::RamCapture;
use example_ethernet::{Captured, LINE_BPS, Link, PayloadMix, RESET_EVERY};
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use ww_tape::payloads::PayloadCodec;
use ww_tape::{ChainOptions, CutPolicy, MemTapes, OwnerId, Tapes};
use ww_tape_bench::{Bench, Cell, Common, Discard, cpu_time};
use ww_tape_benchmarks::{TRIAGE_COLUMNS, WINDOW_COLUMNS, WindowRun, triage_row, window_row};
use ww_tape_fs::Files;
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
    /// The receive queues of a port of the capture into memory, a run per number.
    #[arg(long, value_delimiter = ',', default_values_t = [2, 4])]
    window_queues: Vec<u32>,
    /// Chains in memory of the capture, 1 or 2, a run per number: one mux for both ports, or a mux per port.
    #[arg(long, value_delimiter = ',', default_values_t = [1, 2])]
    window_chains: Vec<u32>,
    /// MiB of a piece in memory.
    #[arg(long, default_value_t = 64)]
    window_piece_mib: u64,
    /// Threads of the triage pool: the largest capture into memory runs once for each, and once for the machine's
    /// cores. The other runs take the default, 2 threads a chain.
    #[arg(long, value_delimiter = ',', default_values_t = [1, 2, 4])]
    triage_threads: Vec<u32>,
}

/// An error of the library, kept as its debug text.
fn err<E: core::fmt::Debug>(e: E) -> anyhow::Error {
    anyhow::anyhow!("{e:?}")
}

const OWNER: OwnerId = OwnerId([0xe7; 32]);
/// The owner of the analyser's results.
const RESULTS_OWNER: OwnerId = OwnerId([0xe8; 32]);

/// What one run took.
struct Run {
    captured: Captured,
    wall: Duration,
    /// CPU time of every thread of the process, None where the system doesn't say.
    cpu: Option<Duration>,
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
    let cpu_start = cpu_time();
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
    let wall = start.elapsed();
    Ok(Run {
        captured,
        wall,
        cpu: cpu_time().zip(cpu_start).map(|(end, start)| end - start),
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
            (
                "triage threads",
                args.triage_threads
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
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
    payloads(&args, &mut bench)?;
    windows(&args, &mut bench)?;
    bench.finish()
}

/// The table `payload`: the link with random and with plaintext payloads, the `frames` payloads coded raw and tried
/// with LZ4, into a store that keeps no bytes; what the chain takes and the CPU time a frame costs.
fn payloads(args: &Args, bench: &mut Bench) -> anyhow::Result<()> {
    let queues = 2;
    let mut table = bench.table(
        "payload",
        &format!(
            "{} ms of both directions of a 10 Gbps link at {} % load, {queues} receive queues a port, one chain, no \
             bytes kept. The `frames` payloads are random bytes or a synthetic mix of plaintext protocols, coded raw \
             or tried with LZ4 (`Auto`). CPU is that of every thread of the process, capture, decoders and the \
             mux's writing thread, per frame.",
            args.ms, args.load
        ),
        &[
            "Payloads",
            "Codec",
            "Frames",
            "Wall ms",
            "M frames/s",
            "Chain MB",
            "Bytes per frame",
            "CPU ns per frame",
        ],
    );
    for mix in [PayloadMix::Random, PayloadMix::Plain] {
        for (codec, name) in [(PayloadCodec::Raw, "Raw"), (PayloadCodec::Auto, "Auto")] {
            let link = Link::full()
                .micros(args.ms * 1_000)
                .load(args.load)
                .queues(queues)
                .payload(mix)
                .payload_codec(codec);
            let start = Instant::now();
            let mux = TapeMux::create(
                Discard::default(),
                OWNER,
                0,
                &[],
                options(args.piece_mib),
                MuxOptions::default(),
            )
            .map_err(err)?;
            let r = run(link, vec![mux], start)?;
            let [a, b] = &r.captured.ports;
            let frames = a.frames + b.frames;
            table.row(vec![
                Cell::text(mix.name()),
                Cell::text(name),
                Cell::count(frames),
                Cell::num(r.wall.as_secs_f64() * 1e3, 0),
                Cell::num(frames as f64 / r.wall.as_secs_f64() / 1e6, 2),
                Cell::num(r.written as f64 / 1e6, 0),
                Cell::num(r.written as f64 / frames as f64, 1),
                r.cpu.map_or_else(
                    || Cell::text("unknown"),
                    |cpu| Cell::num(cpu.as_secs_f64() * 1e9 / frames as f64, 0),
                ),
            ]);
        }
    }
    println!("{}", bench.push(table));
    Ok(())
}

/// The tables `window` and `triage`: the link into a window in memory, in one chain and in a chain per port, the
/// pieces around each reset kept in memory, then in piece files in `--dir` with the most chains and queues.
fn windows(args: &Args, bench: &mut Bench) -> anyhow::Result<()> {
    let title = |what: &str| {
        format!(
            "{} ms of both directions of a 10 Gbps link at {} % load into a window in memory, each chain written by \
             a mux and read by a triage thread of its own, pieces of {} MiB, a TCP reset in one frame of {} a port; \
             the triage keeps the piece before and after each. {what}",
            args.ms, args.load, args.window_piece_mib, RESET_EVERY
        )
    };
    let columns = ["Kept to", "Chains", "Queues a port", "Triage threads"];
    let mut window = bench.table(
        "window",
        &title(
            "The frames come as fast as the threads take them; a window takes half the machine's memory.",
        ),
        &[columns.as_slice(), &WINDOW_COLUMNS].concat(),
    );
    let mut triage = bench.table(
        "triage",
        &title("The analyser decodes every frame of each piece."),
        &[columns.as_slice(), &TRIAGE_COLUMNS].concat(),
    );
    let memory = bench.report().machine.memory_bytes;
    let capture = |chains: u32, queues: u32, threads: Option<NonZeroU32>| RamCapture {
        triage_threads: threads,
        link: Link::full()
            .micros(args.ms * 1_000)
            .load(args.load)
            .queues(queues),
        chains,
        piece_bytes: args.window_piece_mib << 20,
        ..RamCapture::full()
    };
    let mut rows = |kept: &str, c: &RamCapture, r: &WindowRun| {
        let head = [
            Cell::text(kept),
            Cell::count(u64::from(c.chains)),
            Cell::count(u64::from(c.link.queues)),
            if c.triage_threads.is_none() {
                Cell::text(format!("{} (default)", c.triage_threads_used()))
            } else {
                Cell::count(c.triage_threads_used() as u64)
            },
        ];
        window.row([head.to_vec(), window_row(r, memory)].concat());
        triage.row([head.to_vec(), triage_row(r)].concat());
    };
    let in_memory = |c: &RamCapture| -> anyhow::Result<WindowRun> {
        let kept: Vec<MemTapes> = (0..c.chains).map(|_| MemTapes::new()).collect();
        let results = TapeMux::create(
            MemTapes::new(),
            RESULTS_OWNER,
            0,
            &[],
            chain_options(),
            MuxOptions::default(),
        )
        .map_err(err)?;
        window_run(c, kept, results)
    };
    for &chains in &args.window_chains {
        for &queues in &args.window_queues {
            let c = capture(chains, queues, None);
            rows("memory", &c, &in_memory(&c)?);
        }
    }
    // the largest capture again with each size of the triage pool: one thread decodes the chunks one after the other
    if let (Some(&chains), Some(&queues)) = (
        args.window_chains.iter().max(),
        args.window_queues.iter().max(),
    ) {
        let cores = std::thread::available_parallelism().map_or(1, usize::from);
        let mut sweep: Vec<u32> = args.triage_threads.clone();
        sweep.extend(u32::try_from(cores));
        sweep.sort_unstable();
        sweep.dedup();
        for threads in sweep.into_iter().filter_map(NonZeroU32::new) {
            let c = capture(chains, queues, Some(threads));
            rows("memory", &c, &in_memory(&c)?);
        }
    }
    if let (Some(dir), Some(&chains), Some(&queues)) = (
        &args.dir,
        args.window_chains.iter().max(),
        args.window_queues.iter().max(),
    ) {
        anyhow::ensure!(
            !dir.exists() || std::fs::read_dir(dir)?.next().is_none(),
            "{} is not empty",
            dir.display()
        );
        let results_dir = dir.join("results");
        std::fs::create_dir_all(&results_dir)?;
        let mut kept = Vec::new();
        for chain in 0..chains {
            let d = dir.join(format!("kept-{chain}"));
            std::fs::create_dir_all(&d)?;
            kept.push(Files::new(&d));
        }
        let c = capture(chains, queues, None);
        let results = ww_tape_fs::mux::create(
            &results_dir,
            RESULTS_OWNER,
            0,
            &[],
            chain_options(),
            MuxOptions::default(),
        )
        .map_err(err)?;
        let r = window_run(&c, kept, results)?;
        rows("piece files", &c, &r);
        std::fs::remove_dir_all(dir)?;
    }
    println!("{}", bench.push(window));
    println!("{}", bench.push(triage));
    Ok(())
}

/// Run capture `c`, kept pieces into `kept`, results through `results`, closed after.
fn window_run<K, R>(c: &RamCapture, kept: Vec<K>, results: TapeMux<R>) -> anyhow::Result<WindowRun>
where
    K: Tapes + Send + 'static,
    K::Error: Send + core::fmt::Debug,
    R: Tapes + Send + 'static,
    R::Sink: Send,
    R::Key: Send,
    R::Error: Send + Sync + core::fmt::Debug,
{
    let start = Instant::now();
    let run = c.run(kept, &results).map_err(err)?;
    let wall = start.elapsed();
    let closed = results.close().map_err(err)?;
    Ok(WindowRun {
        signal_s: c.link.micros as f64 / 1e6,
        capture: run.capture_time,
        wall,
        triaged: run.triaged,
        results_bytes: closed.iter().map(|p| p.piece.len).sum(),
    })
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
