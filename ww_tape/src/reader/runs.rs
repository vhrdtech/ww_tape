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
//! | a gap or a bad block (damage) | its blocks of a group dropped, its other blocks listed |
//! | its [RUN_BLOCKS_MAX]th block, marked | dropped, with the blocks after it up to its first unmarked one |
//!
//! A group of puts ([TapeWriter::put_group](crate::TapeWriter::put_group)) is taken whole or not at all, damaged too:
//! its blocks are marked [FLAG_GROUP], its first one [FLAG_GROUP_START]. A block that doesn't read hides its own
//! marks, so after damage a scan drops the blocks of a group without a start, up to the first one not marked
//! [FLAG_WITH_NEXT]: they may be the rest of a group the damage hit. A group that starts after the damage is whole.
//! Damage costs no other block: a run of other puts, or of chunks, loses only what doesn't read. Marks that break a
//! group ([crate::group_marks]) drop it as damage does: a writer makes no such blocks.

use alloc::vec::Vec;

use super::PieceInfo;
use crate::group_marks::{Link, Marks, link};
use crate::index::IndexEntry;
use crate::{Block, FLAG_GROUP, FLAG_GROUP_START, FLAG_WITH_NEXT, RUN_BLOCKS_MAX};

/// A block of a run a scan holds.
struct Held {
    /// Where the block starts and ends in the source
    at: u64,
    end: u64,
    /// Whether it is a block of a group ([FLAG_GROUP])
    group: bool,
    /// Its entries, listed once the run is whole
    entries: Vec<IndexEntry>,
}

/// What a scan does with the next blocks.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Mode {
    /// Holds marked blocks until the block they belong with
    Hold,
    /// After damage: drops blocks of a group without a start up to the first unmarked one
    AfterDamage,
    /// After a run longer than [RUN_BLOCKS_MAX]: drops every block up to the first unmarked one
    TooLong,
}

/// The run a scan holds and what it does with the next blocks.
pub(super) struct Runs {
    held: Vec<Held>,
    mode: Mode,
    /// The marks of the block taken last, None after a block of no puts or the run's end
    prev: Option<Marks>,
}

impl Runs {
    pub(super) fn new() -> Self {
        Self {
            held: Vec::new(),
            mode: Mode::Hold,
            prev: None,
        }
    }

    /// Take `block` of data or puts, from `at` to `end` in the source and at `in_piece` in the piece: held when marked
    /// [FLAG_WITH_NEXT], else listed with the run it ends; dropped after damage or a run too long.
    pub(super) fn take(
        &mut self,
        piece: &mut PieceInfo,
        (at, end): (u64, u64),
        in_piece: Option<u64>,
        block: &Block<'_>,
    ) {
        let marks = (block.kind == crate::BlockKind::Records)
            .then(|| Marks::of_puts(block.flags))
            .flatten();
        // marks that break a group drop it as damage does ([crate::group_marks])
        if link(self.prev, marks) == Link::Breaks {
            self.damaged(piece);
        }
        self.prev = marks;
        let marked = block.flags & FLAG_WITH_NEXT != 0;
        let group = block.flags & FLAG_GROUP != 0;
        let dropped = match self.mode {
            Mode::AfterDamage => group && block.flags & FLAG_GROUP_START == 0,
            Mode::TooLong => true,
            Mode::Hold => false,
        };
        if dropped {
            push_dropped(piece, at, end);
            if !marked {
                self.mode = Mode::Hold;
            }
            return;
        }
        self.mode = Mode::Hold;
        let entries = in_piece.map_or_else(Vec::new, |off| IndexEntry::all_of_block(off, block));
        if marked {
            if self.held.len() >= RUN_BLOCKS_MAX.saturating_sub(1) {
                // longer than any writer makes, as a follower stops at it
                self.drop_torn(piece);
                push_dropped(piece, at, end);
                self.mode = Mode::TooLong;
                return;
            }
            self.held.push(Held {
                at,
                end,
                group,
                entries,
            });
            return;
        }
        self.list(piece);
        if let Some(list) = piece.entries.as_mut() {
            list.extend(entries);
        }
    }

    /// The run ends whole without a block of data: before a close, a close summary or an index.
    pub(super) fn end_whole(&mut self, piece: &mut PieceInfo) {
        self.mode = Mode::Hold;
        self.prev = None;
        self.list(piece);
    }

    /// The run ends torn: at the end of the scan, a tail or a piece head. Its blocks are dropped.
    pub(super) fn drop_torn(&mut self, piece: &mut PieceInfo) {
        self.mode = Mode::Hold;
        self.prev = None;
        for h in self.held.drain(..) {
            push_dropped(piece, h.at, h.end);
        }
    }

    /// Damage at the scan's place: the held blocks of a group are dropped, the other held blocks listed, and the
    /// blocks of a group without a start after it are dropped up to the first unmarked one.
    pub(super) fn damaged(&mut self, piece: &mut PieceInfo) {
        self.prev = None;
        for h in self.held.drain(..) {
            if h.group {
                push_dropped(piece, h.at, h.end);
            } else if let Some(list) = piece.entries.as_mut() {
                list.extend(h.entries);
            }
        }
        self.mode = Mode::AfterDamage;
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
