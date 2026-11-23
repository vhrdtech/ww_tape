//! Whether verify, range-hash and resume's content-hash walk scale with cores over a large chain's pieces (FS-2).
//!
//! Three piece sets go into `--dir`, each sized so its own operation does real BLAKE3 work:
//!
//! - `verify_chain`: a closed chain at [Integrity::Full], so [TapeReader::verify] replays and re-hashes every byte.
//! - `range_chain`: a closed chain at [Integrity::CrcOnly], so its index entries carry no stated unit hash and
//!   [TapeReader::range_hash] must read and hash every chunk's payload (at `Full` it would just trust the index).
//! - `resume_pieces`: several single-piece chains left unclosed (the writer dropped without a cut or a close, as a
//!   crash leaves one), so [ww_tape_fs::writer::resume] does its from-scratch content-hash walk.
//!
//! Each operation is timed three ways: one piece at a time; pieces in parallel with rayon (each piece still
//! single-threaded inside); and, for comparison, BLAKE3's own multithreaded hashing
//! ([blake3::Hasher::update_rayon]) of a piece's raw bytes, one piece at a time. The third is a ceiling, not a
//! literal substitution: a piece's content hash is a chained hash with chunk payloads substituted by their own
//! hash, not one contiguous buffer, so `update_rayon` can't simply replace it inside [TapeReader::verify].
//!
//! The generated pieces are removed when the bench finishes.
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
use example_capture::rf::{Rf, iq_key};
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::{Duration, Instant};
use ww_tape::chunk::ChunkHead;
use ww_tape::{
    ChainOptions, ColumnSpec, CutPolicy, Integrity, ItemNum, OwnerId, PieceTapes, TapeReader,
    TrackHash, TrackId, TrackNum, WriterOptions,
};
use ww_tape_bench::{Bench, Cell, Common};
use ww_tape_fs::{FileSource, FsError};

#[derive(Parser)]
#[command(
    name = "archive_parallel",
    about = "Verify, range-hash and resume's content-hash walk over a chain's pieces: sequential, rayon-parallel, and BLAKE3's own multithreaded hashing (FS-2)",
    version = vc_build::version!(),
    after_long_help = "Examples:
  just bench-archive-parallel
  archive_parallel --dir /data/bench-archive --verify-gib 40 --range-gib 20"
)]
struct Args {
    #[command(flatten)]
    common: Common,
    /// The directory the piece sets are written into and removed from; it must not hold one.
    #[arg(long, default_value = "target/bench-archive-parallel")]
    dir: PathBuf,
    /// GiB the verify chain (`Integrity::Full`) grows to.
    #[arg(long, default_value_t = 10)]
    verify_gib: u64,
    /// GiB the range-hash chain (`Integrity::CrcOnly`) grows to.
    #[arg(long, default_value_t = 6)]
    range_gib: u64,
    /// Unclosed single-piece chains left for the resume walk.
    #[arg(long, default_value_t = 12)]
    resume_pieces: usize,
    /// MiB each unclosed piece grows to.
    #[arg(long, default_value_t = 384)]
    resume_mib: u64,
    /// MiB a verify or range-hash piece is cut at.
    #[arg(long, default_value_t = 512)]
    piece_mib: u64,
    /// Complex samples a second, millions, of the chunks generated.
    #[arg(long, default_value_t = 20)]
    msps: u32,
    /// Threads that make and code chunks while generating the verify and range-hash chains.
    #[arg(long, default_value_t = 8)]
    workers: usize,
    /// Keep the generated piece sets instead of removing them at the end.
    #[arg(long)]
    keep: bool,
}

const OWNER: OwnerId = OwnerId([0x61; 32]);
const PRODUCER: &str = "archive_parallel bench";

/// A chunk made by a worker: its head, its coded column.
struct Made {
    head: ChunkHead,
    spec: ColumnSpec,
    bytes: Vec<u8>,
}

/// Worker `w` of `n`: chunks `w`, `w + n`, ... of `rf` on track `iq`, each into `tx`.
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
            .map(|(head, spec)| Made { head, spec, bytes })
            .map_err(|e| anyhow::anyhow!("{e}"));
        if tx.send(made).is_err() {
            return;
        }
        k += n;
    }
}

