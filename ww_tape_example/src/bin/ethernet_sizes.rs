//! Print the sizes of the analyser's capture: `ethernet [MS]` records MS milliseconds of the link (10 by default) at
//! its line rate in both directions into one chain in memory, reads it back and prints what each track takes.
#![allow(
    clippy::print_stdout,
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::arithmetic_side_effects,
    reason = "the binary prints the capture's numbers, computed freely from sizes far below 2^52"
)]

use ww_tape_example::ethernet::Link;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let ms: u64 = match std::env::args().nth(1) {
        Some(ms) => ms.parse()?,
        None => 10,
    };
    let sizes = Link::full().micros(ms * 1_000).sizes()?;
    println!("{sizes}");
    Ok(())
}
