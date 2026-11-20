//! What reading an events chunk's starts column costs per item, intact and salvaged: opening it (`EventTimes::read`
//! checks every group; `EventTimes::salvage` decides once which groups hold up), and reading every item one at a time
//! (`Starts::get`, `Starts::is_suspect`) from a salvaged column with one damaged byte. Prints a Markdown table, the
//! best of `--rounds` rounds, with the load average at the start.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::indexing_slicing,
    clippy::print_stdout,
    reason = "a bench reads no file of others: it computes sizes and rates freely, fails by panicking and prints its table"
)]

use clap::Parser;
use std::hint::black_box;
use std::time::Instant;
use ww_tape::chunk::{ChunkHead, chunk_len, write_chunk};
use ww_tape::times::{EncodedTimes, EventTimes};
use ww_tape::{Block, BlockId, Codec, ColumnKind, ColumnSpec, SessionTag};

#[derive(Parser)]
#[command(about = "ww_tape starts column: intact and salvaged open, get and is_suspect per item")]
struct Args {
    /// Rounds, the best one counts.
    #[arg(long, default_value_t = 5)]
    rounds: u32,
    /// Items in the chunk.
    #[arg(long, default_value_t = 1 << 20)]
    items: usize,
}

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

/// An events chunk of `starts` and a small payload column, as a block.
fn block(starts: &[i64]) -> anyhow::Result<Vec<u8>> {
    let enc = EncodedTimes::encode(starts, None, None)?;
    let payload = [0xa5u8; 5];
    let mut cols: Vec<(ColumnSpec, &[u8])> = enc.columns().collect();
    cols.push((
        ColumnSpec {
            kind: ColumnKind::Payloads,
            codec: Codec::RAW,
            param: 0,
            len: payload.len(),
        },
        &payload,
    ));
    let specs: Vec<ColumnSpec> = cols.iter().map(|c| c.0).collect();
    let head: ChunkHead = enc.head.chunk_head(9, 1, 0);
    let mut buf = vec![0u8; chunk_len(&head, &specs, 0)];
    let id = BlockId::new(SessionTag::from_u32(1), 0);
    let len = write_chunk(&mut buf, id, &head, &cols, 0).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    buf.truncate(len);
    Ok(buf)
}

/// Best of `rounds`, ns per item of `f` over `n` items.
fn best(rounds: u32, n: usize, mut f: impl FnMut() -> u64) -> f64 {
    let mut best = f64::MAX;
    for _ in 0..rounds {
        let t = Instant::now();
        black_box(f());
        best = best.min(t.elapsed().as_nanos() as f64 / n as f64);
    }
    best
}

fn load() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|l| l.split_whitespace().next().map(str::to_owned))
        .unwrap_or_else(|| "?".to_owned())
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let n = args.items;
    let mut r = Rng(0xc0de);
    let mut t = 0i64;
    let starts: Vec<i64> = (0..n)
        .map(|_| {
            t += r.below(1000) as i64;
            t
        })
        .collect();
    let good = block(&starts)?;
    let mut bad = good.clone();
    let mid = bad.len() / 2;
    bad[mid] ^= 1;
    let good = Block::read(&good).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let bad = Block::salvage(&bad).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let (good, bad) = (
        good.chunk().ok_or_else(|| anyhow::anyhow!("no chunk"))?,
        bad.chunk().ok_or_else(|| anyhow::anyhow!("no chunk"))?,
    );
    let salvaged = EventTimes::salvage(&bad)?;
    let s = salvaged.starts();
    println!("load {}, {n} items, one damaged byte\n", load());
    println!("| starts column | ns per item |");
    println!("|---|---:|");
    let rows = [
        (
            "`EventTimes::read`, intact",
            best(args.rounds, n, || {
                EventTimes::read(&good).map_or(0, |t| t.len() as u64)
            }),
        ),
        (
            "`EventTimes::salvage`, damaged",
            best(args.rounds, n, || {
                EventTimes::salvage(&bad).map_or(0, |t| t.len() as u64)
            }),
        ),
        (
            "`Starts::get` each item, salvaged",
            best(args.rounds, n, || {
                (0..n).fold(0u64, |a, i| a.wrapping_add(s.get(i) as u64))
            }),
        ),
        (
            "`Starts::is_suspect` each item, salvaged",
            best(args.rounds, n, || {
                (0..n).filter(|i| s.is_suspect(*i)).count() as u64
            }),
        ),
    ];
    for (what, ns) in rows {
        println!("| {what} | {ns:.2} |");
    }
    Ok(())
}