fn writer_options(integrity: Integrity) -> WriterOptions {
    WriterOptions {
        producer: PRODUCER.to_owned(),
        integrity,
        ..WriterOptions::default()
    }
}

/// A chain of closed pieces at `dir`, cut every `piece_bytes`, until it holds `target_bytes`, at `integrity`.
fn write_chain(
    dir: &Path,
    rf: Rf,
    piece_bytes: u64,
    target_bytes: u64,
    integrity: Integrity,
    workers: usize,
) -> anyhow::Result<Duration> {
    let e = |e: ww_tape::WriteError<FsError>| anyhow::anyhow!("{e:?}");
    let (defs, iq) = rf
        .definitions::<FsError>()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let options = ChainOptions {
        writer: writer_options(integrity),
        cut: CutPolicy {
            bytes: exact_units::Bytes::new(piece_bytes),
            chunks: exact_units::Count::new(0),
            interval: None,
        },
        tapes: PieceTapes::Each,
    };
    std::fs::create_dir_all(dir)?;
    let mut chain = ww_tape_fs::chain::create(dir, OWNER, 0, &[], options).map_err(e)?;
    let chunks = rf.chunks();
    let n = workers.max(1) as u64;
    let (txs, rxs): (Vec<_>, Vec<Receiver<_>>) = (0..n).map(|_| sync_channel(4)).unzip();
    let start = Instant::now();
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
            if chain.cut_due() {
                chain.cut_now(&defs).map_err(e)?;
            }
            chain
                .write_chunk(&defs, &made.head, &[(made.spec, &made.bytes)])
                .map_err(e)?;
            let closed: u64 = chain.closed().iter().map(|p| p.piece.len).sum();
            if closed.saturating_add(chain.writer().map_err(e)?.bytes()) >= target_bytes {
                break;
            }
        }
        drop(rxs);
        Ok(())
    })?;
    chain.close().map_err(e)?;
    Ok(start.elapsed())
}

/// One single-piece chain at `dir`, written to about `target_bytes` and left unclosed, as a crash leaves one.
fn write_unclosed(dir: &Path, rf: Rf, target_bytes: u64) -> anyhow::Result<()> {
    let e = |e: ww_tape::WriteError<FsError>| anyhow::anyhow!("{e:?}");
    let (defs, iq) = rf
        .definitions::<FsError>()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let options = ChainOptions {
        writer: writer_options(Integrity::Full),
        // never cut: one piece only, left open
        cut: CutPolicy {
            bytes: exact_units::Bytes::new(0),
            chunks: exact_units::Count::new(0),
            interval: None,
        },
        tapes: PieceTapes::Each,
    };
    std::fs::create_dir_all(dir)?;
    let mut chain = ww_tape_fs::chain::create(dir, OWNER, 0, &[], options).map_err(e)?;
    let signal = rf.signal();
    let mut k = 0u64;
    loop {
        let (first, count) = rf.chunk_range(k);
        if count == 0 {
            break;
        }
        let mut values = vec![0i16; count as usize * 2];
        signal.fill(first, &mut values);
        let mut bytes = Vec::new();
        let (head, spec) = rf
            .encode_chunk::<FsError>(iq, first, &values, &mut bytes)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        chain
            .write_chunk(&defs, &head, &[(spec, &bytes)])
            .map_err(e)?;
        if chain.writer().map_err(e)?.bytes() >= target_bytes {
            break;
        }
        k += 1;
    }
    // deliberately no cut_now/close: the file stays as a crash would leave it, for the resume bench
    drop(chain);
    Ok(())
}

/// The `.wwt` files of `dir`, sorted (chain order).
fn pieces_in(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)?
        .map(|e| e.map(|e| e.path()))
        .collect::<Result<_, _>>()?;
    paths.retain(|p| p.extension().is_some_and(|x| x == "wwt"));
    paths.sort();
    Ok(paths)
}

fn total_bytes(paths: &[PathBuf]) -> anyhow::Result<u64> {
    let mut sum = 0u64;
    for p in paths {
        sum += std::fs::metadata(p)?.len();
    }
    Ok(sum)
}

