//! The RF capture's writer and reader on one core: a minute of IQ (`ww_tape_example::rf`) at `--msps`, chunk by
//! chunk, the samples made before each chunk and not timed. Prints, per codec, what the writer spends on a chunk:
//! the codec, `TapeWriter::write_chunk` into a sink that keeps no bytes (CRCs, the content hash, the index), and the
//! summaries; the rate they sustain together in MS/s and MB/s of 4-byte samples. Then the reader on `--read-seconds`
//! of the capture in memory: every chunk the index lists read with its CRCs checked and decoded, the best of
//! `--rounds`. Each table carries the load average before and after it.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::print_stdout,
    reason = "a bench reads no file of others: it computes sizes and rates freely and prints its table"
)]

use clap::Parser;
use std::hint::black_box;
use std::time::{Duration, Instant};
use ww_tape::{Codec, SyncPolicy, TapeWriter};
use ww_tape_bench::{Bench, Cell, Common};
use ww_tape_example::rf::{self, Rf, Tally};

#[derive(Parser)]
#[command(
    name = "rf_core",
    about = "The RF capture: what ww_tape's writer and reader sustain on IQ samples, on one core",
    version = vc_build::version!(),
    after_long_help = "Examples:
  just bench-rf-core
  rf_core --seconds 10 --json rf_core.json"
)]
struct Args {
    #[command(flatten)]
    common: Common,
    /// Complex samples a second, millions.
    #[arg(long, default_value_t = 20)]
    msps: u32,
    /// Seconds of the capture the writer is timed on.
    #[arg(long, default_value_t = 60)]
    seconds: u32,
    /// Seconds of the capture the reader is timed on, in memory.
    #[arg(long, default_value_t = 10)]
    read_seconds: u32,
    /// Rounds of the reader; the best is kept.
    #[arg(long, default_value_t = 3)]
    rounds: u32,
}

/// Time spent on each part of the writer over a capture.
#[derive(Default)]
struct Parts {
    codec: Duration,
    write: Duration,
    summaries: Duration,
    close: Duration,
}

impl Parts {
    fn total(&self) -> Duration {
        self.codec + self.write + self.summaries + self.close
    }
}

fn write(rf: &Rf) -> anyhow::Result<(Parts, u64)> {
    let e = |e| anyhow::anyhow!("{e}");
    let (defs, iq) = rf.definitions::<std::io::Error>().map_err(e)?;
    let mut writer = TapeWriter::create_on(Tally::default(), SyncPolicy::None);
    let mut summaries = rf.summary_builder::<std::io::Error>(iq).map_err(e)?;
    let signal = rf.signal();
    let (mut values, mut bytes) = (Vec::new(), Vec::new());
    let mut parts = Parts::default();
    for k in 0..rf.chunks() {
        let (first, count) = rf.chunk_range(k);
        values.resize(count as usize * 2, 0);
        signal.fill(first, &mut values);
        let t = Instant::now();
        let (head, spec) = rf
            .encode_chunk::<std::io::Error>(iq, first, &values, &mut bytes)
            .map_err(e)?;
        parts.codec += t.elapsed();
        let t = Instant::now();
        writer
            .write_chunk(&defs, &head, &[(spec, &bytes)])
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        parts.write += t.elapsed();
        if let Some(s) = &mut summaries {
            let t = Instant::now();
            s.push(&values, 0).map_err(|e| anyhow::anyhow!("{e:?}"))?;
            for block in s.take_blocks() {
                writer
                    .defer_block(&defs, block)
                    .map_err(|e| anyhow::anyhow!("{e:?}"))?;
            }
            parts.summaries += t.elapsed();
        }
    }
    let t = Instant::now();
    if let Some(s) = &mut summaries {
        s.finish().map_err(|e| anyhow::anyhow!("{e:?}"))?;
        for block in s.take_blocks() {
            writer
                .defer_block(&defs, block)
                .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        }
    }
    let len = writer.close().map_err(|e| anyhow::anyhow!("{e:?}"))?.len;
    parts.close = t.elapsed();
    Ok((parts, len))
}

fn main() -> anyhow::Result<()> {
    let args: Args = ww_tape_bench::args();
    let base = Rf::at(args.msps * 1_000_000).seconds(args.seconds);
    let samples = base.count() as f64;
    let mut bench = Bench::start(
        "rf_core",
        vc_build::build_info!(),
        &args.common,
        vec![
            ("MS/s", args.msps.to_string()),
            ("seconds", args.seconds.to_string()),
            ("chunk samples", base.chunk_samples.to_string()),
            ("read seconds", args.read_seconds.to_string()),
            ("rounds", args.rounds.to_string()),
        ],
    )?;
    println!("{}", bench.header());
    let mut table = bench.table(
        "writer",
        &format!(
            "The writer on {} s of IQ at {} MS/s in {} chunks, one core: ms of work per second of capture.",
            args.seconds,
            args.msps,
            base.chunks()
        ),
        &[
            "Codec",
            "Bytes per sample",
            "Codec ms/s",
            "`write_chunk` ms/s",
            "Summaries ms/s",
            "Writer MS/s",
            "Writer MB/s",
        ],
    );
    let codecs = [
        ("`RAW`", Codec::RAW),
        ("`PACKED12_LSB`", Codec::PACKED12_LSB),
        ("`DELTA`", Codec::DELTA),
    ];
    // ms of work per second of capture
    let per_s = |d: Duration| d.as_secs_f64() * 1e3 / f64::from(args.seconds);
    for (name, codec) in codecs {
        let (parts, len) = write(&base.codec(codec))?;
        let msps = samples / parts.total().as_secs_f64() / 1e6;
        table.row(vec![
            Cell::text(name),
            Cell::num(len as f64 / samples, 3),
            Cell::num(per_s(parts.codec), 1),
            Cell::num(per_s(parts.write), 1),
            Cell::num(per_s(parts.summaries), 1),
            Cell::num(msps, 0),
            Cell::num(msps * 4.0, 0),
        ]);
    }
    println!("{}", bench.push(table));
    let mut table = bench.table(
        "reader",
        &format!(
            "The reader on {} s in memory, every chunk read with its CRCs checked and decoded, best of {}.",
            args.read_seconds, args.rounds
        ),
        &["Codec", "Reader MS/s", "Reader MB/s"],
    );
    for (name, codec) in codecs {
        let rf = base.codec(codec).seconds(args.read_seconds);
        let piece = rf.record().map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut best = f64::INFINITY;
        for _ in 0..args.rounds {
            let t = Instant::now();
            let read = rf::read(black_box(&piece.bytes)).map_err(|e| anyhow::anyhow!("{e}"))?;
            best = best.min(t.elapsed().as_secs_f64());
            anyhow::ensure!(read.chunks == rf.chunks(), "every chunk read");
            black_box(read);
        }
        let msps = rf.count() as f64 / best / 1e6;
        table.row(vec![
            Cell::text(name),
            Cell::num(msps, 0),
            Cell::num(msps * 4.0, 0),
        ]);
    }
    println!("{}", bench.push(table));
    bench.finish()
}
