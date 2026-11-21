//! What track relations cost in events chunks of 4096 items: the bytes per item of a relation column for the shapes
//! of a decoder's source ordinal and of keys, encoding it (`EncodedRelation::encode`), reading every value
//! (`RelationColumn::get`) and a lower bound in it, and a writer's `write_chunk` of an events chunk with and without
//! two relation columns (the column check and the range the index entry takes included). Prints Markdown tables, the
//! best of `--rounds` rounds, with the load average before and after each table.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::indexing_slicing,
    clippy::print_stdout,
    reason = "a bench reads no file of others: it computes sizes and rates freely, fails by panicking and prints its table"
)]

use ww_tape::ItemNum;

use clap::Parser;
use shrink_wrap::prelude::*;
use std::time::Instant;
use ww_tape::times::{EncodedRelation, EncodedTimes, RelationColumn, StoredRelations};
use ww_tape::{
    Block, ClockOrigin, ClockSpec, Definitions, EventsDef, FullVersion, Magnitude, MemSink,
    RelationDef, RelationIndex, RelationTarget, RelationValues, Segment, SyncPolicy, TapeWriter,
    TrackKind, TrackSpec, Version, clock_key, track_key,
};
use ww_tape::{ClockNum, TrackNum};
use ww_tape::{RelationSet, TrackId};
use ww_tape_bench::{Bench, Cell, Common, Rng, ns_per};

#[derive(Parser)]
#[command(
    name = "relations",
    about = "ww_tape track relations: column bytes, encode, read, lower bound, write_chunk",
    version = vc_build::version!(),
    after_long_help = "Examples:
  just bench-relations
  relations --chunks 500"
)]
struct Args {
    #[command(flatten)]
    common: Common,
    /// Rounds, the best one counts.
    #[arg(long, default_value_t = 5)]
    rounds: u32,
    /// Chunks per round of the writer rows.
    #[arg(long, default_value_t = 2000)]
    chunks: u32,
}

const N: usize = 4096;

/// The values of each shape, `N` of them, and whether they never decrease.
fn shapes() -> Vec<(&'static str, Vec<u64>, bool)> {
    let mut r = Rng(0x5eed);
    let mut walk = |step: &mut dyn FnMut(&mut Rng) -> u64| {
        let mut v = 1_000_000_000u64;
        (0..N)
            .map(|_| {
                v += step(&mut r);
                v
            })
            .collect::<Vec<u64>>()
    };
    let one = walk(&mut |_| 1);
    let dropped = walk(&mut |r| 1 + u64::from(r.below(50) == 0));
    let ten = walk(&mut |r| u64::from(r.below(10) == 0));
    let sparse = walk(&mut |r| 500 + r.below(1001));
    let mut r = Rng(0xbee5);
    let keys20: Vec<u64> = (0..20).map(|_| r.below(2048)).collect();
    let keys: Vec<u64> = (0..N).map(|_| keys20[r.below(20) as usize]).collect();
    let wide: Vec<u64> = (0..N).map(|_| r.below(u64::MAX)).collect();
    vec![
        ("source, 1:1", one, true),
        ("source, 1:1, 2 % dropped", dropped, true),
        ("source, 10 children per parent", ten, true),
        ("source, 1 per 1000", sparse, true),
        ("key, 20 CAN ids", keys, false),
        ("key, random u64", wide, false),
    ]
}

const RELATIONS: &[RelationDef<'static>] = &[
    RelationDef {
        name: "source",
        target: Some(RelationTarget {
            run: [1; 16],
            track: TrackId::new(7),
        }),
        values: RelationValues::Stored,
        monotone: true,
        index: RelationIndex::Range,
    },
    RelationDef {
        name: "key",
        target: None,
        values: RelationValues::Stored,
        monotone: false,
        index: RelationIndex::None,
    },
];

/// Track `index` on `clock`, keyed by its index, with `relations`.
fn track(
    index: u32,
    clock: ClockNum,
    relations: &'static [RelationDef<'static>],
) -> TrackSpec<'static> {
    TrackSpec {
        key: track_key(0xc5, &[Segment::Index(index)]),
        clock,
        dimensions: RefVec::Slice { slice: &[] },
        items: ww_tape::Items::WireWeaver {
            api: FullVersion::new("bench_api", Version::new(0, 1, 0)),
            bundle: RefVec::Slice { slice: &[] },
            path: RefVec::Slice { slice: &[] },
            keys: RefVec::Slice { slice: &[] },
            item: None,
        },
        label: "bench",
        repeat: ww_tape::TrackRepeat::Stream,
        kind: TrackKind::Events(EventsDef::INSTANTS),
        relations: RefVec::Slice { slice: relations },
        owner: [0; 32],
    }
}

