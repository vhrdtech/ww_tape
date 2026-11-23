//! Where a case's headless binary keeps what its triage keeps: directories of piece files.
//!
//! Under a directory `DIR`, `DIR/kept-<chain>/` holds the pieces each chain of the memory window keeps, and
//! `DIR/results/` the analysers' results, a chain of its own written by a mux.

use std::path::Path;

use ww_tape::OwnerId;
pub use ww_tape_fs::Files;
use ww_tape_fs::FsError;
use ww_tape_mux::{MuxError, MuxOptions, TapeMux};

/// A directory of piece files per chain of the window, `dir/kept-<chain>`, each created when missing.
///
/// # Errors
///
/// A directory that could not be created.
pub fn kept(dir: &Path, chains: u32) -> std::io::Result<Vec<Files>> {
    (0..chains)
        .map(|chain| {
            let d = dir.join(format!("kept-{chain}"));
            std::fs::create_dir_all(&d)?;
            Ok(Files::new(&d))
        })
        .collect()
}

/// The chain of the analysers' results, `dir/results`, of `owner`, written by a mux of its own with
/// [crate::chain_options].
///
/// # Errors
///
/// A directory that could not be created, or the mux that didn't start.
pub fn results(dir: &Path, owner: OwnerId) -> Result<TapeMux<Files>, ResultsError> {
    let d = dir.join("results");
    std::fs::create_dir_all(&d).map_err(ResultsError::Dir)?;
    ww_tape_fs::mux::create(
        &d,
        owner,
        0,
        &[],
        crate::chain_options(),
        MuxOptions::default(),
    )
    .map_err(ResultsError::Mux)
}

/// Why [results] failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum ResultsError {
    Dir(std::io::Error),
    Mux(MuxError<FsError>),
}

impl core::fmt::Display for ResultsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Dir(e) => write!(f, "the results' directory: {e}"),
            Self::Mux(e) => write!(f, "the results' mux: {e:?}"),
        }
    }
}

impl core::error::Error for ResultsError {}
