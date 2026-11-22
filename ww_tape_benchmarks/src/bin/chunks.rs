//! What a writer spends per series chunk: `TapeWriter::write_chunk` of a chunk it encodes from columns, and
//! `write_block` of the same chunk made by the caller, against the parts they are made of: encoding the block
//! (`chunk::write_chunk`: the copy and the CRCs), reading it back with its CRCs checked (`Block::read`), and the
//! content hash every placed block goes into (`ItemHash::of`, BLAKE3), and the unit hash of the chunk that its index
//! entry stores and its track's hash takes in (`TrackHash::chunk_unit`). Prints a Markdown table of ns per chunk and
//! GB/s for each, the best of `--rounds` rounds of `--chunks` chunks, with the load average around it. Then what a
//! reader spends on such a track: its series timeline from the chunk heads alone and from whole blocks, and the hash
//! of all its chunks from the index (`TapeReader::range_hash`).
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::print_stdout,
    reason = "a bench reads no file of others: it computes sizes and rates freely, fails by panicking and prints its table"
)]

use ww_tape::ItemNum;

use clap::Parser;
use shrink_wrap::prelude::*;
use std::hint::black_box;
use std::io;
use std::time::Instant;
use ww_tape::TrackNum;
use ww_tape::chunk::{ChunkHead, chunk_len, write_chunk};
use ww_tape::piece::payload_hash;
use ww_tape::reader::ByteSource;
use ww_tape::{
    ALIGNMENT, Block, BlockId, BlockKind, ChainOptions, ClockOrigin, ClockSpec, Codec, ColumnKind,
    ColumnSpec, CutPolicy, Definitions, EndMode, ItemHash, Layout, Magnitude, MemSink, MemTapes,
    OpenOptions, OwnerId, PayloadCheck, PieceChain, PieceTapes, Ratio64, Segment, SeriesDef,
    SeriesGrid, SeriesTimeline, SessionTag, SyncPolicy, TapeReader, TapeSink, TapeWriter,
    TrackHash, TrackKind, TrackSpec, WriterOptions, clock_key, track_key,
};
use ww_tape_bench::{Bench, Cell, Common, Measure, ns_per_try};

/// Chunks per piece of the chain row: a cut every 16 chunks.
const CHAIN_CUT_CHUNKS: u64 = 16;

#[derive(Parser)]
#[command(
    name = "chunks",
    about = "ww_tape series chunks: ns per chunk the writer spends, and its parts",
    version = vc_build::version!(),
    after_long_help = "Examples:
  just bench-chunks
  just bench-chunks --chunk-kib 4096 --rounds 3
  chunks --json docs/bench/chunks.json"
)]
struct Args {
    #[command(flatten)]
    common: Common,
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
    bytes: u64,
}