fn main() -> anyhow::Result<()> {
    let args: Args = ww_tape_bench::args();
    let mut bench = Bench::start(
        "relations",
        vc_build::build_info!(),
        &args.common,
        vec![
            ("rounds", args.rounds.to_string()),
            ("chunks", args.chunks.to_string()),
        ],
    )?;
    println!("{}", bench.header());
    let mut columns = bench.table(
        "columns",
        &format!("Relation columns of {N} items."),
        &[
            "Column",
            "Codec",
            "Bytes per item",
            "Encode ns per item",
            "Get ns per item",
            "Lower bound ns",
        ],
    );
    let starts: Vec<i64> = (0..N as i64).map(|i| i * 1000).collect();
    let times = EncodedTimes::encode(&starts, None, None)?;
    for (what, values, monotone) in shapes() {
        let enc = EncodedRelation::encode(0, &values, monotone)?;
        let (spec, bytes) = enc.column();
        let encode = ns_per(args.rounds, 64, |_| {
            EncodedRelation::encode(0, &values, monotone)
                .map(|e| e.column().1.len() as u64)
                .unwrap_or(0)
        }) / N as f64;
        let mut cols: Vec<_> = times.columns().collect();
        cols.push(enc.column());
        let specs: Vec<_> = cols.iter().map(|c| c.0).collect();
        let head = times
            .head
            .chunk_head(TrackId::new(1), TrackNum::new(1), ItemNum::new(0));
        let mut buf = vec![0u8; ww_tape::chunk::chunk_len(&head, &specs, 0)];
        let id = ww_tape::BlockId::new(ww_tape::SessionTag::writable(1), 1);
        let len = ww_tape::chunk::write_chunk(&mut buf, id, &head, &cols, 0)
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let block = Block::read(&buf[..len]).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let chunk = block.chunk().ok_or_else(|| anyhow::anyhow!("a chunk"))?;
        let column = RelationColumn::read(
            &chunk,
            StoredRelations {
                stored: RelationSet::single(0),
                monotone: RelationSet::from_bits(u16::from(monotone)),
            },
            0,
        )?
        .ok_or_else(|| anyhow::anyhow!("its column"))?;
        let get = ns_per(args.rounds, 64, |_| {
            (0..N).map(|i| column.get(i)).fold(0, u64::wrapping_add)
        }) / N as f64;
        let mut r = Rng(9);
        let probes: Vec<u64> = (0..1024)
            .map(|_| values[r.below(N as u64) as usize])
            .collect();
        let lower = match column.lower_bound(0) {
            Some(_) => Cell::num(
                ns_per(args.rounds, probes.len(), |i| {
                    column.lower_bound(probes[i]).unwrap_or(0) as u64
                }),
                0,
            ),
            None => Cell::text("not monotone"),
        };
        columns.row(vec![
            Cell::text(what),
            Cell::text(spec.codec.0.to_string()),
            Cell::num(bytes.len() as f64 / N as f64, 3),
            Cell::num(encode, 2),
            Cell::num(get, 2),
            lower,
        ]);
    }
    println!("{}", bench.push(columns));

    let mut writes = bench.table(
        "write",
        &format!("`write_chunk` of an events chunk of {N} instants."),
        &["Chunk", "ns per chunk"],
    );
    let shapes = shapes();
    // a monotone relation never decreases across a track's chunks: chunk k's sources follow chunk k - 1's
    let span = shapes[2].1.iter().max().map_or(1, |m| m + 1);
    let sources = (0..u64::from(args.chunks))
        .map(|k| {
            let values: Vec<u64> = shapes[2].1.iter().map(|v| v + k * span).collect();
            EncodedRelation::encode(0, &values, true)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let key = EncodedRelation::encode(1, &shapes[4].1, false)?;
    for (what, with) in [
        ("without relations", false),
        ("with a source (range) and a key column", true),
    ] {
        let d = |e: ww_tape::DescribeError| anyhow::anyhow!("{e:?}");
        let mut defs = Definitions::new();
        let clock = defs
            .define_clock(ClockSpec {
                key: clock_key(0xc5, &[], 1),
                rate: Magnitude::pow10(9).into(),
                resolution: Magnitude::pow10(-9).into(),
                origin: ClockOrigin::Boot,
                label: "bench",
            })
            .map_err(d)?;
        let spec = track(1, clock, if with { RELATIONS } else { &[] });
        let id = spec.id();
        let number = defs.define_track(spec).map_err(d)?;
        let mut best_ns = f64::MAX;
        for _ in 0..args.rounds {
            let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
            let t = Instant::now();
            for (k, source) in (0..u64::from(args.chunks)).zip(&sources) {
                let head = times
                    .head
                    .chunk_head(id, number, ItemNum::new(k * N as u64));
                let mut cols: Vec<_> = times.columns().collect();
                if with {
                    cols.extend([source.column(), key.column()]);
                }
                w.write_chunk(&defs, &head, &cols)?;
            }
            best_ns = best_ns.min(t.elapsed().as_nanos() as f64 / f64::from(args.chunks));
            w.close()?;
        }
        writes.row(vec![Cell::text(what), Cell::num(best_ns, 0)]);
    }
    println!("{}", bench.push(writes));
    bench.finish()
}
