//! What a writer spends per series chunk: `TapeWriter::write_chunk` of a chunk it encodes from columns, and
//! `write_block` of the same chunk made by the caller, against the parts they are made of: encoding the block
//! (`chunk::write_chunk`: the copy and the CRCs), reading it back with its CRCs checked (`Block::read`), and the
//! content hash every placed block goes into (`ItemHash::of`, BLAKE3). Prints a Markdown table of ns per chunk and
//! GB/s for each, the best of `--rounds` rounds of `--chunks` chunks, with the load average at the start. Then what a
//! reader spends on the series timeline of such a track: from the chunk heads alone, and from whole blocks.

use clap::Parser;
use shrink_wrap::prelude::*;
use std::hint::black_box;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;
use ww_tape::chunk::{ChunkHead, chunk_len, write_chunk};
use ww_tape::reader::ByteSource;
use ww_tape::{
    ALIGNMENT, Block, BlockId, BlockKind, ClockDef, ClockOrigin, Codec, ColumnKind, ColumnSpec,
    Definitions, Describe, EndMode, FullVersion, ItemHash, Magnitude, Ratio64, Segment, SeriesDef,
    SeriesGrid, SeriesTimeline, SessionTag, SyncPolicy, TapeReader, TapeSink, TapeWriter, TrackDef,
    TrackKind, Version, clock_key, track_key,
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
            parent: None,
            context: RefVec::Slice { slice: &[] },
            entity_key: None,
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
    timeline(&defs, &columns, count, &args, 0)?;
    timeline(&defs, &columns, count, &args, ALIGNMENT)
}

/// Bytes that count what is read of them.
struct Counted {
    bytes: Vec<u8>,
    read: std::cell::Cell<u64>,
}

impl ByteSource for Counted {
    type Error = ();

    fn size(&self) -> Result<u64, ()> {
        Ok(self.bytes.len() as u64)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), ()> {
        let at = usize::try_from(offset).map_err(|_| ())?;
        buf.copy_from_slice(self.bytes.get(at..at + buf.len()).ok_or(())?);
        self.read.set(self.read.get() + buf.len() as u64);
        Ok(())
    }
}

/// What a series timeline of a track of `--chunks` chunks costs a reader, from the chunk heads alone
/// (`TapeReader::series_timeline`) and from whole blocks, in a file of payloads aligned to `alignment`, in memory:
/// bytes read and ns per chunk.
fn timeline(
    defs: &Definitions,
    columns: &[(ColumnSpec, &[u8])],
    count: u32,
    args: &Args,
    alignment: u32,
) -> anyhow::Result<()> {
    let e = |e: ww_tape::WriteError| anyhow::anyhow!("{e:?}");
    let dir = std::env::temp_dir().join(format!("ww_tape_bench_chunks_{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("timeline.wwt");
    let mut w = TapeWriter::create(&path, SyncPolicy::None).map_err(e)?;
    w.set_alignment(alignment).map_err(e)?;
    for k in 0..u64::from(args.chunks) {
        w.write_chunk(defs, &head(k, count)?, columns).map_err(e)?;
    }
    w.finish().map_err(e)?;
    let src = Counted {
        bytes: std::fs::read(&path)?,
        read: 0.into(),
    };
    _ = std::fs::remove_dir_all(&dir);
    let r = TapeReader::open(&src).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let re = |e: ww_tape::ReadError<()>| anyhow::anyhow!("{e:?}");
    let track_id = head(0, count)?.track_id;
    let c = f64::from(args.chunks);
    let mut heads = (f64::INFINITY, 0u64);
    let mut whole = (f64::INFINITY, 0u64);
    for _ in 0..args.rounds {
        let from = src.read.get();
        let t = Instant::now();
        let (tl, errors) = r.series_timeline(0, track_id, period()).map_err(re)?;
        let ns = t.elapsed().as_nanos() as f64 / c;
        anyhow::ensure!(errors.is_empty() && tl.stretches().len() == 1, "{errors:?}");
        heads = (heads.0.min(ns), src.read.get() - from);

        let from = src.read.get();
        let t = Instant::now();
        let mut buf = Vec::new();
        let mut tl = SeriesTimeline::new(period());
        for entry in r.track_entries(0, track_id).map_err(re)? {
            let b = r.read_block(0, &entry, &mut buf).map_err(re)?;
            let h = b
                .chunk()
                .ok_or_else(|| anyhow::anyhow!("not a chunk"))?
                .head();
            tl.push_chunk(&h).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        }
        let ns = t.elapsed().as_nanos() as f64 / c;
        anyhow::ensure!(tl.stretches().len() == 1);
        whole = (whole.0.min(ns), src.read.get() - from);
    }
    println!(
        "\nTimeline of a track of {} chunks, aligned to {alignment} bytes (0: not aligned), in memory.\n",
        args.chunks
    );
    println!("| Read | Bytes read per chunk | ns per chunk |");
    println!("|---|---:|---:|");
    for (what, (ns, bytes)) in [
        ("`TapeReader::series_timeline`: heads alone", heads),
        ("`TapeReader::read_block` of every chunk", whole),
    ] {
        println!("| {what} | {} | {ns:.0} |", bytes / u64::from(args.chunks));
    }
    Ok(())
}