fn verify_piece(path: &Path) -> anyhow::Result<()> {
    let src = FileSource::open(path).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let r = TapeReader::open(src).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let v = r.verify(0).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    anyhow::ensure!(v.holds, "{}: verify didn't hold", path.display());
    Ok(())
}

fn range_hash_piece(path: &Path, track_id: TrackId, owner: OwnerId) -> anyhow::Result<()> {
    let src = FileSource::open(path).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let r = TapeReader::open(src).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let rh = r
        .range_hash(
            track_id,
            owner,
            ItemNum::new(0)..ItemNum::new(u64::MAX),
            TrackHash::START,
        )
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    anyhow::ensure!(
        rh.chunks > 0,
        "{}: range_hash folded no chunks (a generation or owner bug, not a measurement)",
        path.display()
    );
    Ok(())
}

fn resume_piece(path: &Path, options: &WriterOptions) -> anyhow::Result<()> {
    let (_writer, resumed) =
        ww_tape_fs::writer::resume(path, options.clone()).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    anyhow::ensure!(
        resumed.dropped_bytes == 0 && resumed.cut.is_none(),
        "{}: resume dropped bytes it shouldn't have (a generation bug, not damage)",
        path.display()
    );
    Ok(())
}

/// Read every byte of `paths` into the page cache and drop it, so a timed pass that follows starts from the same
/// (warm) cache state as the one it is compared against: without this, whichever strategy runs first pays for
/// every cold read and the other looks faster for a reason that has nothing to do with being sequential or
/// parallel.
fn warm(paths: &[PathBuf]) -> anyhow::Result<()> {
    paths.par_iter().try_for_each(|p| -> anyhow::Result<()> {
        std::fs::read(p)?;
        Ok(())
    })
}

/// `f` over `paths` one at a time, after warming the page cache.
fn timed_seq<F: Fn(&Path) -> anyhow::Result<()>>(
    paths: &[PathBuf],
    f: F,
) -> anyhow::Result<Duration> {
    warm(paths)?;
    let t = Instant::now();
    for p in paths {
        f(p)?;
    }
    Ok(t.elapsed())
}

/// `f` over `paths`, pieces in parallel with rayon, after warming the page cache.
fn timed_par<F: Fn(&Path) -> anyhow::Result<()> + Sync>(
    paths: &[PathBuf],
    f: F,
) -> anyhow::Result<Duration> {
    warm(paths)?;
    let t = Instant::now();
    paths.par_iter().try_for_each(|p| f(p))?;
    Ok(t.elapsed())
}

/// Hash every piece's raw bytes with BLAKE3, one piece at a time after warming the page cache, `multithread`
/// picking [blake3::Hasher::update] or [blake3::Hasher::update_rayon]: the ceiling a piece's own multithreaded
/// hashing could reach.
fn raw_hash_seq(paths: &[PathBuf], multithread: bool) -> anyhow::Result<Duration> {
    warm(paths)?;
    let t = Instant::now();
    for p in paths {
        let bytes = std::fs::read(p)?;
        let mut h = blake3::Hasher::new();
        if multithread {
            h.update_rayon(&bytes);
        } else {
            h.update(&bytes);
        }
        let _ = h.finalize();
    }
    Ok(t.elapsed())
}

fn gib(bytes: u64) -> f64 {
    bytes as f64 / (1u64 << 30) as f64
}

fn gb_per_s(bytes: u64, wall: Duration) -> f64 {
    bytes as f64 / 1e9 / wall.as_secs_f64()
}

fn op_table(
    bench: &mut Bench,
    id: &str,
    about: &str,
    bytes: u64,
    pieces: usize,
    seq: Duration,
    par: Duration,
) {
    let mut table = bench.table(
        id,
        about,
        &["Strategy", "Wall", "Throughput", "Speedup vs sequential"],
    );
    table.row(vec![
        Cell::text(format!("{pieces} pieces one at a time")),
        Cell::num(seq.as_secs_f64(), 1),
        Cell::num(gb_per_s(bytes, seq), 2),
        Cell::num(1.0, 2),
    ]);
    table.row(vec![
        Cell::text(format!("{pieces} pieces in parallel (rayon)")),
        Cell::num(par.as_secs_f64(), 1),
        Cell::num(gb_per_s(bytes, par), 2),
        Cell::num(seq.as_secs_f64() / par.as_secs_f64(), 2),
    ]);
    println!("{}", bench.push(table));
}