impl TapeSink for Discard {
    type Error = io::Error;

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.bytes += bytes.len() as u64;
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn close(&mut self) -> io::Result<()> {
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

    fn rebase(&mut self, base: u64) -> io::Result<()> {
        if base == 0 {
            Ok(())
        } else {
            Err(io::ErrorKind::Unsupported.into())
        }
    }
}

fn period() -> Ratio64 {
    Ratio64::new(62_500, 3).expect("48 kHz on a 1 ns clock")
}

/// The definitions of the series track and its clock, and the track's number.
fn definitions() -> anyhow::Result<(Definitions, TrackNum)> {
    let d = |e: ww_tape::DescribeError| anyhow::anyhow!("{e:?}");
    let mut defs = Definitions::new();
    let clock = defs
        .define_clock(ClockSpec {
            key: clock_key(0xc4, &[], 1),
            rate: Magnitude::pow10(9).into(),
            resolution: Magnitude::pow10(-9).into(),
            origin: ClockOrigin::Boot,
            label: "bench",
        })
        .map_err(d)?;
    let track = defs
        .define_track(TrackSpec {
            key: track_key(0xc4, &[Segment::Name("samples")]),
            clock,
            dimensions: RefVec::Slice { slice: &[] },
            items: ww_tape::Items::numbers::<i16>(None),
            label: "bench",
            repeat: ww_tape::TrackRepeat::Stream,
            kind: TrackKind::Series(SeriesDef::of::<i16>(1, period())),
            relations: RefVec::Slice { slice: &[] },
            owner: [0; 32],
        })
        .map_err(d)?;
    Ok((defs, track))
}

/// The head of chunk `k` of `count` i16 samples on track `track`.
fn head(track: TrackNum, k: u64, count: u32) -> anyhow::Result<ChunkHead> {
    let grid = SeriesGrid::start(0, 0, Magnitude::ONE);
    let first = k * u64::from(count);
    let (first_tick, last_tick) = grid
        .chunk_ticks(period(), ItemNum::new(first), count)
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    Ok(ChunkHead {
        kind: BlockKind::Series,
        // the track's id: its key, it has no dimensions
        track_id: track_key(0xc4, &[Segment::Name("samples")]).id(),
        track_number: track,
        first_item: ItemNum::new(first),
        count,
        first_tick,
        last_tick,
        latest_end: last_tick,
        time_mode: 0,
        end_mode: EndMode::Absent,
        grid: Some(grid),
    })
}

fn main() -> anyhow::Result<()> {
    let args: Args = ww_tape_bench::args();
    let mut bench = Bench::start(
        "chunks",
        vc_build::build_info!(),
        &args.common,
        vec![
            ("chunk KiB", args.chunk_kib.to_string()),
            ("chunks", args.chunks.to_string()),
            ("rounds", args.rounds.to_string()),
        ],
    )?;
    println!("{}", bench.header());
    let mut table = bench.table(
        "writer",
        &format!(
            "Series chunks of {} KiB ({} i16 samples), best of {} rounds of {}.",
            args.chunk_kib,
            args.chunk_kib * 512,
            args.rounds,
            args.chunks
        ),
        &["What", "ns per chunk", "GB/s"],
    );
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
    let (defs, track) = definitions()?;
    let h0 = head(track, 0, count)?;
    let mut block = vec![0u8; chunk_len(&h0, &[spec], 0)];
    let n = write_chunk(&mut block, id, &h0, &columns, 0).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    block.truncate(n);
    let e = |e: ww_tape::WriteError<_>| anyhow::anyhow!("{e:?}");
    let writer = || {
        let mut w = TapeWriter::create_on(Discard { bytes: 0 }, SyncPolicy::None);
        w.set_alignment(ALIGNMENT)?;
        Ok::<_, anyhow::Error>(w)
    };

    let (r, c) = (args.rounds, args.chunks as usize);
    let mut buf = vec![0u8; block.len()];
    let encode = ns_per_try(r, c, |k| {
        let h = head(track, k as u64, count)?;
        let n = write_chunk(&mut buf, id, &h, &columns, 0).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        black_box(n);
        Ok(())
    })?;
    let read = ns_per_try(r, c, |_| {
        black_box(Block::read(black_box(&block)).map_err(|e| anyhow::anyhow!("{e:?}"))?);
        Ok(())
    })?;
    let hash = ns_per_try(r, c, |_| {
        black_box(ItemHash::of(black_box(&block)));
        Ok(())
    })?;
    let whole = Block::read(&block).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let chunk = whole
        .chunk()
        .ok_or_else(|| anyhow::anyhow!("not a chunk"))?;
    let payload = payload_hash(chunk.payload());
    let unit = ns_per_try(r, c, |_| {
        black_box(TrackHash::chunk_unit(
            black_box(&chunk),
            black_box(&payload),
        ));
        Ok(())
    })?;
    let mut w = writer()?;
    let own = ns_per_try(r, c, |_| {
        let k = w.next_id().seq;
        w.write_chunk(&defs, &head(track, k, count)?, &columns)
            .map_err(e)?;
        Ok(())
    })?;
    // into memory, as a setup kept in RAM writes: every byte kept (rounds x chunks of them)
    let mut mem = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    mem.set_alignment(ALIGNMENT)?;
    let in_memory = ns_per_try(r, c, |_| {
        let k = mem.next_id().seq;
        mem.write_chunk(&defs, &head(track, k, count)?, &columns)
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        Ok(())
    })?;
    drop(mem);
    // a chain in one memory tape, a piece per CHAIN_CUT_CHUNKS chunks: growing the tape copies only the open piece
    let mut chain = PieceChain::create_on(
        MemTapes::new(),
        OwnerId([1; 32]),
        0,
        &[],
        ChainOptions {
            tapes: PieceTapes::One,
            writer: WriterOptions {
                sync: SyncPolicy::None,
                layout: Layout::ALIGNED,
                ..WriterOptions::default()
            },
            cut: CutPolicy {
                chunks: exact_units::Count::new(CHAIN_CUT_CHUNKS),
                ..CutPolicy::default()
            },
        },
    )
    .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let mut k = 0;
    let in_chain = ns_per_try(r, c, |_| {
        chain
            .write_chunk(&defs, &head(track, k, count)?, &columns)
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        k += 1;
        Ok(())
    })?;
    drop(chain);
    let mut w = writer()?;
    let theirs = ns_per_try(r, c, |_| {
        let k = w.next_id().seq;
        let h = head(track, k, count)?;
        let n = write_chunk(&mut buf, w.next_id(), &h, &columns, 0)
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        w.write_block(&defs, &buf[..n]).map_err(e)?;
        Ok(())
    })?;

    let gbs = |ns: f64| block.len() as f64 / ns;
    for (what, ns) in [
        ("`chunk::write_chunk`: copy and CRCs", encode),
        ("`Block::read`: the CRCs checked", read),
        ("`ItemHash::of`: BLAKE3", hash),
        ("`TrackHash::chunk_unit`: the unit hash", unit),
        ("`TapeWriter::write_chunk` (columns)", own),
        ("`TapeWriter::write_chunk` into a `MemSink`", in_memory),
        (
            "`PieceChain::write_chunk` into one memory tape, a cut per 16 chunks",
            in_chain,
        ),
        ("`chunk::write_chunk` + `TapeWriter::write_block`", theirs),
    ] {
        table.row(vec![
            Cell::text(what),
            Cell::num(ns, 0),
            Cell::num(gbs(ns), 2),
        ]);
    }
    println!("{}", bench.push(table));
    for alignment in [0, ALIGNMENT] {
        let table = timeline(&bench, &defs, track, &columns, count, &args, alignment)?;
        println!("{}", bench.push(table));
    }
    let table = payload_check(&bench, &columns, count, &args)?;
    println!("{}", bench.push(table));
    bench.finish()
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
        #[expect(
            clippy::map_err_ignore,
            reason = "a TryFromIntError says only that the number is out of range, which this error says"
        )]
        let at = usize::try_from(offset).map_err(|_| ())?;
        buf.copy_from_slice(self.bytes.get(at..at + buf.len()).ok_or(())?);
        self.read.set(self.read.get() + buf.len() as u64);
        Ok(())
    }
}

