//! Chains of piece files in a directory.

use std::path::Path;

use ww_tape::writer::{Resumed, WriteError};
use ww_tape::{ChainOptions, OwnerId, PieceChain, PieceTapes, TrackStart};

use crate::sink::{Files, FsError, none_at};

/// Name of piece `seq` in its chain's directory: the sequence number in 8 digits, so names sort in chain order.
pub fn piece_name(seq: u64) -> String {
    format!("{seq:08}.wwt")
}

/// Start chain `chain` of `owner` in `dir` (which must exist; with [PieceTapes::One] the file to create) with its
/// first piece, see [PieceChain::create_on].
pub fn create(
    dir: &Path,
    owner: OwnerId,
    chain: u64,
    tracks: &[TrackStart],
    options: ChainOptions,
) -> Result<PieceChain<Files>, WriteError<FsError>> {
    PieceChain::create_on(files_at(dir, &options), owner, chain, tracks, options)
}

/// Reopen chain `chain` of `owner` in `dir` (with [PieceTapes::One] the file) after a crash, see
/// [PieceChain::resume_on].
///
/// A chain that isn't there, `dir` missing or without a first piece (nor its `.partial`), is [FsError::NoPiece] naming
/// `dir`, told apart from a failure.
pub fn resume(
    dir: &Path,
    owner: OwnerId,
    chain: u64,
    options: ChainOptions,
) -> Result<(PieceChain<Files>, Resumed), WriteError<FsError>> {
    let files = files_at(dir, &options);
    if none_at(&files.path_of(0)) {
        return Err(WriteError::Sink {
            what: "opening a chain to resume",
            error: FsError::NoPiece(dir.to_path_buf()),
        });
    }
    PieceChain::resume_on(files, owner, chain, options)
}

/// The piece files of a chain at `path`: a directory, or with [PieceTapes::One] the file.
fn files_at(path: &Path, options: &ChainOptions) -> Files {
    match options.tapes {
        PieceTapes::Each => Files::new(path),
        PieceTapes::One => Files::file(path),
    }
}
