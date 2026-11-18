//! The longest a reader of a memory piece waits while the piece is written: a writer appends `--piece-mib` MiB in
//! 64 KiB blocks to a `MemSink`, a reader reads 4 KiB at the start of it every few µs and times each `read_at`.
//! Prints a Markdown table of the worst and the 99.9th percentile wait, with and without `TapeSink::reserve` of the
//! piece's size up front (what `PieceChain` does for a `MemTapes` piece), the worst of `--rounds` rounds each, with
//! the load average at the start.

use clap::Parser;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use ww_tape::reader::ByteSource;
use ww_tape::{MemSink, TapeSink};

#[derive(Parser)]
#[command(
    about = "worst wait of a reader of a memory piece while it is written, with and without reserving"
)]
struct Args {
    /// Size of the piece, MiB.
    #[arg(long, default_value_t = 256)]
    piece_mib: usize,
    /// Rounds each; the worst is kept.
    #[arg(long, default_value_t = 5)]
    rounds: u32,
}

/// (worst, p99.9) of the reader's waits, in the order of the writer's progress.
fn round(piece_mib: usize, reserve: bool) -> (Duration, Duration, usize) {
    let mut sink = MemSink::new();
    let tape = sink.tape();
    if reserve {
        sink.reserve(((piece_mib + 1) << 20) as u64);
    }
    let done = AtomicBool::new(false);
    let mut waits: Vec<Duration> = Vec::new();
    std::thread::scope(|s| {
        let reader = s.spawn(|| {
            let mut buf = [0u8; 4096];
            let mut waits = Vec::new();
            while tape.size().unwrap() < 4096 {
                std::hint::spin_loop();
            }
            loop {
                let t = Instant::now();
                tape.read_at(0, &mut buf).unwrap();
                waits.push(t.elapsed());
                // between reads the lock is free: a reader never waits for what it does not ask
                std::thread::sleep(Duration::from_micros(2));
                if done.load(Ordering::Acquire) {
                    break;
                }
            }
            waits
        });
        let block = vec![0xA5u8; 64 << 10];
        for _ in 0..piece_mib * 16 {
            sink.write_all(&block).unwrap();
            // a real writer encodes the next chunk between appends: the lock is free then
            std::thread::sleep(Duration::from_micros(100));
        }
        done.store(true, Ordering::Release);
        waits = reader.join().unwrap();
    });
    waits.sort_unstable();
    let p999 = waits[waits.len().saturating_sub(1) * 999 / 1000];
    (waits.last().copied().unwrap_or_default(), p999, waits.len())
}

fn main() {
    let a = Args::parse();
    let load = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
    println!(
        "load average at the start: {}\n",
        load.split_whitespace()
            .take(3)
            .collect::<Vec<_>>()
            .join(" ")
    );
    println!(
        "| Piece {} MiB | worst wait | p99.9 wait | reads (fewest) |\n|---|---|---|---|",
        a.piece_mib
    );
    for (name, reserve) in [("grows by doubling", false), ("reserved", true)] {
        let mut worst = (Duration::ZERO, Duration::ZERO, usize::MAX);
        for _ in 0..a.rounds {
            let r = round(a.piece_mib, reserve);
            worst = (worst.0.max(r.0), worst.1.max(r.1), worst.2.min(r.2));
        }
        println!("| {name} | {:?} | {:?} | {} |", worst.0, worst.1, worst.2);
    }
}
