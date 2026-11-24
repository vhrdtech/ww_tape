//! What a [TapeWriter](super::TapeWriter) remembers of the state records of the piece it writes: per slot, the newest
//! item, where it lies and how many records of it lie in the piece.
//!
//! - **Near.** A put writes an item unless the newest record of its slot ([Slot]) is the same item (by [ItemHash]) at
//!   most `near` bytes behind ([StatePolicy::near](crate::StatePolicy)). The memory is per slot, not per hash: a slot
//!   that went from item A to B and back to A needs A written again, though A lies close behind.
//! - **Copies on use.** A put of the same item close behind still writes it, as a copy, while fewer than
//!   1 + [StatePolicy::copies](crate::StatePolicy) records of it lie in the piece and the newest lies at least
//!   [NearPolicy::copy_spacing] behind. Nothing is scheduled ahead: a copy is written only where the item is used.
//! - **Twice at once.** A definition a track asks to have written twice
//!   ([TrackDef::defined_twice](crate::TrackDef::defined_twice)) is written while fewer than two records of it lie in
//!   the piece. Its first record gets a copy after its run ([FLAG_TWICE](crate::FLAG_TWICE)); one written once before (a clock a track
//!   that asked nothing put) is written once more. Its copies spaced apart come on top of those two.
//! - **Cap.** The memory holds at most [NearPolicy::memory_max] slots and forgets the one written longest ago first;
//!   a slot forgotten costs one more write, never a wrong read. The layout and the producer, two slots every call
//!   uses, are held besides and never forgotten. A slot whose newest record lies in a damaged block
//!   (a resume knows it from the index) is held as unknown: the next put writes.
//!
//! Everything here follows from the blocks written, in order (the writer learns every state record of every block, a
//! resume's replay included), so a resumed writer decides as the writer did before the crash. A relay that hands
//! blocks to several consumers ([Near] per consumer, positions in that consumer's stream) decides by the same rule.

use alloc::collections::{BTreeMap, BTreeSet};

use crate::state::{ItemHash, Slot};
use exact_units::units::Item;
use exact_units::{Bytes, Count};

/// Bytes between records of an item before a use writes a copy ([NearPolicy::copy_spacing]): 1 MiB, past the damage
/// around one place (a block or two).
pub const COPY_SPACING: Bytes<u64> = Bytes::new(1 << 20);
/// Slots a writer remembers by default ([NearPolicy::memory_max]).
pub const MEMORY_MAX: Count<Item, u64> = Count::new(1 << 18);

/// How a [TapeWriter](super::TapeWriter) remembers and copies state records, see the [module docs](self). Changed
/// while writing with [TapeWriter::set_near_policy](super::TapeWriter::set_near_policy); a resume is to be given the
/// policy the piece was written with.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct NearPolicy {
    /// Bytes from the newest record of an item to where a use writes a copy ([COPY_SPACING]).
    pub copy_spacing: Bytes<u64>,
    /// Slots remembered ([MEMORY_MAX]).
    pub memory_max: Count<Item, u64>,
    /// Bytes of the newest state items kept for a cut to carry ([KEPT_BYTES_MAX](super::KEPT_BYTES_MAX)).
    pub kept_bytes_max: Bytes<u64>,
}

impl Default for NearPolicy {
    fn default() -> Self {
        Self {
            copy_spacing: COPY_SPACING,
            memory_max: MEMORY_MAX,
            kept_bytes_max: super::KEPT_BYTES_MAX,
        }
    }
}

/// The newest record of a slot in the piece.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct Written {
    /// None when the block holding it is damaged: the item isn't known
    hash: Option<ItemHash>,
    /// Where the block holding it starts in the piece
    at: u64,
    /// Records of this item in the slot in the piece, the newest included
    records: u32,
}

/// What a put decides by: the slot's `near`, the copies its track asks for and the spacing between them.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    /// Bytes behind a use within which the newest record of the item serves ([StatePolicy::near](crate::StatePolicy)).
    pub near: Bytes<u64>,
    /// Copies after the first record ([StatePolicy::copies](crate::StatePolicy)).
    pub copies: Count<Item, u8>,
    /// Bytes from the newest record to where a use writes a copy ([NearPolicy::copy_spacing]).
    pub spacing: Bytes<u64>,
    /// Whether the record is written twice at once ([TrackDef::defined_twice](crate::TrackDef::defined_twice)).
    pub twice: bool,
}

/// The state records of a piece a writer remembers, see the [module docs](self).
#[derive(Clone, Debug, Default)]
pub struct Near {
    slots: BTreeMap<Slot, Written>,
    /// The slots remembered by where their newest record lies, oldest first
    by_age: BTreeSet<(u64, Slot)>,
}

impl Near {
    /// Forget everything: a new piece has nothing behind it.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Whether a put of the item `hash` in `slot` writes it, with the next block at `pos` in the piece: not when the
    /// slot's newest record is that item at most `rule.near` behind, unless it is due a copy.
    pub fn writes(&self, slot: &Slot, hash: ItemHash, pos: u64, rule: Rule) -> bool {
        let Some(w) = self.slots.get(slot) else {
            return true;
        };
        let behind = pos.saturating_sub(w.at);
        let copies = u32::from(rule.copies.get()).saturating_add(u32::from(rule.twice));
        let copy = w.records <= copies && behind >= rule.spacing.get();
        let at_once = rule.twice && w.records < 2;
        w.hash != Some(hash) || behind > rule.near.get() || copy || at_once
    }

