//! The RF capture (`example_capture::rf`) recorded at full speed into a chain of piece files, then read back.
//!
//! Worker threads make each chunk's samples and code them, as a recorder's receive threads do; one thread writes
//! the chunks and the summaries into the chain, a piece a minute of capture, until the chain holds `--gib` GiB.
//! Prints what the run sustained: samples a second, MB/s of 4-byte samples and MB/s written, and how busy the
//! writer's thread was. Then a reader opens every piece and reads one second of IQ, counting the bytes and reads.
//! The chain stays in `--dir` unless `--remove`. Each table carries the load average before and after it.
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
use example_capture::rf::{self, Rf, iq_key};
use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::{Duration, Instant};
use ww_tape::chunk::ChunkHead;
use ww_tape::reader::ByteSource;
use ww_tape::samples::Samples;
use ww_tape::{
    BlockKind, ChainOptions, ColumnKind, ColumnSpec, CutPolicy, OwnerId, PieceTapes, TapeReader,
    TrackNum, WriterOptions,
};
use ww_tape_bench::{Bench, Cell as TableCell, Common};
use ww_tape_fs::{FileSource, FsError};

#[derive(Parser)]
#[command(
    name = "rf_chain",
    about = "The RF capture into a chain of piece files at full speed, then opened and queried",
    version = vc_build::version!(),
    after_long_help = "Examples:
  just bench-rf-chain --dir /data/rf --gib 100 --remove
  rf_chain --dir /data/rf --read-only --second 900"
)]
struct Args {
    #[command(flatten)]
    common: Common,
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

/// Worker `w` of `n`: chunks `w`, `w + n`, ... of `rf` on track `iq` until `stop` chunks, each into `tx`.
fn worker(
    rf: Rf,
    iq: TrackNum,
    w: u64,
    n: u64,
    chunks: u64,
    tx: &SyncSender<anyhow::Result<Made>>,
) {
    let signal = rf.signal();
    let mut k = w;
    while k < chunks {
        let (first, count) = rf.chunk_range(k);
        let mut values = vec![0i16; count as usize * 2];
        signal.fill(first, &mut values);
        let mut bytes = Vec::new();
        let made = rf
            .encode_chunk::<FsError>(iq, first, &values, &mut bytes)
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
    let (defs, iq) = rf
        .definitions::<FsError>()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let options = ChainOptions {
        writer: WriterOptions {
            producer: "rf_chain bench".to_owned(),
            ..WriterOptions::default()
        },
        cut: CutPolicy {
            bytes: exact_units::Bytes::new(0),
            chunks: exact_units::Count::new(6_000), // a minute of 10 ms chunks
            interval: None,
        },
        tapes: PieceTapes::Each,
    };
    std::fs::create_dir_all(&args.dir)?;
    let mut chain =
        ww_tape_fs::chain::create(&args.dir, OwnerId([0x5d; 32]), 0, &[], options).map_err(e)?;
    let mut summaries = rf
        .summary_builder::<FsError>(iq)
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
            s.spawn(move || worker(rf, iq, w as u64, n, chunks, &tx));
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
    let args: Args = ww_tape_bench::args();
    let mut bench = Bench::start(
        "rf_chain",
        vc_build::build_info!(),
        &args.common,
        vec![
            ("MS/s", args.msps.to_string()),
            ("GiB", args.gib.to_string()),
            ("workers", args.workers.to_string()),
        ],
    )?;
    println!("{}", bench.header());
    if args.read_only {
        return read(&args, None, bench);
    }
    anyhow::ensure!(
        !args.dir.exists() || std::fs::read_dir(&args.dir)?.next().is_none(),
        "{} is not empty",
        args.dir.display()
    );
    // a capture longer than the target needs: an hour per 100 GiB at 20 MS/s is ample
    let hours = (args.gib.div_ceil(100) * 2) as u32;
    let rf = Rf::at(args.msps * 1_000_000).seconds(3_600 * hours);
    let mut table = bench.table(
        "chain",
        &format!(
            "IQ at {} MS/s into a chain of piece files, {} workers and one writer thread, until it holds {} GiB.",
            args.msps, args.workers, args.gib
        ),
        &["What", "Value", "Unit"],
    );
    let w = write(&args, rf)?;
    let secs = w.wall.as_secs_f64();
    let msps = w.samples as f64 / secs / 1e6;
    let (min, max) = (
        w.pieces.iter().min().copied().unwrap_or(0),
        w.pieces.iter().max().copied().unwrap_or(0),
    );
    let rows = [
        (
            "Capture recorded",
            TableCell::num(w.samples as f64 / (f64::from(args.msps) * 1e6), 1),
            "s",
        ),
        ("Wall time", TableCell::num(secs, 1), "s"),
        ("Sustained", TableCell::num(msps, 1), "MS/s"),
        ("Samples in", TableCell::num(msps * 4.0, 0), "MB/s"),
        (
            "Written",
            TableCell::num(w.bytes as f64 / (1u64 << 30) as f64, 2),
            "GiB",
        ),
        (
            "Write rate",
            TableCell::num(w.bytes as f64 / secs / 1e6, 0),
            "MB/s",
        ),
        (
            "Writer thread busy",
            TableCell::num(100.0 * w.busy.as_secs_f64() / secs, 0),
            "%",
        ),
        ("Pieces", TableCell::count(w.pieces.len() as u64), ""),
        ("Smallest piece", TableCell::num(min as f64 / 1e9, 2), "GB"),
        ("Largest piece", TableCell::num(max as f64 / 1e9, 2), "GB"),
        (
            "Bytes per sample",
            TableCell::num(w.bytes as f64 / w.samples as f64, 3),
            "",
        ),
    ];
    for (what, value, unit) in rows {
        table.row(vec![TableCell::text(what), value, TableCell::text(unit)]);
    }
    println!("{}", bench.push(table));
    read(&args, Some(w.samples), bench)
}

/// Open every piece in `--dir`, then read and decode one second of IQ: `--second`, or the middle of `samples`.
fn read(args: &Args, samples: Option<u64>, mut bench: Bench) -> anyhow::Result<()> {
    let mut table = bench.table(
        "read",
        "Every piece of the chain opened, then one second of IQ read and decoded.",
        &["Read", "ms", "Bytes read", "Reads"],
    );
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
                tracks.iter().any(|t| {
                    t.track_id == iq_key().id() && t.first_tick < to && from <= t.last_tick
                })
            });
        if !holds {
            continue;
        }
        for entry in r
            .track_entries(0, iq_key().id())
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
    table.row(vec![
        TableCell::text(format!("Open {} pieces", readers.len())),
        TableCell::num(open.0.as_secs_f64() * 1e3, 1),
        TableCell::count(open.1),
        TableCell::count(open.2),
    ]);
    table.row(vec![
        TableCell::text(format!("Second {second}: {got} samples decoded")),
        TableCell::num(query.0.as_secs_f64() * 1e3, 1),
        TableCell::count(query.1),
        TableCell::count(query.2),
    ]);
    println!("{}", bench.push(table));
    if args.remove {
        drop(readers);
        std::fs::remove_dir_all(&args.dir)?;
    }
    bench.finish()
}
