//! Verifying and range-hashing every piece of a chain at once: each piece one at a time, or, with the `parallel`
//! cargo feature, pieces processed in parallel over rayon's thread pool.
//!
//! Each piece's [verify](ww_tape::TapeReader::verify) or [range_hash](ww_tape::TapeReader::range_hash) is already
//! independent of every other piece's: reading and hashing its bytes is the costly, data-bound part, and a chunk's
//! unit hash costs the same whichever piece holds it. [verify_each] and [range_hash_each] give one piece's result
//! per path in `paths`, in that order, one piece read and checked at a time. `verify_each_parallel` and
//! `range_hash_each_parallel` (behind the `parallel` feature) give the same results in the same order, with the
//! pieces spread over rayon's threads instead. Measured in `ww_tape_benchmarks`'s `archive_parallel` bench: pieces
//! in parallel over rayon won clearly
//! over one piece at a time, on a 14-core machine with the pieces in the page cache (FS-2); a cold read is bound
//! by the storage underneath instead, where this may help less.
//!
//! [range_hash_each] folds each piece from [TrackHash::START]: it gives each piece's own chunks, not a single hash
//! of the whole chain. [TrackHash::then] folds one chunk's unit hash into a running hash, not two pieces' folded
//! hashes into one, so combining many pieces into the chain's own hash still means folding every chunk in order,
//! once; nothing here replaces that.

use std::ops::Range;
use std::path::Path;

use ww_tape::{
    ItemNum, OwnerId, RangeHash, ReadError, TapeReader, TrackHash, TrackId, Verification,
};

use crate::sink::{FileSource, FsError};

/// [ww_tape::TapeReader::verify] of the one piece at `path`, opened fresh.
fn verify_one(path: &Path) -> Result<Verification, ReadError<FsError>> {
    let src = FileSource::open(path).map_err(ReadError::Source)?;
    let r = TapeReader::open(src)?;
    r.verify(0)
}

/// [TapeReader::verify] of every path in `paths`, one piece at a time, in order.
pub fn verify_each(paths: &[&Path]) -> Vec<Result<Verification, ReadError<FsError>>> {
    paths.iter().map(|p| verify_one(p)).collect()
}

/// [verify_each], pieces verified in parallel over rayon's global thread pool: the same results, in the same
/// order. Turns on with the `parallel` cargo feature.
#[cfg(feature = "parallel")]
pub fn verify_each_parallel(paths: &[&Path]) -> Vec<Result<Verification, ReadError<FsError>>> {
    use rayon::prelude::*;
    paths.par_iter().map(|p| verify_one(p)).collect()
}

/// [ww_tape::TapeReader::range_hash] of the one piece at `path`, opened fresh, folded from [TrackHash::START].
fn range_hash_one(
    path: &Path,
    track_id: TrackId,
    owner: OwnerId,
    items: Range<ItemNum>,
) -> Result<RangeHash, ReadError<FsError>> {
    let src = FileSource::open(path).map_err(ReadError::Source)?;
    let r = TapeReader::open(src)?;
    r.range_hash(track_id, owner, items, TrackHash::START)
}

/// [TapeReader::range_hash] of every path in `paths`, one piece at a time, in order, each folded from
/// [TrackHash::START].
pub fn range_hash_each(
    paths: &[&Path],
    track_id: TrackId,
    owner: OwnerId,
    items: Range<ItemNum>,
) -> Vec<Result<RangeHash, ReadError<FsError>>> {
    paths
        .iter()
        .map(|p| range_hash_one(p, track_id, owner, items.clone()))
        .collect()
}

/// [range_hash_each], pieces hashed in parallel over rayon's global thread pool: the same results, in the same
/// order. Turns on with the `parallel` cargo feature.
#[cfg(feature = "parallel")]
pub fn range_hash_each_parallel(
    paths: &[&Path],
    track_id: TrackId,
    owner: OwnerId,
    items: Range<ItemNum>,
) -> Vec<Result<RangeHash, ReadError<FsError>>> {
    use rayon::prelude::*;
    paths
        .par_iter()
        .map(|p| range_hash_one(p, track_id, owner, items.clone()))
        .collect()
}