/// The three piece sets this bench measures, generated once, and the paths and bytes of each.
struct Sets {
    verify: Vec<PathBuf>,
    verify_bytes: u64,
    range: Vec<PathBuf>,
    range_bytes: u64,
    resume: Vec<PathBuf>,
    resume_bytes: u64,
}

/// Writes `verify_dir`, `range_dir` and `resume_dir` (under `args.dir`) and a "generate" table of them.
fn generate(bench: &mut Bench, args: &Args, rf: Rf) -> anyhow::Result<Sets> {
    let verify_dir = args.dir.join("verify_chain");
    let range_dir = args.dir.join("range_chain");
    let resume_dir = args.dir.join("resume_pieces");
    let piece_bytes = args.piece_mib << 20;
    let mut table = bench.table(
        "generate",
        "The three piece sets this bench measures, generated once.",
        &["Set", "GiB", "Pieces", "Generated in", "Write rate"],
    );

    let verify_time = write_chain(
        &verify_dir,
        rf,
        piece_bytes,
        args.verify_gib << 30,
        Integrity::Full,
        args.workers,
    )?;
    let verify = pieces_in(&verify_dir)?;
    let verify_bytes = total_bytes(&verify)?;
    table.row(vec![
        Cell::text("verify_chain (Integrity::Full)"),
        Cell::num(gib(verify_bytes), 2),
        Cell::count(verify.len() as u64),
        Cell::num(verify_time.as_secs_f64(), 1),
        Cell::num(gb_per_s(verify_bytes, verify_time), 0),
    ]);

    let range_time = write_chain(
        &range_dir,
        rf,
        piece_bytes,
        args.range_gib << 30,
        Integrity::CrcOnly,
        args.workers,
    )?;
    let range = pieces_in(&range_dir)?;
    let range_bytes = total_bytes(&range)?;
    table.row(vec![
        Cell::text("range_chain (Integrity::CrcOnly)"),
        Cell::num(gib(range_bytes), 2),
        Cell::count(range.len() as u64),
        Cell::num(range_time.as_secs_f64(), 1),
        Cell::num(gb_per_s(range_bytes, range_time), 0),
    ]);

    let resume_mib = args.resume_mib;
    let resume_start = Instant::now();
    std::thread::scope(|s| -> anyhow::Result<()> {
        let mut handles = Vec::new();
        for i in 0..args.resume_pieces {
            let dir = resume_dir.join(format!("{i:04}"));
            handles.push(s.spawn(move || write_unclosed(&dir, rf, resume_mib << 20)));
        }
        for h in handles {
            match h.join() {
                Ok(r) => r?,
                Err(e) => {
                    let msg = e
                        .downcast_ref::<&str>()
                        .map(|s| (*s).to_owned())
                        .or_else(|| e.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "a generator thread panicked".to_owned());
                    anyhow::bail!(msg);
                }
            }
        }
        Ok(())
    })?;
    let resume_time = resume_start.elapsed();
    let resume: Vec<PathBuf> = (0..args.resume_pieces)
        .map(|i| {
            pieces_in(&resume_dir.join(format!("{i:04}")))
                .map(|mut p| p.pop().context("a resume piece"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?
        .into_iter()
        .collect::<anyhow::Result<Vec<_>>>()?;
    let resume_bytes = total_bytes(&resume)?;
    table.row(vec![
        Cell::text("resume_pieces (unclosed)"),
        Cell::num(gib(resume_bytes), 2),
        Cell::count(resume.len() as u64),
        Cell::num(resume_time.as_secs_f64(), 1),
        Cell::num(gb_per_s(resume_bytes, resume_time), 0),
    ]);
    println!("{}", bench.push(table));

    Ok(Sets {
        verify,
        verify_bytes,
        range,
        range_bytes,
        resume,
        resume_bytes,
    })
}

fn main() -> anyhow::Result<()> {
    let args: Args = ww_tape_bench::args();
    anyhow::ensure!(
        !args.dir.exists() || std::fs::read_dir(&args.dir)?.next().is_none(),
        "{} is not empty",
        args.dir.display()
    );
    let mut bench = Bench::start(
        "archive_parallel",
        vc_build::build_info!(),
        &args.common,
        vec![
            ("verify GiB", args.verify_gib.to_string()),
            ("range GiB", args.range_gib.to_string()),
            ("resume pieces", args.resume_pieces.to_string()),
            ("workers", args.workers.to_string()),
        ],
    )?;
    println!("{}", bench.header());

    // a capture longer than any target needs
    let hours = (args.verify_gib.max(args.range_gib).div_ceil(10) + 1) as u32;
    let rf = Rf::at(args.msps * 1_000_000)
        .seconds(3_600 * hours)
        .codec(ww_tape::Codec::PACKED12_LSB);
    let rf = Rf {
        summaries: false,
        ..rf
    };

    let Sets {
        verify: verify_pieces,
        verify_bytes,
        range: range_pieces,
        range_bytes,
        resume: resume_pieces,
        resume_bytes,
    } = generate(&mut bench, &args, rf)?;

    let track_id = iq_key().id();

    let v_seq = timed_seq(&verify_pieces, verify_piece)?;
    let v_par = timed_par(&verify_pieces, verify_piece)?;
    op_table(
        &mut bench,
        "verify",
        "TapeReader::verify over every piece of verify_chain, each replayed and content-hashed whole.",
        verify_bytes,
        verify_pieces.len(),
        v_seq,
        v_par,
    );

    // Rf::iq_track defines the track with TrackSpec::owner all zero ("the owner of the piece"): the stored rows
    // keep that literal sentinel, so range_hash is asked for OwnerId::NONE, not the chain's own OwnerId.
    let r_seq = timed_seq(&range_pieces, |p| {
        range_hash_piece(p, track_id, OwnerId::NONE)
    })?;
    let r_par = timed_par(&range_pieces, |p| {
        range_hash_piece(p, track_id, OwnerId::NONE)
    })?;
    op_table(
        &mut bench,
        "range_hash",
        "TapeReader::range_hash over every piece of range_chain: no stated unit hash, so every chunk's payload is read and hashed.",
        range_bytes,
        range_pieces.len(),
        r_seq,
        r_par,
    );

    let resume_options = writer_options(Integrity::Full);
    let s_seq = timed_seq(&resume_pieces, |p| resume_piece(p, &resume_options))?;
    let s_par = timed_par(&resume_pieces, |p| resume_piece(p, &resume_options))?;
    op_table(
        &mut bench,
        "resume",
        "ww_tape_fs::writer::resume over every unclosed piece of resume_pieces: a from-scratch content-hash walk, as a crash leaves one.",
        resume_bytes,
        resume_pieces.len(),
        s_seq,
        s_par,
    );

    let mut table = bench.table(
        "blake3_raw",
        "For comparison: BLAKE3 of a piece's raw bytes, one piece at a time, single- and multithreaded (update_rayon). A piece's content hash is a chained hash with chunk payloads substituted by their own hash, not one contiguous buffer, so this is a ceiling, not a literal stand-in for the rows above.",
        &["Set", "GiB", "Single-threaded", "update_rayon", "Ratio"],
    );
    for (name, paths, bytes) in [
        ("verify_chain", &verify_pieces, verify_bytes),
        ("range_chain", &range_pieces, range_bytes),
        ("resume_pieces", &resume_pieces, resume_bytes),
    ] {
        let single = raw_hash_seq(paths, false)?;
        let multi = raw_hash_seq(paths, true)?;
        table.row(vec![
            Cell::text(name),
            Cell::num(gib(bytes), 2),
            Cell::num(gb_per_s(bytes, single), 2),
            Cell::num(gb_per_s(bytes, multi), 2),
            Cell::num(single.as_secs_f64() / multi.as_secs_f64(), 2),
        ]);
    }
    println!("{}", bench.push(table));

    if !args.keep {
        std::fs::remove_dir_all(&args.dir)?;
    }
    bench.finish()
}
