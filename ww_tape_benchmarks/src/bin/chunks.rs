//! What a writer spends per series chunk: `TapeWriter::write_chunk` of a chunk it encodes from columns, and
//! `write_block` of the same chunk made by the caller, against the parts they are made of: encoding the block
//! (`chunk::write_chunk`: the copy and the CRCs), reading it back with its CRCs checked (`Block::read`), and the
//! content hash every placed block goes into (`ItemHash::of`, BLAKE3). Prints a Markdown table of ns per chunk and
//! GB/s for each, the best of `--rounds` rounds of `--chunks` chunks, with the load average at the start.

use clap::Parser;
use shrink_wrap::prelude::*;
use std::hint::black_box;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;
use ww_tape::chunk::{ChunkHead, chunk_len, write_chunk};
use ww_tape::{
    ALIGNMENT, Block, BlockId, BlockKind, ClockDef, ClockOrigin, Codec, ColumnKind, ColumnSpec,
    Definitions, Describe, EndMode, FullVersion, ItemHash, Magnitude, Ratio64, Segment, SeriesDef,
    SeriesGrid, SessionTag, SyncPolicy, TapeSink, TapeWriter, TrackDef, TrackKind, Version,
    clock_key, track_key,
};

#[derive(Parser)]
#[command(about = "ww_tape series chunks: ns per chunk the writer spends, and its parts")]
struct Args {
    /// Bytes of samples per chunk, KiB.
    #[arg(long, default_value_t = 1024)]
    chunk_kib: usize,
    /// Chunks per round.
    #[arg(long, default_value_t = 256)]
    chunks: u32,
    /// Rounds; the best is kept.
    #[arg(long, default_value_t = 5)]
    rounds: u32,
}

/// A sink that keeps no bytes: the writer's own work is measured, not a file's.
struct Discard {
    path: PathBuf,
    bytes: u64,
}

impl TapeSink for Discard {
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.bytes += bytes.len() as u64;
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn finish(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn size(&self) -> io::Result<u64> {
        Ok(self.bytes)
    }

    fn read_at(&self, _: u64, _: &mut [u8]) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "a discarding sink keeps no bytes",
        ))
    }

    fn truncate(&mut self, _: u64) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "a discarding sink keeps no bytes",
        ))
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

const CLOCK: u32 = 1;
const TRACK: u32 = 1;

fn period() -> Ratio64 {
    Ratio64::new(62_500, 3).expect("48 kHz on a 1 ns clock")
}

fn definitions() -> anyhow::Result<Definitions> {
    let mut defs = Definitions::new();
    defs.define_all(&[
        Describe::Clock(ClockDef {
            number: UNib32(CLOCK),
            key: clock_key(0xc4, &[], 1),
            rate: Magnitude::pow10(9).into(),
            resolution: Magnitude::pow10(-9).into(),
            origin: ClockOrigin::Boot,
            label: "bench",
        }),
        Describe::Track(TrackDef {
            number: UNib32(TRACK),
            key: track_key(0xc4, &[Segment::Name("samples")]),
            clock: UNib32(CLOCK),
            dimensions: RefVec::Slice { slice: &[] },
            api: FullVersion::new("bench_api", Version::new(0, 1, 0)),
            bundle: RefVec::Slice { slice: &[] },
            path: RefVec::Slice {
                slice: &[UNib32(1)],
            },
            keys: RefVec::Slice { slice: &[] },
            item: None,
            label: "bench",
            repeat: ww_tape::TrackRepeat::Stream,
            kind: TrackKind::Series(SeriesDef::of::<i16>(None, 1, period())),
        }),
    ])
    .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    Ok(defs)
}

/// The head of chunk `k` of `count` i16 samples.
fn head(k: u64, count: u32) -> anyhow::Result<ChunkHead> {
    let grid = SeriesGrid::start(0, 0, Magnitude::ONE);
    let first = k * u64::from(count);
    let (first_tick, last_tick) = grid
        .chunk_ticks(period(), first, count)
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    Ok(ChunkHead {
        kind: BlockKind::Series,
        track_id: 7,
        track_number: TRACK,
        first_item: first,
        count,
        first_tick,
        last_tick,
        latest_end: last_tick,
        time_mode: 0,
        end_mode: EndMode::Absent,
        grid: Some(grid),
    })
}

