//! The RF capture in two chains, headless: `example_rf DIR [K]` writes the minute at 1/K of the rate (100 by
//! default) into the chains `iq` and `decoded` under `DIR` while a reader follows both, and prints what each holds.
//!
//! ```text
//! radio thread   -> mux of iq      -> DIR/iq/: the samples
//!      | the samples of each chunk
//!      v
//! decoder thread -> mux of decoded -> DIR/decoded/: the bursts and each second's power
//!
//! reader thread: follows DIR/iq and DIR/decoded while they are written, merges them by time
//! ```
#![allow(
    clippy::print_stdout,
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::arithmetic_side_effects,
    reason = "the binary prints the capture's numbers, computed freely from sizes far below 2^52"
)]

use std::path::{Path, PathBuf};

use example_capture::rf::Rf;
use example_rf::chains::{Chains, ChainsError, Decoded, Followed, Written, follow};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (dir, rf) = args()?;
    let chains = Chains::new(rf);
    let open = chains.open(&dir)?;
    let (written, followed) = std::thread::scope(|s| {
        let reader = s.spawn(|| follow(&rf, &Chains::iq_dir(&dir), &Chains::decoded_dir(&dir)));
        let written = chains.record(open)?;
        let followed = reader
            .join()
            .map_err(|_panic| ChainsError::Thread("the reader"))??;
        Ok::<_, ChainsError>((written, followed))
    })?;
    report(&rf, &dir, &written, &followed);
    Ok(())
}

/// The directory of the chains and the capture: the minute at 1/K of the rate.
fn args() -> Result<(PathBuf, Rf), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let dir = args.next().ok_or("usage: example_rf DIR [K]")?.into();
    let k = match args.next() {
        Some(k) => k.parse()?,
        None => 100,
    };
    Ok((dir, Rf::scaled(k).ok_or("K divides 200 000")?))
}

/// What each chain holds, what the decoder found and what the reader merged.
fn report(rf: &Rf, dir: &Path, written: &Written, followed: &Followed) {
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
}
