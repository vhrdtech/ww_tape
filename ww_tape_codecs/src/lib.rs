//! The column codecs of ww_tape: values to column bytes and back, with no notion of a block, a chunk, a track or a
//! piece.
//!
//! - [Codec]: the ids of how columns are coded, each defined once.
//! - [bitpack]: bit widths, frame-of-reference groups and their vector kernels.
//! - [crc32c]: the CRC-32C of every block, in hardware where the CPU has it.
//! - [from_file]: numbers read from file bytes, [FromFile], reaching a length or a loop only through a bound.
//! - [varint] and [bytes]: variable-length integers and little-endian fields.
//! - [lz4]: a column's bytes as one lz4 block.
//! - `kernel` (with `std`): single-pass statistics of integer samples for series summaries.
//!
//! `ww_tape` re-exports these modules under its own paths; the format and its caps are documented there.

#![cfg_attr(not(feature = "std"), no_std)]
#![deny(unsafe_code)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::unreachable,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::as_conversions,
        clippy::cast_lossless,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss,
        reason = "tests fail by panicking and compute expected values freely"
    )
)]

pub mod bitpack;
pub mod bytes;
pub mod codec;
pub use codec::Codec;
pub mod crc32c;
pub mod from_file;
pub use from_file::{BoundError, FromFile};
#[cfg(feature = "std")]
pub mod kernel;
pub mod lz4;
pub mod samples;
pub mod varint;
