//! Bytes an event tree's flag counts cost: for a track of `--items` instants with flags, the summary blocks of every
//! level without counts and with counts of 1 or 8 bits, with 0.1 % of items flagged (bit 0; bits 0 to 7 each at
//! 0.1 % for 8 bits) and with none flagged. Prints Markdown tables: bytes per node per level and in total.

use clap::Parser;
use std::collections::BTreeMap;
use ww_tape::summary::{EventTree, Summary};
use ww_tape::{Block, EventFields};

#[derive(Parser)]
#[command(about = "Bytes per node and per track of an event tree's flag counts")]
struct Args {
    /// Items of the track.
    #[arg(long, default_value_t = 10_000_000)]
    items: u64,
    /// One item in this many has a flag bit set.
    #[arg(long, default_value_t = 1000)]
    one_in: u64,
}

/// xorshift64: the same flags on every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// Bytes and nodes per level.
type Levels = BTreeMap<u8, (u64, u64)>;

/// Bytes and nodes per level of the tree of `items` instants whose flags `flags(i)` gives, counting `k` bits.
fn tree(items: u64, k: u8, mut flags: impl FnMut(u64) -> u32) -> anyhow::Result<Levels> {
    let fields = EventFields {
        flags: true,
        latest_end: false,
        flag_counts: k,
    };
    let mut t = EventTree::new(1, 1, fields, 0);
    let mut levels = Levels::new();
    let mut tally = |blocks: Vec<Vec<u8>>| -> anyhow::Result<()> {
        for b in blocks {
            let s: Summary<'_> = Block::read(&b)
                .map_err(|e| anyhow::anyhow!("{e:?}"))?
                .summary()
                .ok_or_else(|| anyhow::anyhow!("not a summary"))?;
            let e = levels.entry(s.head().level).or_default();
            e.0 += b.len() as u64;
            e.1 += s.node_count() as u64;
        }
        Ok(())
    };
    for i in 0..items {
        let tick = i64::try_from(i)? * 1000;
        t.push(tick, tick, flags(i))
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        if i % (1 << 20) == 0 {
            tally(t.take_blocks())?;
        }
    }
    t.finish().map_err(|e| anyhow::anyhow!("{e:?}"))?;
    tally(t.take_blocks())?;
    Ok(levels)
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let n = args.items;
    let p = args.one_in;
    let none = |_: u64| 0u32;
    let mut r = Rng(0x5eed_f1a6);
    let rare: Vec<u32> = (0..n)
        .map(|_| u32::from(r.next().is_multiple_of(p)))
        .collect();
    let mut r = Rng(0x0dd_f1a6);
    let rare8: Vec<u32> = (0..n)
        .map(|_| {
            (0..8).fold(0u32, |f, j| {
                f | (u32::from(r.next().is_multiple_of(p)) << j)
            })
        })
        .collect();
    let cases: Vec<(&str, Levels)> = vec![
        (
            "no counts, 0.1 % flagged",
            tree(n, 0, |i| rare[i as usize])?,
        ),
        ("1 bit, 0.1 % flagged", tree(n, 1, |i| rare[i as usize])?),
        ("1 bit, none flagged", tree(n, 1, none)?),
        (
            "8 bits, each 0.1 % flagged",
            tree(n, 8, |i| rare8[i as usize])?,
        ),
        ("8 bits, none flagged", tree(n, 8, none)?),
    ];
    let base = &cases[0].1;
    println!(
        "Event tree of {n} instants with flags, one item in {p} flagged; blocks of at most 4096 bytes.\n"
    );
    println!("| Case | Level | Nodes | Bytes | Bytes per node | Count bytes per node |");
    println!("|---|---:|---:|---:|---:|---:|");
    for (name, levels) in &cases {
        for (level, (bytes, nodes)) in levels {
            let (b0, _) = base.get(level).copied().unwrap_or_default();
            println!(
                "| {name} | {level} | {nodes} | {bytes} | {:.2} | {:.3} |",
                *bytes as f64 / *nodes as f64,
                (*bytes as f64 - b0 as f64) / *nodes as f64
            );
        }
    }
    println!(
        "\n| Case | Bytes, all levels | Count bytes, all levels | Count bytes per 10 M items |"
    );
    println!("|---|---:|---:|---:|");
    let total = |l: &Levels| l.values().map(|v| v.0).sum::<u64>();
    for (name, levels) in &cases {
        let extra = total(levels) as f64 - total(base) as f64;
        println!(
            "| {name} | {} | {extra:.0} | {:.0} |",
            total(levels),
            extra * 10_000_000.0 / n as f64
        );
    }
    Ok(())
}
