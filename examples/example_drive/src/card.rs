//! The drive's run on a raw SD card, written over an earlier run that the card still holds behind it.

use ww_tape::{FileBlocks, FileError, Medium, OpenOptions, Recovery, TapeReader};

use crate::Error;
use crate::record::{Crash, Crashed, Recording};

/// The chain of the drive's earlier run on the card: another chain of the same board, so other session tags.
pub const EARLIER_CHAIN: u64 = 1;

/// A raw card after the power failed: the run's pieces up to the crash, then the earlier run's bytes behind them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Card {
    /// The card's bytes, as long as the earlier run.
    pub bytes: Vec<u8>,
    /// Where the power failed.
    pub crash: Crash,
}

/// What a reader finds on the card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CardRead {
    /// Pieces a reader of a raw medium finds, and where the data ends.
    pub raw_pieces: usize,
    pub raw_end: u64,
    /// Where the walk of the raw medium stopped and why.
    pub raw_stop: Option<(u64, String)>,
    /// Pieces a reader that takes the card for a file finds, and those of them that are the earlier run's.
    pub file_pieces: usize,
    pub file_earlier: usize,
}

impl Crashed {
    /// The crashed run as a raw card holds it after the power failed, the card having held the recording `earlier`.
    pub fn on_card(&self, earlier: &Recording) -> Result<Card, Error> {
        let crashed = self;
        let mut bytes = crashed.pieces()?.concat();
        let old = earlier.concat();
        let rest = old.get(bytes.len()..).ok_or(Error::Settings(
            "the earlier run fills the card past the crash",
        ))?;
        bytes.extend_from_slice(rest);
        Ok(Card {
            bytes,
            crash: crashed.crash,
        })
    }
}

impl Card {
    /// Read the card as a raw medium and as a file; `earlier` is the run it held before.
    pub fn read(&self, earlier: &Recording) -> Result<CardRead, Error> {
        let raw = TapeReader::open_with(
            &self.bytes[..],
            OpenOptions {
                medium: Medium::Raw,
                ..OpenOptions::default()
            },
        )?;
        let raw_end = raw.pieces().last().map_or(0, |p| p.end);
        let mut walk = FileBlocks::with_recovery(&self.bytes[..], Recovery::Prefix).on(Medium::Raw);
        let mut raw_stop = None;
        while let Some(b) = walk.next_block() {
            if let Err(e @ FileError::Cut { offset, .. }) = b {
                raw_stop = Some((offset, e.to_string()));
            }
        }
        let earlier_ids: Vec<_> = earlier
            .pieces
            .iter()
            .map(|p| piece_id(p))
            .collect::<Result<_, _>>()?;
        let file = TapeReader::open(&self.bytes[..])?;
        let file_earlier = file
            .pieces()
            .iter()
            .filter(|p| {
                p.head
                    .as_ref()
                    .is_some_and(|(h, _)| earlier_ids.contains(&Some(h.ident.id)))
            })
            .count();
        Ok(CardRead {
            raw_pieces: raw.pieces().len(),
            raw_end,
            raw_stop,
            file_pieces: file.pieces().len(),
            file_earlier,
        })
    }
}

/// The id a piece's head names.
fn piece_id(piece: &[u8]) -> Result<Option<ww_tape::PieceId>, Error> {
    let r = TapeReader::open(piece)?;
    Ok(r.pieces()
        .first()
        .and_then(|p| p.head.as_ref())
        .map(|(h, _)| h.ident.id))
}
