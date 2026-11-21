//! Files of ww_tape: a [TapeSink](ww_tape::TapeSink) that writes a file, a [Tapes](ww_tape::Tapes) store of piece
//! files, and writers, chains and muxes opened by path.
//!
//! - [FileSink] writes one file under its final name from the start: a new file of one piece, created exclusively, a
//!   file appended to in place, or a file that holds every piece of a chain. Whether a piece is open, closed or torn
//!   is read from its bytes (its close and tail), never from its name.
//! - [Files] keeps the pieces of a [PieceChain](ww_tape::PieceChain) in files: each piece a file of a directory,
//!   named by its seq ([piece_name]), or every piece in one file.
//! - [FileSource] reads a file back, a closed piece or any tape, as a [ByteSource](ww_tape::ByteSource).
//! - [writer] opens a [TapeWriter](ww_tape::TapeWriter) on a path: [writer::create], [writer::append],
//!   [writer::resume], [writer::cut]. [chain] opens a chain in a directory: [chain::create], [chain::resume].
//! - [mux] opens a chain in a directory written from a [TapeMux](ww_tape_mux::TapeMux)'s writing thread:
//!   [mux::create], [mux::resume].
//!
//! A file sink is the only writer of its file while it lives: it holds the kernel's exclusive lock on the open file
//! (`flock` on unix, a sharing mode that admits no second writer on Windows). A second sink of the same file, under
//! any name and in any process, is refused with [FsError::Locked], so two writers never fork one file. The lock goes
//! with the open file, so a crash leaves nothing behind, and a fork handler keeps it out of a child process forked
//! meanwhile. A file on a network or user-space file system ([SharedFs]), where the lock may not hold, gets a
//! [FsWarning] ([FileSink::warning]); writing is not refused.
//!
//! Everything that names no path stays in `ww_tape`: the sink and store traits, the memory sinks, the writer and the
//! chain on any sink, the readers.

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

pub mod chain;
mod lock;
pub mod mux;
mod sink;
pub mod writer;

pub use chain::piece_name;
pub use lock::{FsWarning, SharedFs};
pub use sink::{FileSink, FileSource, Files, FsError};
