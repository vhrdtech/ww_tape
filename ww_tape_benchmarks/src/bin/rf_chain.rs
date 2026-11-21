//! The RF capture (`ww_tape_example::rf`) recorded at full speed into a chain of piece files, then read back.
//!
//! Worker threads make each chunk's samples and code them, as a recorder's receive threads do; one thread writes
//! the chunks and the summaries into the chain, a piece a minute of capture, until the chain holds `--gib` GiB.
//! Prints what the run sustained: samples a second, MB/s of 4-byte samples and MB/s written, and how busy the
//! writer's thread was. Then a reader opens every piece and reads one second of IQ, counting the bytes and reads.
//! The chain stays in `--dir` unless `--remove`. The load average at the start and at the end are in the output.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::print_stdout,
    reason = "a bench reads no file of others: it computes sizes and rates freely and prints its table"
)]

use anyhow::Context;
use clap::Parser;
use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::{Duration, Instant};
use ww_tape::chunk::ChunkHead;
use ww_tape::reader::ByteSource;
use ww_tape::samples::Samples;
use ww_tape::{
    BlockKind, ChainOptions, ColumnKind, ColumnSpec, CutPolicy, OwnerId, PieceTapes, TapeReader,
    WriterOptions,
};
use ww_tape_example::rf::{self, Rf, iq_key};
use ww_tape_fs::{FileSource, FsError};

#[derive(Parser)]
#[command(
    about = "The RF capture into a chain of piece files at full speed, then opened and queried"
)]
struct Args {
    /// The directory of the chain; it must not hold one.
    #[arg(long)]
    dir: PathBuf,
    /// GiB the chain grows to.
    #[arg(long, default_value_t = 100)]
    gib: u64,
    /// Complex samples a second, millions.
    #[arg(long, default_value_t = 20)]
    msps: u32,
    /// Threads that make and code chunks.
    #[arg(long, default_value_t = 8)]
    workers: usize,
    /// The second of the capture the query reads; by default the one in the middle.
    #[arg(long)]
    second: Option<u64>,
    /// Remove the chain after reading it.
    #[arg(long)]
    remove: bool,
    /// Read the chain already in `--dir` and write none.
    #[arg(long)]
    read_only: bool,
}

/// A chunk made by a worker: its head, its coded column and its samples for the summaries.
struct Made {
    head: ChunkHead,
    spec: ColumnSpec,
    bytes: Vec<u8>,
    values: Vec<i16>,
}

fn load() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .map(|l| l.split_whitespace().take(3).collect::<Vec<_>>().join(" "))
        .unwrap_or_else(|_| "unknown".to_owned())
}

/// Worker `w` of `n`: chunks `w`, `w + n`, ... of `rf` until `stop` chunks, each into `tx`.
fn worker(rf: Rf, w: u64, n: u64, chunks: u64, tx: &SyncSender<anyhow::Result<Made>>) {
    let signal = rf.signal();
    let mut k = w;
    while k < chunks {
        let (first, count) = rf.chunk_range(k);
        let mut values = vec![0i16; count as usize * 2];
        signal.fill(first, &mut values);
        let mut bytes = Vec::new();
        let made = rf
            .encode_chunk::<FsError>(first, &values, &mut bytes)
            .map(|(head, spec)| Made {
                head,
                spec,
                bytes,
                values,
            })
            .map_err(|e| anyhow::anyhow!("{e}"));
        if tx.send(made).is_err() {
            return;
        }
        k += n;
    }
}

/// What writing the chain took.
struct Written {
    wall: Duration,
    busy: Duration,
    samples: u64,
    bytes: u64,
    pieces: Vec<u64>,
}

