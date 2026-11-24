//! The runs of blocks marked [FLAG_WITH_NEXT] a scan meets: a run is listed whole with the block it belongs with, or
//! dropped.
//!
//! What ends a run decides what a scan does with it:
//!
//! | Run ended by | Its blocks |
//! |---|---|
//! | its last block, unmarked | listed |
//! | a close, a close summary or an index (the writer writes none inside a run) | listed |
//! | the end of the scan, a tail, a piece head or an append head (a crash tore it) | dropped |
//! | a gap or a bad block (damage) | dropped |

use alloc::vec::Vec;

use super::PieceInfo;
use crate::index::IndexEntry;
use crate::{Block, FLAG_WITH_NEXT};

/// A block of a run a scan holds.
struct Held {
    /// Where the block starts and ends in the source
    at: u64,
    end: u64,
    /// Its entries, listed once the run is whole
    entries: Vec<IndexEntry>,
}

/// The run a scan holds.
pub(super) struct Runs {
    held: Vec<Held>,
}

impl Runs {
    pub(super) fn new() -> Self {
        Self { held: Vec::new() }
    }

    /// Take `block` of data or puts, from `at` to `end` in the source and at `in_piece` in the piece: held when marked
    /// [FLAG_WITH_NEXT], else listed with the run it ends.
    pub(super) fn take(
        &mut self,
        piece: &mut PieceInfo,
        (at, end): (u64, u64),
        in_piece: Option<u64>,
        block: &Block<'_>,
    ) {
        let entries = in_piece.map_or_else(Vec::new, |off| IndexEntry::all_of_block(off, block));
        if block.flags & FLAG_WITH_NEXT != 0 {
            self.held.push(Held { at, end, entries });
            return;
        }
        self.list(piece);
        if let Some(list) = piece.entries.as_mut() {
            list.extend(entries);
        }
    }

    /// The run ends whole without a block of data: before a close, a close summary or an index.
    pub(super) fn end_whole(&mut self, piece: &mut PieceInfo) {
        self.list(piece);
    }

    /// The run ends torn: at the end of the scan, a tail, a piece head or damage. Its blocks are dropped.
    pub(super) fn drop_torn(&mut self, piece: &mut PieceInfo) {
        for h in self.held.drain(..) {
            push_dropped(piece, h.at, h.end);
        }
    }

    /// Forget the entries of the block at `gone` in the piece, which a later copy replaces.
    pub(super) fn forget(&mut self, piece: &mut PieceInfo, gone: u64) {
        if let Some(list) = piece.entries.as_mut() {
            list.retain(|e| e.offset != gone);
        }
        for h in &mut self.held {
            h.entries.retain(|e| e.offset != gone);
        }
    }

    /// List the held blocks' entries.
    fn list(&mut self, piece: &mut PieceInfo) {
        for h in self.held.drain(..) {
            if let Some(list) = piece.entries.as_mut() {
                list.extend(h.entries);
            }
        }
    }
}

/// Note the block from `at` to `end` in the source as dropped, with the dropped stretch it goes on.
fn push_dropped(piece: &mut PieceInfo, at: u64, end: u64) {
    let len = end.saturating_sub(at);
    if let Some((start, n)) = piece.dropped_runs.last_mut()
        && start.saturating_add(*n) == at
    {
        *n = n.saturating_add(len);
        return;
    }
    piece.dropped_runs.push((at, len));
}
