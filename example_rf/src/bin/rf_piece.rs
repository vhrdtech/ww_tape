//! Print the sizes of the RF capture: a minute of IQ at a rate (`rf [MS/s]`, 20 by default) in each sample codec, as
//! one piece, its bytes per sample and its parts. `rf write PATH [K]` writes the minute at 1/K of the rate (1 by
//! default) as one piece into the file `PATH`. `rf chains DIR [K]` writes the minute at 1/K of the rate (100 by
//! default) into two chains under `DIR` through two muxes while a reader follows both, and prints what each holds.
#![allow(
    clippy::print_stdout,
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::arithmetic_side_effects,
    reason = "the binary prints the capture's numbers, computed freely from sizes far below 2^52"
)]

use example_capture::rf::Rf;
use example_rf::chains::{Chains, Decoded};
use example_rf::{Tally, record_on, write_file};
use ww_tape::Codec;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().nth(1).as_deref() == Some("chains") {
        let dir = std::env::args().nth(2).ok_or("usage: rf chains DIR [K]")?;
        let k = match std::env::args().nth(3) {
            Some(k) => k.parse()?,
            None => 100,
        };
        return chains(std::path::Path::new(&dir), k);
    }
    if std::env::args().nth(1).as_deref() == Some("write") {
        let path = std::env::args().nth(2).ok_or("usage: rf write PATH [K]")?;
        let k = match std::env::args().nth(3) {
            Some(k) => k.parse()?,
            None => 1,
        };
        let rf = Rf::scaled(k).ok_or("K divides 200 000")?;
        let c = write_file(&rf, std::path::Path::new(&path))?;
        println!(
            "{path}: a minute of IQ at {} S/s, {} bytes, {:.3} per sample.",
            rf.rate_hz,
            c.len,
            c.bytes_per_sample()
        );
        return Ok(());
    }
    let msps: u32 = match std::env::args().nth(1) {
        Some(r) => r.parse()?,
        None => 20,
    };
    let rf = Rf::at(msps * 1_000_000);
    println!(
        "A minute of IQ at {msps} MS/s: {} samples of 4 bytes, {} MB raw, chunks of {} samples.\n",
        rf.count(),
        rf.count() * 4 / 1_000_000,
        rf.chunk_samples
    );
    println!(
        "| Codec | Piece, MB | Bytes per sample | Samples, MB | Summaries, KB | Summary blocks |"
    );
    println!("|---|---:|---:|---:|---:|---:|");
    for (name, codec) in [
        ("`RAW`", Codec::RAW),
        ("`PACKED12_LSB`", Codec::PACKED12_LSB),
        ("`DELTA`", Codec::DELTA),
        ("`DELTA2`", Codec::DELTA2),
    ] {
        let c = record_on(&rf.codec(codec), Tally::default())?;
        println!(
            "| {name} | {:.1} | {:.3} | {:.1} | {:.1} | {} |",
            c.len as f64 / 1e6,
            c.bytes_per_sample(),
            c.sample_bytes as f64 / 1e6,
            c.summary_bytes as f64 / 1e3,
            c.summary_blocks
        );
    }
    Ok(())
}

/// Write the minute at 1/`k` of the rate into two chains under `dir` while a reader follows them; print both.
fn chains(dir: &std::path::Path, k: u32) -> Result<(), Box<dyn std::error::Error>> {
    let rf = Rf::scaled(k).ok_or("K divides 200 000")?;
    let w = Chains::new(rf).write_followed(dir)?;
    let (written, followed) = (&w.written, &w.followed);
    let bursts = written
        .decoded
        .iter()
        .filter(|d| matches!(d, Decoded::Burst { .. }))
        .count();
    println!(
        "A minute of IQ at {} S/s in two chains under {}:\n",
        rf.rate_hz,
        dir.display()
    );
    println!("| Chain | Pieces | Bytes |");
    println!("|---|---:|---:|");
    println!("| `iq` | {} | {} |", written.iq_pieces, written.iq_bytes);
    println!(
        "| `decoded` | {} | {} |",
        written.decoded_pieces, written.decoded_bytes
    );
    println!(
        "\nThe decoder found {bursts} bursts and measured {} seconds.",
        written.decoded.len() - bursts
    );
    println!(
        "The reader merged {} chunks of {} samples and {} finds, {} of them late.",
        followed.chunks,
        followed.samples,
        followed.decoded.len(),
        followed.late
    );
    let s = |tick: i64| tick as f64 / 1e9;
    for (d, first) in &followed.bursts {
        if let Decoded::Burst { start, end, power } = d {
            println!(
                "Burst {:.4} s to {:.4} s, power {power}, after the chunk from sample {first} ({:.2} s).",
                s(*start),
                s(*end),
                *first as f64 / f64::from(rf.rate_hz)
            );
        }
    }
    Ok(())
}