/// Bytes of the item of each records block of [payload_check]: a records block of about 4 KiB.
const RECORD_ITEM_LEN: usize = 4000;

/// What `TapeReader::read_block` costs per block with the CRCs over payloads checked and not
/// (`OpenOptions::check_payloads`): `--chunks` series chunks and as many records blocks of one 4,000-byte record, in a
/// tape in memory; ns per block and GB/s.
fn payload_check(
    bench: &Bench,
    columns: &[(ColumnSpec, &[u8])],
    count: u32,
    args: &Args,
) -> anyhow::Result<Measure> {
    let mut table = bench.table(
        "payload-check",
        &format!(
            "`TapeReader::read_block` of {} chunks and {} records blocks in memory, payload CRCs checked and not.",
            args.chunks, args.chunks
        ),
        &["Block", "Payload CRCs", "Bytes per block", "ns per block", "GB/s"],
    );
    let e = |e: ww_tape::WriteError<_>| anyhow::anyhow!("{e:?}");
    let (mut defs, track) = definitions()?;
    let events = defs
        .define_track(TrackSpec {
            key: track_key(0xc4, &[Segment::Name("events")]),
            clock: ww_tape::ClockNum::new(1),
            dimensions: RefVec::Slice { slice: &[] },
            items: ww_tape::Items::numbers::<i16>(None),
            label: "bench",
            repeat: ww_tape::TrackRepeat::Stream,
            kind: TrackKind::Events(ww_tape::EventsDef::INSTANTS),
            relations: RefVec::Slice { slice: &[] },
            owner: [0; 32],
        })
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    // bytes that don't compress, so a records block keeps its 4,000-byte item whole
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let item: Vec<u8> = (0..RECORD_ITEM_LEN)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect();
    for k in 0..u64::from(args.chunks) {
        w.write_chunk(&defs, &head(track, k, count)?, columns)
            .map_err(e)?;
        let mut buf = vec![0u8; RECORD_ITEM_LEN + 256];
        let clock = ww_tape::ClockNum::new(1);
        let mut b = ww_tape::BlockWriter::new(&mut buf, BlockKind::Records, w.next_id(), &[clock])
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        b.push(
            events,
            ww_tape::ClockTime::new(clock, k.cast_signed()),
            &[&item],
        )
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let n = b.finish();
        w.write_block(&defs, &buf[..n]).map_err(e)?;
    }
    let tape = w.sink().tape();
    w.close().map_err(e)?;
    let bytes = tape.to_vec().map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let re = |e: ww_tape::ReadError<ww_tape::reader::OutOfRange>| anyhow::anyhow!("{e:?}");
    for check in [PayloadCheck::Always, PayloadCheck::Never] {
        let options = OpenOptions {
            check_payloads: check,
            ..OpenOptions::default()
        };
        let r = TapeReader::open_with(bytes.as_slice(), options).map_err(re)?;
        let entries = r.entries(0).map_err(re)?;
        for (name, kind) in [
            ("chunk", BlockKind::Series),
            ("records", BlockKind::Records),
        ] {
            // the records blocks written above, not the definitions and the state the writer adds
            let these: Vec<_> = entries
                .iter()
                .filter(|x| x.kind == kind && x.len as usize > RECORD_ITEM_LEN)
                .collect();
            anyhow::ensure!(!these.is_empty(), "no {name} blocks");
            let len =
                these.iter().map(|x| u64::from(x.len)).sum::<u64>() as f64 / these.len() as f64;
            let mut buf = Vec::new();
            let ns = ns_per_try(args.rounds, these.len(), |i| {
                black_box(r.read_block(0, these[i], &mut buf).map_err(re)?);
                Ok(())
            })?;
            let what = if check == PayloadCheck::Always {
                "checked"
            } else {
                "not checked"
            };
            table.row(vec![
                Cell::text(name),
                Cell::text(what),
                Cell::num(len, 0),
                Cell::num(ns, 0),
                Cell::num(len / ns, 2),
            ]);
        }
    }
    Ok(table)
}

