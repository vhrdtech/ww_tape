//! The RF capture as one piece. `rf_piece [MS/s]` prints the size of a minute of IQ at a rate (20 by default) in
//! each sample codec: its bytes per sample and its parts. `rf_piece write PATH [K]` writes the minute at 1/K of the
//! rate (1 by default) as one piece into the file `PATH`.
#![allow(
    clippy::print_stdout,
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::arithmetic_side_effects,
    reason = "the binary prints the capture's numbers, computed freely from sizes far below 2^52"
)]

use example_capture::rf::Rf;
use example_rf::{Tally, record_on, write_file};
use ww_tape::Codec;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().nth(1).as_deref() == Some("write") {
        let path = std::env::args()
            .nth(2)
            .ok_or("usage: rf_piece write PATH [K]")?;
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
