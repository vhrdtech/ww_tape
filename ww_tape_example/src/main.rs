//! Print the numbers of the drive's story, recorded at a scale: `ww_tape_example [K]`, `K` dividing 1000 (1, the full
//! hour, by default).
#![allow(
    clippy::print_stdout,
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::arithmetic_side_effects,
    reason = "the binary prints the story's numbers, computed freely from sizes far below 2^52"
)]

use ww_tape_example::{Drive, Error, Story};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let k = match std::env::args().nth(1) {
        Some(k) => k.parse()?,
        None => 1,
    };
    let drive = Drive::scaled(k).ok_or(Error::Settings("the scale divides 1000"))?;
    print(&drive.story()?);
    Ok(())
}

fn mib(bytes: u64) -> String {
    format!("{:.2} MiB", bytes as f64 / f64::from(1 << 20))
}

fn print(s: &Story) {
    let total: u64 = s.pieces.iter().sum();
    println!(
        "The drive's run at scale 1/{}: {} minutes.",
        25_000 / s.drive.phase_hz,
        s.drive.minutes
    );
    println!();
    println!("| Number | Value |");
    println!("|---|---:|");
    println!("| pieces | {} |", s.pieces.len());
    for (seq, len) in s.pieces.iter().enumerate() {
        println!("| piece {seq} | {} ({len} bytes) |", mib(*len));
    }
    println!("| recording | {} ({total} bytes) |", mib(total));
    println!("| can0 frames | {} |", s.frames);
    println!(
        "| can0 bytes per frame | {:.2} |",
        s.frame_bytes as f64 / s.frames as f64
    );
    println!("| phase_a samples | {} |", s.samples);
    println!(
        "| phase_a bytes per sample | {:.3} |",
        s.sample_bytes as f64 / s.samples as f64
    );
    let at = s.torn_from_us / 1000;
    println!(
        "| crash | byte {} of the recording, piece {} at {} |",
        s.crash.byte, s.crash.piece, s.crash.offset
    );
    println!(
        "| torn chunk of frames from | {:02}:{:02}.{:03} |",
        at / 60_000,
        at / 1000 % 60,
        at % 1000
    );
    println!(
        "| resume keeps | {} of piece {}; cuts {} bytes |",
        s.kept_bytes,
        s.crash.piece,
        s.crash.offset - s.kept_bytes
    );
    println!("| acts the crash lost, given again | {} |", s.lost_acts);
    println!(
        "| resumed chain has the bytes of the uncrashed one | {} |",
        s.same_bytes
    );
    println!(
        "| fault | minute {}, emergency frame {} |",
        s.fault_minute, s.emergency_frame
    );
}
