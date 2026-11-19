//! The figures of ww_tape's book pages that show bytes, offsets or numbers, each drawn from blocks [ww_tape] writes
//! and reads back, so a figure cannot drift from the format:
//!
//! - `block.svg`: a records block byte by byte, its header fields, its body and its CRC.
//! - `piece.svg`: a closed piece on the 4 KiB grid, every block to scale, the close padded to the boundary.
//! - `puts.svg`: a state track put before every block of data, and the few puts that write: the first record, its
//!   copy and the record again once the first lies more than `near` behind.
//! - `grids.svg`: series chunks on their grids: chunks of one grid, lost samples, and a gap to a new grid.
//!
//! [figures] draws them; `ww_tape_figures DIR` writes them into `DIR` (`just book-figures`: `docs/plots`).

mod block;
mod defs;
mod grids;
mod piece;
mod puts;
mod svg;

/// Every figure: its file name and its SVG text.
pub fn figures() -> anyhow::Result<Vec<(&'static str, String)>> {
    Ok(vec![
        ("block.svg", block::block()?),
        ("piece.svg", piece::piece()?),
        ("puts.svg", puts::puts()?),
        ("grids.svg", grids::grids()?),
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
