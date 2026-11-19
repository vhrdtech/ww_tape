//! What track relations cost in events chunks of 4096 items: the bytes per item of a relation column for the shapes
//! of a decoder's source ordinal and of keys, encoding it (`EncodedRelation::encode`), reading every value
//! (`RelationColumn::get`) and a lower bound in it, and a writer's `write_chunk` of an events chunk with and without
//! two relation columns (the column check and the range the index entry takes included). Prints Markdown tables, the
//! best of `--rounds` rounds, with the load average at the start.

use clap::Parser;
use shrink_wrap::prelude::*;
use std::hint::black_box;
use std::time::Instant;
use ww_tape::times::{EncodedRelation, EncodedTimes, RelationColumn, StoredRelations};
use ww_tape::{
    Block, ClockDef, ClockOrigin, Definitions, Describe, EventsDef, FullVersion, Magnitude,
    MemSink, RelationDef, RelationIndex, RelationTarget, RelationValues, Segment, SyncPolicy,
    TapeWriter, TrackDef, TrackKind, Version, clock_key, track_key,
};

#[derive(Parser)]
#[command(about = "ww_tape track relations: column bytes, encode, read, lower bound, write_chunk")]
struct Args {
    /// Rounds, the best one counts.
    #[arg(long, default_value_t = 5)]
    rounds: u32,
    /// Chunks per round of the writer rows.
    #[arg(long, default_value_t = 2000)]
    chunks: u32,
}

const N: usize = 4096;
const CLOCK: u32 = 1;

/// xorshift64*.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) % n
    }
}

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

/// Best of `rounds`, ns per call of `f` over `iters` calls.
fn best(rounds: u32, iters: usize, mut f: impl FnMut(usize) -> u64) -> f64 {
    let mut best = f64::MAX;
    for _ in 0..rounds {
        let t = Instant::now();
        let mut acc = 0u64;
        for i in 0..iters {
            acc = acc.wrapping_add(f(i));
        }
        black_box(acc);
        best = best.min(t.elapsed().as_nanos() as f64 / iters as f64);
    }
    best
}

const RELATIONS: &[RelationDef<'static>] = &[
    RelationDef {
        name: "source",
        target: Some(RelationTarget {
            run: [1; 16],
            track: 7,
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

fn track(number: u32, relations: &'static [RelationDef<'static>]) -> TrackDef<'static> {
    TrackDef {
        number: UNib32(number),
        key: track_key(0xc5, &[Segment::Index(number)]),
        clock: UNib32(CLOCK),
        dimensions: RefVec::Slice { slice: &[] },
        api: FullVersion::new("bench_api", Version::new(0, 1, 0)),
        bundle: RefVec::Slice { slice: &[] },
        path: RefVec::Slice { slice: &[] },
        keys: RefVec::Slice { slice: &[] },
        item: None,
        label: "bench",
        repeat: ww_tape::TrackRepeat::Stream,
        kind: TrackKind::Events(EventsDef::INSTANTS),
        relations: RefVec::Slice { slice: relations },
        owner: [0; 32],
    }
}

fn load() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .unwrap_or_default()
        .split_whitespace()
        .take(3)
        .collect::<Vec<_>>()
        .join(" ")
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    println!("load average at start: {}\n", load());
    println!(
        "| Column, {N} items | codec | bytes per item | encode ns per item | get ns per item | lower bound ns |"
    );
    println!("|---|---:|---:|---:|---:|---:|");
    let starts: Vec<i64> = (0..N as i64).map(|i| i * 1000).collect();
    let times = EncodedTimes::encode(&starts, None, None)?;
    for (what, values, monotone) in shapes() {
        let enc = EncodedRelation::encode(0, &values, monotone)?;
        let (spec, bytes) = enc.column();
        let encode = best(args.rounds, 64, |_| {
            EncodedRelation::encode(0, &values, monotone)
                .map(|e| e.column().1.len() as u64)
                .unwrap_or(0)
        }) / N as f64;
        let mut cols: Vec<_> = times.columns().collect();
        cols.push(enc.column());
        let specs: Vec<_> = cols.iter().map(|c| c.0).collect();
        let head = times.head.chunk_head(1, 1, 0);
        let mut buf = vec![0u8; ww_tape::chunk::chunk_len(&head, &specs, 0)];
        let id = ww_tape::BlockId::new(ww_tape::SessionTag::writable(1), 1);
        let len = ww_tape::chunk::write_chunk(&mut buf, id, &head, &cols, 0)
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let block = Block::read(&buf[..len]).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let chunk = block.chunk().ok_or_else(|| anyhow::anyhow!("a chunk"))?;
        let column = RelationColumn::read(
            &chunk,
            StoredRelations {
                stored: 1,
                monotone: u16::from(monotone),
            },
            0,
        )?
        .ok_or_else(|| anyhow::anyhow!("its column"))?;
        let get = best(args.rounds, 64, |_| {
            (0..N).map(|i| column.get(i)).fold(0, u64::wrapping_add)
        }) / N as f64;
        let mut r = Rng(9);
        let probes: Vec<u64> = (0..1024)
            .map(|_| values[r.below(N as u64) as usize])
            .collect();
        let lower = match column.lower_bound(0) {
            Some(_) => format!(
                "{:.0}",
                best(
                    args.rounds,
                    probes.len(),
                    |i| column.lower_bound(probes[i]).unwrap_or(0) as u64
                )
            ),
            None => "n/a".to_owned(),
        };
        println!(
            "| {what} | {} | {:.3} | {encode:.2} | {get:.2} | {lower} |",
            spec.codec.0,
            bytes.len() as f64 / N as f64
        );
    }

    println!("\n| write_chunk, events chunk of {N} instants | ns per chunk |");
    println!("|---|---:|");
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
        let mut defs = Definitions::new();
        defs.define_all(&[
            Describe::Clock(ClockDef {
                number: UNib32(CLOCK),
                key: clock_key(0xc5, &[], 1),
                rate: Magnitude::pow10(9).into(),
                resolution: Magnitude::pow10(-9).into(),
                origin: ClockOrigin::Boot,
                label: "bench",
            }),
            Describe::Track(track(1, if with { RELATIONS } else { &[] })),
        ])
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let id = track(1, if with { RELATIONS } else { &[] }).id();
        let mut best_ns = f64::MAX;
        for _ in 0..args.rounds {
            let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
            let t = Instant::now();
            for (k, source) in (0..u64::from(args.chunks)).zip(&sources) {
                let head = times.head.chunk_head(id, 1, k * N as u64);
                let mut cols: Vec<_> = times.columns().collect();
                if with {
                    cols.extend([source.column(), key.column()]);
                }
                w.write_chunk(&defs, &head, &cols)?;
            }
            best_ns = best_ns.min(t.elapsed().as_nanos() as f64 / f64::from(args.chunks));
            w.close()?;
        }
        println!("| {what} | {best_ns:.0} |");
    }
    Ok(())
}
