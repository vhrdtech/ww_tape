//! The wideband receiver, headless: `example_wideband DIR [MS]` captures MS milliseconds of the band at 250 MS/s
//! (10 000 by default, past the first burst) into the memory window and keeps the pieces around each burst under
//! `DIR`.
//!
//! ```text
//! a producer thread per channel
//!   -> one mux
//!   -> the memory window: a chain of pieces in memory
//!   -> a triage thread, finding the bursts from the summary nodes alone
//!   -> DIR/kept-0/: the pieces around each burst, as piece files
//!   -> DIR/results/: the bursts and the pieces kept, a chain of its own
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

use example_capture::files::{self, Files};
use example_capture::window::Follower;
use example_wideband::Source;
use example_wideband::ram::{RfCapture, RfRun, RfRunning};
use ww_tape::{ChainPiece, OwnerId};

/// The owner of the results' chain.
const RESULTS_OWNER: OwnerId = OwnerId([0x5e; 32]);

// --8<-- [start:main]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (dir, ms) = args()?;
    let capture = capture(ms);
    // where the triage keeps: DIR/kept-0/ for the pieces, DIR/results/ for what the analyser finds
    let [kept] = <[Files; 1]>::try_from(files::kept(&dir, 1)?).map_err(|_k| "one kept store")?;
    let results = files::results(&dir, RESULTS_OWNER)?;
    // a producer thread per channel -> one mux -> the memory window -> a triage thread
    let running = capture.start(Source::Live, kept, &results)?;
    // a viewer beside the triage: the pieces the chain begins
    follow(&running)?;
    // the capture ended, the triage read its last piece
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
        .ok_or("usage: example_wideband DIR [MS]")?
        .into();
    let ms = match args.next() {
        Some(ms) => ms.parse()?,
        None => 10_000,
    };
    Ok((dir, ms))
}

/// [RfCapture::full] cut to `ms`: 2 channels of 125 MS/s, pieces of 64 MiB, the piece before and the piece after a
/// burst kept with it.
fn capture(ms: u64) -> RfCapture {
    let full = RfCapture::full();
    RfCapture {
        wb: full.wb.ms(ms),
        ..full
    }
}

/// Follow the memory window while the capture runs, as a viewer would: print each piece the chain begins and the
/// pieces it holds in memory.
fn follow(running: &RfRunning<Files, Files>) -> Result<(), ww_tape::MemError> {
    let mut follower = Follower::default();
    while !running.is_finished() {
        for cut in follower.next(running.chains(), Duration::from_millis(50))? {
            println!("piece {} begins, {} pieces in memory", cut.piece, cut.held);
        }
    }
    Ok(())
}

/// What the channels pushed, what the window kept and the bursts found.
fn report<K>(capture: &RfCapture, ran: &RfRun, results: &[ChainPiece<K>]) {
    let samples: u64 = ran.recorded.samples.iter().sum();
    println!(
        "{} ms of the band: {samples} samples over {} channels, captured in {:.2} s",
        capture.wb.ms,
        capture.wb.channels,
        ran.capture_time.as_secs_f64()
    );
    let t = &ran.triaged;
    println!(
        "{} pieces, {} kept ({}), {} dropped unread, at most {} in memory",
        t.pieces,
        t.kept.len(),
        example_capture::about(t.kept_bytes),
        t.overrun.len(),
        example_capture::about(t.held_max)
    );
    let bytes: u64 = results.iter().map(|p| p.piece.len).sum();
    println!(
        "{} bursts found; the results take {}",
        ran.bursts.len(),
        example_capture::about(bytes)
    );
}
