//! The protocol analyser, headless: `example_ethernet DIR [MS]` captures MS milliseconds of the 10 Gbps link (1000 by
//! default) into the memory window and keeps the pieces around each TCP reset under `DIR`.
//!
//! ```text
//! capture thread + decoder thread per receive queue
//!   -> a mux per port
//!   -> the memory window: a chain of pieces in memory per port
//!   -> a triage thread per chain, finding the TCP resets
//!   -> DIR/kept-<chain>/: the pieces around each reset, as piece files
//!   -> DIR/results/: the resets and the pieces kept, a chain of its own
//! ```
#![allow(
    clippy::print_stdout,
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::arithmetic_side_effects,
    reason = "the binary prints the capture's numbers, computed freely from sizes far below 2^52"
)]

use std::path::PathBuf;
use std::time::Duration;

use example_capture::files;
use example_capture::files::Files;
use example_capture::window::Follower;
use example_ethernet::Link;
use example_ethernet::ram::{RamCapture, RamRun, RamRunning};
use ww_tape::{ChainPiece, OwnerId};

/// The owner of the results' chain.
const RESULTS_OWNER: OwnerId = OwnerId([0xe8; 32]);

// --8<-- [start:main]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (dir, ms) = args()?;
    let capture = capture(ms);
    // where the triage keeps: DIR/kept-<chain>/ per chain, DIR/results/ for what the analysers find
    let kept = files::kept(&dir, capture.chains)?;
    let results = files::results(&dir, RESULTS_OWNER)?;
    // capture and decoder threads -> a mux per port -> the memory window -> a triage thread per chain
    let running = capture.start(kept, &results)?;
    // a viewer beside the triage: the pieces each chain begins
    follow(&running)?;
    // the capture ended, every triage read its last piece
    let ran = running.join()?;
    let closed = results.close()?;
    report(&capture, &ran, &closed);
    Ok(())
}
// --8<-- [end:main]

/// The directory to keep into and the milliseconds to capture.
fn args() -> Result<(PathBuf, u64), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let dir = args
        .next()
        .ok_or("usage: example_ethernet DIR [MS]")?
        .into();
    let ms = match args.next() {
        Some(ms) => ms.parse()?,
        None => 1_000,
    };
    Ok((dir, ms))
}

/// [RamCapture::full] cut to `ms`: four receive queues a port, a chain per port, pieces of 64 MiB, the piece before
/// and the piece after a reset kept with it.
fn capture(ms: u64) -> RamCapture {
    let full = RamCapture::full();
    RamCapture {
        link: Link {
            micros: ms * 1_000,
            ..full.link
        },
        ..full
    }
}

/// Follow the memory window while the capture runs, as a viewer would: print each piece a chain begins and the
/// pieces it holds in memory.
fn follow(running: &RamRunning<Files, Files>) -> Result<(), ww_tape::MemError> {
    let mut follower = Follower::default();
    while !running.is_finished() {
        for cut in follower.next(running.chains(), Duration::from_millis(50))? {
            println!(
                "chain {}: piece {} begins, {} pieces in memory",
                cut.chain, cut.piece, cut.held
            );
        }
    }
    Ok(())
}

/// What the capture sent, what each chain's window kept and the resets found.
fn report<K>(capture: &RamCapture, ran: &RamRun, results: &[ChainPiece<K>]) {
    let sent: u64 = ran.captured.ports.iter().map(|p| p.frames).sum();
    println!(
        "{} ms of the link: {sent} frames over {} receive queues, captured in {:.2} s",
        capture.link.micros / 1_000,
        2 * capture.link.queues,
        ran.capture_time.as_secs_f64()
    );
    for (c, t) in ran.triaged.iter().enumerate() {
        println!(
            "chain {c}: {} pieces, {} kept ({}), {} dropped unread, at most {} in memory",
            t.pieces,
            t.kept.len(),
            example_capture::about(t.kept_bytes),
            t.overrun.len(),
            example_capture::about(t.held_max)
        );
    }
    let bytes: u64 = results.iter().map(|p| p.piece.len).sum();
    println!(
        "{} TCP resets found; the results take {}",
        ran.resets.len(),
        example_capture::about(bytes)
    );
}