fn write(args: &Args, rf: Rf) -> anyhow::Result<Written> {
    let e = |e: ww_tape::WriteError<FsError>| anyhow::anyhow!("{e:?}");
    let defs = rf
        .definitions::<FsError>()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let options = ChainOptions {
        writer: WriterOptions {
            producer: "rf_chain bench".to_owned(),
            ..WriterOptions::default()
        },
        cut: CutPolicy {
            bytes: 0,
            chunks: 6_000, // a minute of 10 ms chunks
            interval: None,
        },
        tapes: PieceTapes::Each,
    };
    std::fs::create_dir_all(&args.dir)?;
    let mut chain =
        ww_tape_fs::chain::create(&args.dir, OwnerId([0x5d; 32]), 0, &[], options).map_err(e)?;
    let mut summaries = rf
        .summary_builder::<FsError>()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let target = args.gib << 30;
    // more chunks than the target needs: the writer stops at the target
    let chunks = rf.chunks();
    let n = args.workers.max(1) as u64;
    let (txs, rxs): (Vec<_>, Vec<Receiver<_>>) = (0..n).map(|_| sync_channel(4)).unzip();
    let start = Instant::now();
    let mut busy = Duration::ZERO;
    let mut samples = 0u64;
    std::thread::scope(|s| -> anyhow::Result<()> {
        for (w, tx) in txs.into_iter().enumerate() {
            s.spawn(move || worker(rf, w as u64, n, chunks, &tx));
        }
        for k in 0..chunks {
            let made = rxs
                .get((k % n) as usize)
                .context("a worker's channel")?
                .recv()
                .context("a worker stopped")??;
            let t = Instant::now();
            if chain.cut_due() {
                chain.cut_now(&defs).map_err(e)?;
            }
            chain
                .write_chunk(&defs, &made.head, &[(made.spec, &made.bytes)])
                .map_err(e)?;
            if let Some(sum) = &mut summaries {
                sum.push(&made.values, 0)
                    .map_err(|e| anyhow::anyhow!("{e:?}"))?;
                for block in sum.take_blocks() {
                    chain.defer_block(&defs, block).map_err(e)?;
                }
            }
            busy += t.elapsed();
            samples += u64::from(made.head.count);
            let closed: u64 = chain.closed().iter().map(|p| p.piece.len).sum();
            if closed + chain.writer().map_err(e)?.bytes() >= target {
                break;
            }
        }
        // the workers stop when their channel closes
        drop(rxs);
        Ok(())
    })?;
    let t = Instant::now();
    if let Some(sum) = &mut summaries {
        sum.finish().map_err(|e| anyhow::anyhow!("{e:?}"))?;
        for block in sum.take_blocks() {
            chain.defer_block(&defs, block).map_err(e)?;
        }
    }
    let closed = chain.close().map_err(e)?;
    busy += t.elapsed();
    let wall = start.elapsed();
    let pieces: Vec<u64> = closed.iter().map(|p| p.piece.len).collect();
    Ok(Written {
        wall,
        busy,
        samples,
        bytes: pieces.iter().sum(),
        pieces,
    })
}

/// A file source that counts the bytes and the reads asked of it.
struct Counted<'a> {
    file: FileSource,
    bytes: &'a Cell<u64>,
    reads: &'a Cell<u64>,
}

impl ByteSource for Counted<'_> {
    type Error = FsError;

    fn size(&self) -> Result<u64, FsError> {
        self.file.size()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
        self.bytes.set(self.bytes.get() + buf.len() as u64);
        self.reads.set(self.reads.get() + 1);
        self.file.read_at(offset, buf)
    }
}

fn pieces_in(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)?
        .map(|e| e.map(|e| e.path()))
        .collect::<Result<_, _>>()?;
    paths.retain(|p| p.extension().is_some_and(|x| x == "wwt"));
    paths.sort();
    Ok(paths)
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if args.read_only {
        return read(&args, None);
    }
    anyhow::ensure!(
        !args.dir.exists() || std::fs::read_dir(&args.dir)?.next().is_none(),
        "{} is not empty",
        args.dir.display()
    );
    // a capture longer than the target needs: an hour per 100 GiB at 20 MS/s is ample
    let hours = (args.gib.div_ceil(100) * 2) as u32;
    let rf = Rf::at(args.msps * 1_000_000).seconds(3_600 * hours);
    let load_start = load();
    let w = write(&args, rf)?;
    let load_end = load();
    let secs = w.wall.as_secs_f64();
    let msps = w.samples as f64 / secs / 1e6;
    let (min, max) = (
        w.pieces.iter().min().copied().unwrap_or(0),
        w.pieces.iter().max().copied().unwrap_or(0),
    );
    println!(
        "IQ at {} MS/s into a chain in {}, {} workers; load average {load_start} at the start, {load_end} at the end.\n",
        args.msps,
        args.dir.display(),
        args.workers
    );
    println!("| What | Value |");
    println!("|---|---:|");
    println!(
        "| Capture recorded | {:.1} s |",
        w.samples as f64 / (f64::from(args.msps) * 1e6)
    );
    println!("| Wall time | {secs:.1} s |");
    println!(
        "| Sustained | {msps:.1} MS/s, {:.0} MB/s of samples |",
        msps * 4.0
    );
    println!(
        "| Written | {:.2} GiB, {:.0} MB/s |",
        w.bytes as f64 / (1u64 << 30) as f64,
        w.bytes as f64 / secs / 1e6
    );
    println!(
        "| Writer thread busy | {:.0} % |",
        100.0 * w.busy.as_secs_f64() / secs
    );
    println!(
        "| Pieces | {} of {:.2} to {:.2} GB |",
        w.pieces.len(),
        min as f64 / 1e9,
        max as f64 / 1e9
    );
    println!(
        "| Bytes per sample | {:.3} |",
        w.bytes as f64 / w.samples as f64
    );
    read(&args, Some(w.samples))
}