    /// Note a record of item `hash` in `slot` written in the block at `offset` (None: the block is damaged and the
    /// item unknown); `policy` caps the memory.
    pub fn wrote(&mut self, slot: Slot, hash: Option<ItemHash>, offset: u64, policy: &NearPolicy) {
        let records = match self.slots.get(&slot) {
            Some(o) if hash.is_some() && o.hash == hash => o.records.saturating_add(1),
            _ => 1,
        };
        let old = self.slots.insert(
            slot,
            Written {
                hash,
                at: offset,
                records,
            },
        );
        if let Some(o) = old {
            self.by_age.remove(&(o.at, slot));
        }
        if !matches!(slot, Slot::Layout | Slot::Producer) {
            self.by_age.insert((offset, slot));
        }
        while self.by_age.len() > crate::cap_usize(policy.memory_max.get()).max(1) {
            let Some((_, oldest)) = self.by_age.pop_first() else {
                break;
            };
            self.slots.remove(&oldest);
        }
    }

    /// Records of the item `hash` in `slot` the piece holds, the newest of the slot among them; 0 when the slot's
    /// newest record is another item, unknown, or not remembered.
    pub fn records_of(&self, slot: &Slot, hash: ItemHash) -> u32 {
        self.slots
            .get(slot)
            .filter(|w| w.hash == Some(hash))
            .map_or(0, |w| w.records)
    }

    /// Slots remembered.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Whether no slot is remembered.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TrackNum;

    const RULE: Rule = Rule {
        near: Bytes::new(1000),
        copies: Count::new(0),
        spacing: Bytes::new(10),
        twice: false,
    };

    fn wrote(n: &mut Near, slot: Slot, item: &[u8], offset: u64, p: &NearPolicy) {
        n.wrote(slot, Some(ItemHash::of(item)), offset, p);
    }

    #[test]
    fn a_slot_that_moved_on_and_back_is_written_again() {
        let p = NearPolicy::default();
        let mut n = Near::default();
        let s = Slot::State {
            track: TrackNum::new(3),
        };
        wrote(&mut n, s, b"A", 0, &p);
        assert!(!n.writes(&s, ItemHash::of(b"A"), 100, RULE));
        wrote(&mut n, s, b"B", 100, &p);
        assert!(n.writes(&s, ItemHash::of(b"A"), 200, RULE));
        assert!(n.writes(&s, ItemHash::of(b"B"), 2000, RULE));
    }

    #[test]
    fn a_use_writes_the_copies_spaced() {
        let p = NearPolicy::default();
        let rule = Rule {
            copies: Count::new(2),
            ..RULE
        };
        let mut n = Near::default();
        let s = Slot::Track {
            number: TrackNum::new(1),
        };
        let a = ItemHash::of(b"A");
        wrote(&mut n, s, b"A", 0, &p);
        assert!(!n.writes(&s, a, 9, rule));
        assert!(n.writes(&s, a, 10, rule));
        wrote(&mut n, s, b"A", 10, &p);
        assert!(!n.writes(&s, a, 19, rule));
        assert!(n.writes(&s, a, 20, rule));
        wrote(&mut n, s, b"A", 20, &p);
        // three records lie in the piece: no more copies, only past near
        assert!(!n.writes(&s, a, 500, rule));
        assert!(n.writes(&s, a, 1021, rule));
    }

    #[test]
    fn a_record_written_twice_keeps_its_spaced_copies() {
        let p = NearPolicy::default();
        let rule = Rule {
            copies: Count::new(1),
            twice: true,
            ..RULE
        };
        let mut n = Near::default();
        let s = Slot::Clock {
            number: crate::ClockNum::new(1),
        };
        let a = ItemHash::of(b"A");
        // one record, written by a put of a track that doesn't ask for it twice: the next put writes it again
        wrote(&mut n, s, b"A", 0, &p);
        assert!(n.writes(&s, a, 1, rule));
        wrote(&mut n, s, b"A", 1, &p);
        assert!(!n.writes(&s, a, 10, rule));
        // the two at once, then one copy spaced apart
        assert!(n.writes(&s, a, 11, rule));
        wrote(&mut n, s, b"A", 11, &p);
        assert!(!n.writes(&s, a, 500, rule));
    }

    #[test]
    fn a_damaged_record_is_written_again() {
        let p = NearPolicy::default();
        let mut n = Near::default();
        let s = Slot::State {
            track: TrackNum::new(3),
        };
        wrote(&mut n, s, b"A", 0, &p);
        n.wrote(s, None, 50, &p);
        assert!(n.writes(&s, ItemHash::of(b"A"), 60, RULE));
    }

    #[test]
    fn the_memory_forgets_the_oldest_first() {
        let p = NearPolicy {
            memory_max: Count::new(2),
            ..NearPolicy::default()
        };
        let mut n = Near::default();
        for (i, t) in [1u32, 2, 3].into_iter().enumerate() {
            wrote(
                &mut n,
                Slot::State {
                    track: TrackNum::new(t),
                },
                b"x",
                i as u64,
                &p,
            );
        }
        assert_eq!(n.len(), 2);
        assert!(n.writes(
            &Slot::State {
                track: TrackNum::new(1)
            },
            ItemHash::of(b"x"),
            3,
            RULE
        ));
        assert!(!n.writes(
            &Slot::State {
                track: TrackNum::new(3)
            },
            ItemHash::of(b"x"),
            3,
            RULE
        ));
    }
}
