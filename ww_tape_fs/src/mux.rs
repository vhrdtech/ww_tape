//! Muxes writing chains of piece files in a directory.

use std::path::Path;

use ww_tape::writer::Resumed;
use ww_tape::{ChainOptions, Definitions, OwnerId, TrackStart};
use ww_tape_mux::{MuxError, MuxOptions, TapeMux};

use crate::sink::{Files, FsError};

/// Start chain `chain` of `owner` in `dir` ([crate::chain::create]) and write it from a mux's writing thread.
pub fn create(
    dir: &Path,
    owner: OwnerId,
    chain: u64,
    tracks: &[TrackStart],
    options: ChainOptions,
    mux: MuxOptions,
) -> Result<TapeMux<Files>, MuxError<FsError>> {
    let chain =
        crate::chain::create(dir, owner, chain, tracks, options).map_err(MuxError::Start)?;
    TapeMux::start(chain, Definitions::new(), mux)
}

/// Reopen chain `chain` of `owner` in `dir` after a crash or a close ([crate::chain::resume]) and write on from a
/// mux's writing thread; the definitions number as the resumed piece does, as [TapeMux::resume] says.
pub fn resume(
    dir: &Path,
    owner: OwnerId,
    chain: u64,
    options: ChainOptions,
    mux: MuxOptions,
) -> Result<(TapeMux<Files>, Resumed), MuxError<FsError>> {
    let (chain, resumed) =
        crate::chain::resume(dir, owner, chain, options).map_err(MuxError::Start)?;
    Ok((TapeMux::start_resumed(chain, &resumed, mux)?, resumed))
}