/// The best of `rounds` rounds of `f` over `chunks` chunks, in ns per chunk.
fn best(
    rounds: u32,
    chunks: u32,
    mut f: impl FnMut(u64) -> anyhow::Result<()>,
) -> anyhow::Result<f64> {
    let mut best = f64::INFINITY;
    for _ in 0..rounds {
        let t = Instant::now();
        for k in 0..u64::from(chunks) {
            f(k)?;
        }
        best = best.min(t.elapsed().as_nanos() as f64 / f64::from(chunks));
    }
    Ok(best)
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let load = std::fs::read_to_string("/proc/loadavg")
        .map(|l| l.split_whitespace().take(3).collect::<Vec<_>>().join(" "))
        .unwrap_or_else(|_| "unknown".to_owned());
    let bytes = args.chunk_kib * 1024;
    let count = u32::try_from(bytes / 2)?;
    let samples: Vec<u8> = (0..bytes).map(|i| (i * 31 % 251) as u8).collect();
    let spec = ColumnSpec {
        kind: ColumnKind::Samples,
        codec: Codec::RAW,
        param: 0,
        len: samples.len(),
    };
    let columns = [(spec, samples.as_slice())];
    let id = BlockId::new(SessionTag::writable(1), 0);
    let h0 = head(0, count)?;
    let mut block = vec![0u8; chunk_len(&h0, &[spec], 0)];
    let n = write_chunk(&mut block, id, &h0, &columns, 0).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    block.truncate(n);
    let defs = definitions()?;
    let e = |e: ww_tape::WriteError| anyhow::anyhow!("{e:?}");
    let writer = || {
        let mut w = TapeWriter::create_on(
            Discard {
                path: PathBuf::from("chunks"),
                bytes: 0,
            },
            SyncPolicy::None,
        );
        w.set_alignment(ALIGNMENT)?;
        Ok::<_, anyhow::Error>(w)
    };

    let (r, c) = (args.rounds, args.chunks);
    let mut buf = vec![0u8; block.len()];
    let encode = best(r, c, |k| {
        let h = head(k, count)?;
        let n = write_chunk(&mut buf, id, &h, &columns, 0).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        black_box(n);
        Ok(())
    })?;
    let read = best(r, c, |_| {
        black_box(Block::read(black_box(&block)).map_err(|e| anyhow::anyhow!("{e:?}"))?);
        Ok(())
    })?;
    let hash = best(r, c, |_| {
        black_box(ItemHash::of(black_box(&block)));
        Ok(())
    })?;
    let mut w = writer()?;
    let own = best(r, c, |_| {
        let k = w.next_id().seq;
        w.write_chunk(&defs, &head(k, count)?, &columns)
            .map_err(e)?;
        Ok(())
    })?;
    let mut w = writer()?;
    let theirs = best(r, c, |_| {
        let k = w.next_id().seq;
        let h = head(k, count)?;
        let n = write_chunk(&mut buf, w.next_id(), &h, &columns, 0)
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        w.write_block(&defs, &buf[..n]).map_err(e)?;
        Ok(())
    })?;

    println!(
        "Series chunks of {} KiB ({count} i16 samples), best of {r} rounds of {c}; load average {load}.\n",
        args.chunk_kib
    );
    println!("| What | ns per chunk | GB/s |");
    println!("|---|---:|---:|");
    let gbs = |ns: f64| block.len() as f64 / ns;
    for (what, ns) in [
        ("`chunk::write_chunk`: copy and CRCs", encode),
        ("`Block::read`: the CRCs checked", read),
        ("`ItemHash::of`: BLAKE3", hash),
        ("`TapeWriter::write_chunk` (columns)", own),
        ("`chunk::write_chunk` + `TapeWriter::write_block`", theirs),
    ] {
        println!("| {what} | {ns:.0} | {:.2} |", gbs(ns));
    }
    Ok(())
}
