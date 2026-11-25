//! Tracks whose definitions the mux puts without data ([TapeMux::put_definitions](crate::TapeMux::put_definitions)),
//! and puts again in each piece a cut starts: a cut carries the definitions of the tracks with data in the piece
//! before, not of those without.

use std::collections::BTreeSet;

use ww_tape::{Definitions, PieceChain, PieceStore, TrackNum, WriteError};

/// The tracks put without data, and the piece the chain wrote when they were last put.
///
/// Every track held is one the definitions hold ([ww_tape::TapeWriter::put_definitions] refuses any other), so
/// the set is no larger than the definitions.
pub(crate) struct HeldDefs {
    seq: u64,
    tracks: BTreeSet<TrackNum>,
}

impl HeldDefs {
    pub(crate) fn new(seq: u64) -> Self {
        Self {
            seq,
            tracks: BTreeSet::new(),
        }
    }

    /// Put the definitions of `tracks` ([PieceChain::put_definitions]) and hold the tracks for later pieces. A put
    /// that cuts first leaves the held tracks to [Self::carry], after it.
    pub(crate) fn put<T: PieceStore>(
        &mut self,
        chain: &mut PieceChain<T>,
        defs: &Definitions,
        tracks: &[TrackNum],
    ) -> Result<(), WriteError<T::Error>> {
        chain.put_definitions(defs, tracks)?;
        self.tracks.extend(tracks);
        Ok(())
    }

    /// After a unit: in a piece a cut started since, put the definitions of the held tracks, never cutting; the puts
    /// refused (a piece without room for them, the next cut tries again). A failure ([crate::thread::stops]) ends it.
    pub(crate) fn carry<T: PieceStore>(
        &mut self,
        chain: &mut PieceChain<T>,
        defs: &Definitions,
    ) -> Result<u64, WriteError<T::Error>> {
        if chain.seq() == self.seq {
            return Ok(0);
        }
        self.seq = chain.seq();
        if self.tracks.is_empty() {
            return Ok(0);
        }
        let tracks: Vec<TrackNum> = self.tracks.iter().copied().collect();
        match chain.writer()?.put_definitions(defs, &tracks) {
            Err(e) if crate::thread::stops(chain, &e) => Err(e),
            Err(_) => Ok(1),
            Ok(_) => Ok(0),
        }
    }
}