/// Open every piece in `--dir`, then read and decode one second of IQ: `--second`, or the middle of `samples`.
fn read(args: &Args, samples: Option<u64>) -> anyhow::Result<()> {
    println!("\nLoad average before reading: {}.", load());
    // open every piece
    let (bytes, reads) = (Cell::new(0), Cell::new(0));
    let t = Instant::now();
    let mut readers = Vec::new();
    for path in pieces_in(&args.dir)? {
        let file = FileSource::open(&path).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let r = TapeReader::open(Counted {
            file,
            bytes: &bytes,
            reads: &reads,
        })
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        readers.push(r);
    }
    let open = (t.elapsed(), bytes.replace(0), reads.replace(0));

    // one second of IQ
    let second = args
        .second
        .or(samples.map(|n| n / u64::from(args.msps) / 2_000_000))
        .context("--second, for a chain read alone")?;
    let (from, to) = (
        second.cast_signed() * 1_000_000_000,
        (second.cast_signed() + 1) * 1_000_000_000,
    );
    let t = Instant::now();
    let mut got = 0u64;
    let mut buf = Vec::new();
    let mut out = Vec::new();
    for r in &readers {
        let holds = r
            .pieces()
            .first()
            .and_then(|p| p.close.as_ref())
            .is_some_and(|(_, tracks)| {
                tracks
                    .iter()
                    .any(|t| t.track_id == iq_key() && t.first_tick < to && from <= t.last_tick)
            });
        if !holds {
            continue;
        }
        for entry in r
            .track_entries(0, iq_key())
            .map_err(|e| anyhow::anyhow!("{e:?}"))?
        {
            if entry.kind != BlockKind::Series || entry.first_tick >= to || entry.last_tick < from {
                continue;
            }
            let block = r
                .read_block(0, &entry, &mut buf)
                .map_err(|e| anyhow::anyhow!("{e:?}"))?;
            let chunk = block.chunk().ok_or_else(|| anyhow::anyhow!("a chunk"))?;
            let count = chunk.head().count as usize;
            let column = chunk
                .find(ColumnKind::Samples, 0)
                .ok_or_else(|| anyhow::anyhow!("samples"))?;
            let s = Samples::of(&column, rf::iq_shape(), count)
                .map_err(|e| anyhow::anyhow!("{e:?}"))?;
            out.resize(count * 2, 0i16);
            s.decode(0, &mut out)
                .map_err(|e| anyhow::anyhow!("{e:?}"))?;
            got += count as u64;
        }
    }
    let query = (t.elapsed(), bytes.replace(0), reads.replace(0));
    println!("\n| Read | Time | Bytes read | Reads |");
    println!("|---|---:|---:|---:|");
    println!(
        "| Open {} pieces | {:.1} ms | {} | {} |",
        readers.len(),
        open.0.as_secs_f64() * 1e3,
        open.1,
        open.2
    );
    println!(
        "| Second {second}: {got} samples decoded | {:.1} ms | {} | {} |",
        query.0.as_secs_f64() * 1e3,
        query.1,
        query.2
    );
    if args.remove {
        drop(readers);
        std::fs::remove_dir_all(&args.dir)?;
    }
    Ok(())
}
