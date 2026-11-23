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
//! - `index_select.svg`: the index entries of two tracks of the drive's last piece, and those a query keeps.
//! - `summary_tree.svg`: the drive's phase current as summary nodes of two levels and a viewer's columns.
//! - `autorange.svg`: an autoranging meter's chunks on one grid at two scales, and its summary nodes.
//! - `prefix_resync.svg`: the drive's last piece with two damaged blocks, walked in Prefix and in Resync.
//! - `loss_cases.svg`: the drive's last piece with three kinds of damage, and what still reads of each.
//! - `deferred.svg`: one stretch unaligned and two aligned, the summary blocks and the zeros before each chunk payload.
//! - `linear_group.svg`: a group of the drive's frame starts, its line and residuals.
//! - `delta_group.svg`: a group of the drive's phase current, its differences and their width.
//! - `sync_loss.svg`: three seconds of the drive's file, the syncs of four policies, what a crash loses.
//! - `cut.svg`: the end of the drive's piece 0 and the start of piece 1.
//! - `chunk_head.svg`, `index_entry.svg`, `tail.svg`: the format reference's layouts, byte by byte, from the drive's
//!   first piece: the head of a `phase_a` chunk, its entry in the final index, and the piece's tail.
//! - `ethernet_sizes.txt`: what each track of the protocol analyser takes, from `ww_tape_example::ethernet`.
//!
//! - `sample_codecs.svg`, `event_times.svg`, `chunk_cost.svg`, `drive_sizes.svg` and the `bench_*.txt` tables: the
//!   numbers of "Sizes and speeds", "The definitions a new piece carries" and the two case pages, from the bench
//!   reports in `docs/bench/` (`just bench-book`).
//!
//! The figures of records against chunks, the clock map, the checkpoints, the crash, the map, the first tape, the index query, the summary tree and the damage draw the book's running example, the drive's recording ([ww_tape_example]), at a small scale.
//!
//! [figures] draws them; `ww_tape_figures DIR` writes them into `DIR` (`just book-figures`: `docs/plots`).
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    reason = "the figures draw blocks ww_tape wrote itself: coordinates are computed freely and a broken figure fails its test"
)]

mod autorange;
mod bench;
mod block;
mod cases;
mod checkpoints;
mod clock_map;
mod crash;
mod damage;
mod defs;
mod first_tape;
mod grids;
mod index_select;
mod layout;
mod layouts;
mod piece;
mod puts;
mod records_vs_chunk;
mod summary_tree;
mod svg;
mod tape_map;
mod writing;

pub use bench::Says;

/// Where the bench reports the charts draw from are kept: `docs/bench` of this repository.
pub const BENCH_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../docs/bench");

/// What the prose of "Sizes and speeds" may write, from the bench reports in [BENCH_DIR].
pub fn bench_says() -> anyhow::Result<Says> {
    bench::says(&bench::read(std::path::Path::new(BENCH_DIR))?)
}

/// What the prose of "Writing from many threads" writes from the handles report in [BENCH_DIR]: the pins it lacks
/// in `page`.
pub fn handles_missing(page: &str) -> anyhow::Result<Vec<String>> {
    let pins = bench::handles(&bench::read(std::path::Path::new(BENCH_DIR))?)?;
    Ok(bench::missing(&pins, page))
}

/// What the prose of "When bytes go bad" writes from the read_check report in [BENCH_DIR]: the pins it lacks in
/// `page`.
pub fn read_check_missing(page: &str) -> anyhow::Result<Vec<String>> {
    let pins = bench::read_check(&bench::read(std::path::Path::new(BENCH_DIR))?)?;
    Ok(bench::missing(&pins, page))
}

/// Every figure: its file name and its text.
pub fn figures() -> anyhow::Result<Vec<(String, String)>> {
    let drawn: Vec<(&str, String)> = vec![
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
        ("index_select.svg", index_select::index_select()?),
        ("summary_tree.svg", summary_tree::summary_tree()?),
        ("autorange.svg", autorange::autorange()?),
        ("prefix_resync.svg", damage::prefix_resync()?),
        ("loss_cases.svg", damage::loss_cases()?),
        ("deferred.svg", writing::deferred()?),
        ("linear_group.svg", writing::linear_group()?),
        ("delta_group.svg", writing::delta_group()?),
        ("sync_loss.svg", writing::sync_loss()?),
        ("cut.svg", writing::cut()?),
        ("chunk_head.svg", layouts::chunk_head()?),
        ("index_entry.svg", layouts::index_entry()?),
        ("tail.svg", layouts::tail()?),
        ("ethernet_sizes.txt", cases::ethernet_sizes()?),
    ];
    let mut out: Vec<(String, String)> =
        drawn.into_iter().map(|(n, s)| (n.to_owned(), s)).collect();
    out.extend(bench::figures(std::path::Path::new(BENCH_DIR))?);
    Ok(out)
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
