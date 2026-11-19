//! Files of ww_tape: a [TapeSink](ww_tape::TapeSink) that writes a file, a [Tapes](ww_tape::Tapes) store of piece
//! files, and writers and chains opened by path.
//!
//! - [FileSink] writes one file: created as `NAME.partial` ([partial_name]) and renamed to `NAME` at its finish, or a
//!   file appended to in place, or a file that holds every piece of a chain.
//! - [Files] keeps the pieces of a [PieceChain](ww_tape::PieceChain) in files: each piece a file of a directory,
//!   named by its seq ([piece_name]), or every piece in one file.
//! - [FileSource] reads a file back, a finished piece or any tape, as a [ByteSource](ww_tape::ByteSource).
//! - [writer] opens a [TapeWriter](ww_tape::TapeWriter) on a path: [writer::create], [writer::append],
//!   [writer::resume], [writer::cut]. [chain] opens a chain in a directory: [chain::create], [chain::resume].
//!
//! A file sink is the only writer of its file while it lives: it holds an exclusive lock on the lock file
//! `NAME.lock` beside it ([lock_name]), which only writers open. The lock is advisory: other writers of the format
//! take it too. A child process forked meanwhile doesn't hold it. A second sink of the same file, under any name, is
//! refused with [FsError::Locked], so two writers never fork one file. A copy of a file or of a chain's directory
//! made while it is written is not covered by the original's lock: a writer of the copy takes a lock of its own.
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
mod sink;
pub mod writer;

pub use chain::piece_name;
pub use lock::{LOCK_TRIES_MAX, lock_name};
pub use sink::{FileSink, FileSource, Files, FsError, partial_name};