/// What a series timeline of a track of `--chunks` chunks costs a reader, from the chunk heads alone
/// (`TapeReader::series_timeline`) and from whole blocks, in a file of payloads aligned to `alignment`, in memory:
/// bytes read and ns per chunk; the track is `track`, held in `defs`.
fn timeline(
    bench: &Bench,
    defs: &Definitions,
    track: TrackNum,
    columns: &[(ColumnSpec, &[u8])],
    count: u32,
    args: &Args,
    alignment: u32,
) -> anyhow::Result<Measure> {
    let mut table = bench.table(
        &format!("timeline-{alignment}"),
        &format!(
            "Timeline of a track of {} chunks, aligned to {alignment} bytes (0: not aligned), in memory.",
            args.chunks
        ),
        &["Read", "Bytes read per chunk", "ns per chunk"],
    );
    let e = |e: ww_tape::WriteError<_>| anyhow::anyhow!("{e:?}");
    let dir = std::env::temp_dir().join(format!("ww_tape_bench_chunks_{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("timeline.wwt");
    let mut w = ww_tape_fs::writer::create(&path, SyncPolicy::None).map_err(e)?;
    w.set_alignment(alignment).map_err(e)?;
    for k in 0..u64::from(args.chunks) {
        w.write_chunk(defs, &head(track, k, count)?, columns)
            .map_err(e)?;
    }
    w.close().map_err(e)?;
    let src = Counted {
        bytes: std::fs::read(&path)?,
        read: 0.into(),
    };
    _ = std::fs::remove_dir_all(&dir);
    let r = TapeReader::open(&src).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let re = |e: ww_tape::ReadError<()>| anyhow::anyhow!("{e:?}");
    let track_id = head(track, 0, count)?.track_id;
    let c = f64::from(args.chunks);
    let mut heads = (f64::INFINITY, 0u64);
    let mut whole = (f64::INFINITY, 0u64);
    let mut fold = (f64::INFINITY, 0u64);
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

        let from = src.read.get();
        let t = Instant::now();
        let got = r
            .range_hash(
                track_id,
                OwnerId::NONE,
                ItemNum::new(0)..ItemNum::new(u64::MAX),
                TrackHash::START,
            )
            .map_err(re)?;
        let ns = t.elapsed().as_nanos() as f64 / c;
        anyhow::ensure!(
            got.read == 0 && got.chunks == u64::from(args.chunks),
            "{got:?}"
        );
        fold = (fold.0.min(ns), src.read.get() - from);
    }
    for (what, (ns, bytes)) in [
        ("`TapeReader::series_timeline`: heads alone", heads),
        ("`TapeReader::read_block` of every chunk", whole),
        ("`TapeReader::range_hash`: unit hashes from the index", fold),
    ] {
        table.row(vec![
            Cell::text(what),
            Cell::count(bytes / u64::from(args.chunks)),
            Cell::num(ns, 0),
        ]);
    }
    Ok(table)
}
