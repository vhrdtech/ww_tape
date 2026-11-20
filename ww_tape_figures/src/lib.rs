//! The figures of ww_tape's book pages that show bytes, offsets or numbers, each drawn from blocks [ww_tape] writes
//! and reads back, so a figure cannot drift from the format:
//!
//! - `block.svg`: a records block byte by byte, its header fields, its body and its CRC.
//! - `piece.svg`: a closed piece on the 4 KiB grid, every block to scale, the close padded to the boundary.
//! - `puts.svg`: a state track put before every block of data, and the few puts that write: the first record, its
//!   copy and the record again once the first lies more than `near` behind.
//! - `grids.svg`: series chunks on their grids: chunks of one grid, lost samples, and a gap to a new grid.
//! - `records_vs_chunk.svg`: the drive's first 96 CAN frames as a records block and as an events chunk, to scale.
//! - `clock_map.svg`: the drive's clock map from the board's µs to the host's ns, and a second segment after a step.
//! - `checkpoints.svg`: a closed piece of the drive, its checkpoints pointing back, the close and the tail.
//! - `crash.svg`: an unclosed piece after a crash, its checkpoints pointing back, the scan to the torn block.
//! - `resume.svg`: the piece written whole, cut by the crash, and resumed to the same bytes.
//! - `tape_map.svg`: the drive's five tracks over its hour, and the records blocks and chunks that hold them.
//! - `first_tape.svg`: the file of the drive's first minute, its start and its end to scale.
//! - `prefix_resync.svg`: the drive's last piece with two damaged blocks, walked in Prefix and in Resync.
//! - `loss_cases.svg`: the drive's last piece with three kinds of damage, and what still reads of each.
//!
//! The figures of records against chunks, the clock map, the checkpoints, the crash, the map, the first tape and the damage draw the book's running example, the drive's recording ([ww_tape_example]), at a small scale.
//!
//! [figures] draws them; `ww_tape_figures DIR` writes them into `DIR` (`just book-figures`: `docs/plots`).
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "the figures draw blocks ww_tape wrote itself: coordinates are computed freely and a broken figure fails its test"
)]

mod block;
mod checkpoints;
mod clock_map;
mod crash;
mod damage;
mod defs;
mod first_tape;
mod grids;
mod piece;
mod puts;
mod records_vs_chunk;
mod svg;
mod tape_map;

/// Every figure: its file name and its SVG text.
pub fn figures() -> anyhow::Result<Vec<(&'static str, String)>> {
    Ok(vec![
        ("block.svg", block::block()?),
        ("piece.svg", piece::piece()?),
        ("puts.svg", puts::puts()?),
        ("grids.svg", grids::grids()?),
        (
            "records_vs_chunk.svg",
            records_vs_chunk::records_vs_chunk()?,
        ),
        ("clock_map.svg", clock_map::clock_map()?),
        ("checkpoints.svg", checkpoints::checkpoints()?),
        ("crash.svg", crash::crash_svg()?),
        ("resume.svg", crash::resume_svg()?),
        ("tape_map.svg", tape_map::tape_map()?),
        ("first_tape.svg", first_tape::first_tape()?),
        ("prefix_resync.svg", damage::prefix_resync()?),
        ("loss_cases.svg", damage::loss_cases()?),
    ])
}

/// `ww_tape`'s errors that are not [std::error::Error], as [anyhow::Error] by their [Debug](core::fmt::Debug) text.
trait OrDebug<T> {
    fn or_debug(self) -> anyhow::Result<T>;
}

impl<T, E: core::fmt::Debug> OrDebug<T> for Result<T, E> {
    fn or_debug(self) -> anyhow::Result<T> {
        self.map_err(|e| anyhow::anyhow!("{e:?}"))
    }
}
