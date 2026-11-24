//! The blocks of definitions written twice at once ([TrackDef::defined_twice](crate::TrackDef::defined_twice)) and
//! their copies (TRACK-9).
//!
//! A put writes the first record of such a definition in the piece in a block of puts of its own, marked
//! [FLAG_TWICE](crate::FLAG_TWICE), linked into its run of blocks belonging together as any block of puts. The copy of
//! that block follows right after the block that ends the run: the same records, marked [FLAG_REPEAT](crate::FLAG_REPEAT)
//! and [FLAG_TWICE](crate::FLAG_TWICE), never [FLAG_WITH_NEXT](crate::FLAG_WITH_NEXT). It so belongs with no other
//! block, and never ends or joins a run.
//!
//! [Twice] follows every block the writer writes or a resume replays, in order, and says which copies are due. A
//! resume whose bytes stop between a run and its copies writes them itself, so the piece gets the bytes it would have
//! got without the crash.

use alloc::collections::{BTreeSet, VecDeque};
use alloc::vec::Vec;

use crate::state::Slot;
use crate::{BlockKind, CRC_LEN, FLAG_REPEAT, FLAG_TWICE, FLAG_WITH_NEXT, HEADER_LEN};

/// The blocks marked [FLAG_TWICE](crate::FLAG_TWICE) of the open run, and the copies due after the last run.
#[derive(Clone, Debug, Default)]
pub(crate) struct Twice {
    /// Blocks of the run not yet ended, as written
    open: Vec<Vec<u8>>,
    /// Blocks whose copy is due, oldest first
    due: VecDeque<Vec<u8>>,
    /// The slots of the records of those blocks: their second record is the copy due
    slots: BTreeSet<Slot>,
}

/// The header flags of `block`; 0 for a block too short to hold them.
fn flags(block: &[u8]) -> u16 {
    match (block.get(6), block.get(7)) {
        (Some(lo), Some(hi)) => u16::from_le_bytes([*lo, *hi]),
        _ => 0,
    }
}

/// The slots of the records of the good records block `block`; none of a block that doesn't read.
fn slots_of(block: &[u8]) -> Vec<Slot> {
    let Ok(b) = crate::Block::read_trusted(block) else {
        return Vec::new();
    };
    let mut buf = Vec::new();
    let Ok(b) = b.decode(&mut buf) else {
        return Vec::new();
    };
    b.records()
        .filter_map(Result::ok)
        .filter_map(|r| Slot::of(r.track, r.item))
        .collect()
}

/// The kind byte and body of `block`, between its header and CRC: what a copy has the same as its block.
fn content(block: &[u8]) -> (Option<&u8>, &[u8]) {
    let end = block.len().saturating_sub(CRC_LEN);
    (block.get(5), block.get(HEADER_LEN..end).unwrap_or_default())
}

impl Twice {
    /// Note the good block `block` written (or replayed) next.
    pub(crate) fn saw(&mut self, block: &[u8]) {
        let kind = BlockKind::from_u8(block.get(5).copied().unwrap_or(0));
        if crate::index::is_structural(kind) {
            // index, close, tail and piece head blocks belong to no run
            return;
        }
        let f = flags(block);
        let twice = f & FLAG_TWICE != 0 && f & FLAG_REPEAT != 0;
        if let Some(first) = self.due.front() {
            if twice && f & FLAG_WITH_NEXT == 0 && content(first) == content(block) {
                self.due.pop_front();
                self.forget_slots();
                return;
            }
            // a writer writes every copy due before anything else; bytes that differ end what is due
            self.due.clear();
            self.forget_slots();
        }
        if twice {
            self.open.push(block.to_vec());
            self.slots.extend(slots_of(block));
        }
        if f & FLAG_WITH_NEXT == 0 {
            self.due.extend(self.open.drain(..));
        }
    }

    /// Whether the copy of a record of `slot` is due, its first record written: a put has the two records it asks.
    pub(crate) fn copy_due(&self, slot: &Slot) -> bool {
        self.slots.contains(slot)
    }

    /// Forget the slots once no copy is due.
    fn forget_slots(&mut self) {
        if self.open.is_empty() && self.due.is_empty() {
            self.slots.clear();
        }
    }

    /// Note a block that didn't read, or bytes no block explains: what it held is not known, nothing is due.
    pub(crate) fn lost(&mut self) {
        self.open.clear();
        self.due.clear();
        self.slots.clear();
    }

    /// The block whose copy is due next.
    pub(crate) fn next_due(&self) -> Option<&[u8]> {
        self.due.front().map(Vec::as_slice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A block of `kind` with `flags` and body `body`.
    fn block(flags: u16, body: &[u8]) -> Vec<u8> {
        let mut b = vec![0u8; HEADER_LEN];
        b[5] = 1;
        b[6..8].copy_from_slice(&flags.to_le_bytes());
        b.extend_from_slice(body);
        b.extend_from_slice(&[0xee; CRC_LEN]);
        b
    }

    const MAIN: u16 = FLAG_REPEAT | FLAG_TWICE | FLAG_WITH_NEXT;
    const COPY: u16 = FLAG_REPEAT | FLAG_TWICE;

    #[test]
    fn a_copy_is_due_once_its_run_ends() {
        let mut t = Twice::default();
        t.saw(&block(FLAG_REPEAT | FLAG_WITH_NEXT, b"head"));
        t.saw(&block(MAIN, b"defs"));
        assert!(t.next_due().is_none(), "the run is open");
        t.saw(&block(0, b"data"));
        assert_eq!(
            t.next_due().map(content),
            Some(content(&block(MAIN, b"defs")))
        );
        t.saw(&block(COPY, b"defs"));
        assert!(t.next_due().is_none());
    }

    #[test]
    fn a_run_of_one_twice_block_has_its_copy_after_it() {
        let mut t = Twice::default();
        t.saw(&block(COPY, b"defs"));
        assert!(t.next_due().is_some(), "the block ends its own run");
        t.saw(&block(COPY, b"defs"));
        assert!(t.next_due().is_none(), "the copy");
    }

    #[test]
    fn other_bytes_or_a_loss_end_what_is_due() {
        let mut t = Twice::default();
        t.saw(&block(COPY, b"defs"));
        t.saw(&block(0, b"data"));
        assert!(t.next_due().is_none());
        t.saw(&block(MAIN, b"defs"));
        t.lost();
        t.saw(&block(0, b"data"));
        assert!(t.next_due().is_none());
    }

    /// The upper bound of a call's puts holds a definition written twice at once and its copy.
    #[test]
    fn the_upper_bound_of_puts_counts_the_copy() {
        use crate::writer::{Candidate, Item, Rule, puts_len, puts_len_upper};
        use crate::{RecordRef, RecordTrack, TrackNum, state::ItemHash};
        // long enough that the slack of one record's bound holds no second block
        let bytes = vec![0x5a; 1000];
        let c = Candidate {
            slot: Slot::Track {
                number: TrackNum::new(9),
            },
            rule: Rule {
                near: exact_units::Bytes::new(0),
                copies: exact_units::Count::new(1),
                spacing: exact_units::Bytes::new(0),
                twice: true,
            },
            track: RecordTrack::Definitions,
            at: None,
            hash: ItemHash::of(&bytes),
            item: Item::Given(&bytes),
        };
        let one = puts_len::<()>(&[RecordRef {
            track: RecordTrack::Definitions,
            at: None,
            item: &bytes,
        }])
        .unwrap();
        let upper = puts_len_upper(&[c]);
        assert!(upper >= one.saturating_mul(2), "{upper} < 2 * {one}");
    }
}
