//! The RF capture's writer and reader on one core: a minute of IQ (`ww_tape_example::rf`) at `--msps`, chunk by
//! chunk, the samples made before each chunk and not timed. Prints, per codec, what the writer spends on a chunk:
//! the codec, `TapeWriter::write_chunk` into a sink that keeps no bytes (CRCs, the content hash, the index), and the
//! summaries; the rate they sustain together in MS/s and MB/s of 4-byte samples. Then the reader on `--read-seconds`
//! of the capture in memory: every chunk the index lists read with its CRCs checked and decoded, the best of
//! `--rounds`. The load average at the start is in the heading.
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
use ww_tape_example::rf::{self, Rf, Tally};

#[derive(Parser)]
#[command(
    about = "The RF capture: what ww_tape's writer and reader sustain on IQ samples, on one core"
)]
struct Args {
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
    let defs = rf.definitions::<std::io::Error>().map_err(e)?;
    let mut writer = TapeWriter::create_on(Tally::default(), SyncPolicy::None);
    let mut summaries = rf.summary_builder::<std::io::Error>().map_err(e)?;
    let signal = rf.signal();
    let (mut values, mut bytes) = (Vec::new(), Vec::new());
    let mut parts = Parts::default();
    for k in 0..rf.chunks() {
        let (first, count) = rf.chunk_range(k);
        values.resize(count as usize * 2, 0);
        signal.fill(first, &mut values);
        let t = Instant::now();
        let (head, spec) = rf
            .encode_chunk::<std::io::Error>(first, &values, &mut bytes)
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
    let args = Args::parse();
    let load = std::fs::read_to_string("/proc/loadavg")
        .map(|l| l.split_whitespace().take(3).collect::<Vec<_>>().join(" "))
        .unwrap_or_else(|_| "unknown".to_owned());
    let base = Rf::at(args.msps * 1_000_000).seconds(args.seconds);
    let samples = base.count() as f64;
    println!(
        "IQ at {} MS/s, {} s ({} chunks of {} samples), one core; load average {load}.\n",
        args.msps,
        args.seconds,
        base.chunks(),
        base.chunk_samples
    );
    println!(
        "| Codec | Bytes per sample | Codec, ms/s | `write_chunk`, ms/s | Summaries, ms/s | Writer, MS/s | Writer, MB/s |"
    );
    println!("|---|---:|---:|---:|---:|---:|---:|");
    let codecs = [
        ("`PACKED12_LSB`", Codec::PACKED12_LSB),
        ("`DELTA`", Codec::DELTA),
    ];
    // ms of work per second of capture
    let per_s = |d: Duration| d.as_secs_f64() * 1e3 / f64::from(args.seconds);
    for (name, codec) in codecs {
        let (parts, len) = write(&base.codec(codec))?;
        let msps = samples / parts.total().as_secs_f64() / 1e6;
        println!(
            "| {name} | {:.3} | {:.1} | {:.1} | {:.1} | {msps:.0} | {:.0} |",
            len as f64 / samples,
            per_s(parts.codec),
            per_s(parts.write),
            per_s(parts.summaries),
            msps * 4.0
        );
    }

    println!(
        "\nThe reader on {} s in memory, every chunk read and decoded, best of {}.\n",
        args.read_seconds, args.rounds
    );
    println!("| Codec | Reader, MS/s | Reader, MB/s |");
    println!("|---|---:|---:|");
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
        println!("| {name} | {msps:.0} | {:.0} |", msps * 4.0);
    }
    Ok(())
}
