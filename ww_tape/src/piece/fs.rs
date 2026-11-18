//! Chains of piece files in a directory.

use std::path::Path;

use super::writer::{ChainOptions, PieceChain};
use super::{OwnerId, TrackStart};
use crate::sink::Files;
use crate::writer::{Resumed, WriteError};

/// Name of piece `seq` in its chain's directory: the sequence number in 8 digits, so names sort in chain order.
pub fn piece_name(seq: u64) -> String {
    format!("{seq:08}.wwt")
}

impl PieceChain<Files> {
    /// Start chain `chain` of `owner` in `dir` (which must exist; with [PieceFiles::One](super::PieceFiles::One) the file to create) with
    /// its first piece, see [Self::create_on].
    pub fn create(
        dir: &Path,
        owner: OwnerId,
        chain: u64,
        tracks: &[TrackStart],
        options: ChainOptions,
    ) -> Result<Self, WriteError> {
        Self::create_on(Files, dir, owner, chain, tracks, options)
    }

    /// Reopen chain `chain` of `owner` in `dir` after a crash, see [Self::resume_on].
    pub fn resume(
        dir: &Path,
        owner: OwnerId,
        chain: u64,
        options: ChainOptions,
    ) -> Result<(Self, Resumed), WriteError> {
        Self::resume_on(Files, dir, owner, chain, options)
    }
}
