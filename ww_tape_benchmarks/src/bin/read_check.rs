//! What `TapeReader::read_block` costs per block with the CRCs over payloads checked and not
//! (`OpenOptions::check_payloads`): series chunks and records blocks read back from a tape in memory. Prints a
//! Markdown table of ns per block and GB/s, the best of `--rounds` rounds, with the load average around it. The book's
//! "When bytes go bad" draws its table from the report (`--json`).
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::indexing_slicing,
    clippy::print_stdout,
    reason = "a bench reads no file of others: it computes sizes and rates freely, fails by panicking and prints its table"
)]

use clap::Parser;
use shrink_wrap::prelude::*;
use std::hint::black_box;
use ww_tape::{
    BlockKind, Codec, ColumnKind, ColumnSpec, MemSink, OpenOptions, PayloadCheck, Segment,
    SyncPolicy, TapeReader, TapeWriter, TrackKind, TrackSpec, track_key,
};
use ww_tape_bench::{Bench, Cell, Common, Measure, ns_per_try};
use ww_tape_benchmarks::series::{definitions, head};

#[derive(Parser)]
#[command(
    name = "read_check",
    about = "ww_tape reads: ns per block of TapeReader::read_block with payload CRCs checked and not",
    version = vc_build::version!(),
    after_long_help = "Examples:
  just bench-read-check
  just bench-read-check --blocks 64 --rounds 3
  read_check --json docs/bench/read_check.json"
)]
struct Args {
    #[command(flatten)]
    common: Common,
    /// Bytes of samples per series chunk, KiB.
    #[arg(long, default_value_t = 1024)]
    chunk_kib: usize,
    /// Series chunks, and as many records blocks.
    #[arg(long, default_value_t = 256)]
    blocks: u32,
    /// Rounds; the best is kept.
    #[arg(long, default_value_t = 5)]
    rounds: u32,
}

/// Bytes of the item of each records block of [read]: a records block of about 4 KiB.
const RECORD_ITEM_LEN: usize = 4000;

/// What `TapeReader::read_block` costs per block with the CRCs over payloads checked and not
/// (`OpenOptions::check_payloads`): `--blocks` series chunks of `--chunk-kib` KiB and as many records blocks of one 4,000-byte record, in a
/// tape in memory; ns per block and GB/s.
fn read(bench: &Bench, args: &Args) -> anyhow::Result<Measure> {
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
    let mut table = bench.table(
        "read",
        &format!(
            "`TapeReader::read_block` of {} chunks and {} records blocks in memory, payload CRCs checked and not.",
            args.blocks, args.blocks
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
    for k in 0..u64::from(args.blocks) {
        w.write_chunk(&defs, &head(track, k, count)?, &columns)
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

fn main() -> anyhow::Result<()> {
    let args: Args = ww_tape_bench::args();
    let mut bench = Bench::start(
        "read_check",
        vc_build::build_info!(),
        &args.common,
        vec![
            ("chunk KiB", args.chunk_kib.to_string()),
            ("blocks", args.blocks.to_string()),
            ("rounds", args.rounds.to_string()),
        ],
    )?;
    println!("{}", bench.header());
    let table = read(&bench, &args)?;
    println!("{}", bench.push(table));
    bench.finish()
}
