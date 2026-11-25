//! The state records of a group of puts ([TapeWriter::put_group](super::TapeWriter::put_group)) a writer meets, held
//! until the group ends whole: a reader takes a group whole or not at all, so a resume takes its records as lying
//! near, its items as in force and its keys only then. Which blocks make a group is the rule a reader keeps too
//! ([crate::group_marks]). Damage inside a group, marks that break it, or a group still open where the replay ends
//! void it: its records held are dropped and the tracks of its keyed ones doubted. So are those of the blocks of a
//! group without a start after the damage, up to the first block not marked [FLAG_WITH_NEXT](crate::FLAG_WITH_NEXT).

use alloc::vec::Vec;

use crate::ClockTime;
use crate::group_marks::{Link, Marks, link};
use crate::state::Slot;

/// A state record of a group, held until the group ends whole.
pub(crate) struct Held {
    pub(crate) slot: Slot,
    pub(crate) at: Option<ClockTime>,
    pub(crate) item: Vec<u8>,
    /// Where its block starts
    pub(crate) block_at: u64,
}

/// What a writer does with the state records of a block ([GroupRecords::begin]).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Take {
    /// Takes them now: the block is none of a group's.
    Now,
    /// Holds them until the group ends whole ([GroupRecords::hold]).
    Hold,
    /// Takes none, and doubts the tracks of its keyed ones: the block is of a group voided.
    Void,
}

/// The group the blocks met so far leave open.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
enum Chain {
    /// None
    #[default]
    Apart,
    /// A group whose first block came, its records held
    Group,
    /// The blocks of a group without its start, after damage or marks that break a group: none of it is taken
    Void,
}

/// What comes of the next block ([GroupRecords::begin]).
pub(crate) struct Begin {
    /// What to do with the block's state records
    pub(crate) take: Take,
    /// The records of a group that ended whole before the block, to take
    pub(crate) whole: Vec<Held>,
    /// The records of a group the block breaks, to void
    pub(crate) voided: Vec<Held>,
}

/// The group of puts a writer is inside of, as the blocks it met say.
#[derive(Default)]
pub(crate) struct GroupRecords {
    /// The state records of the open group's blocks so far
    held: Vec<Held>,
    chain: Chain,
    /// The marks of the block met last, None after any block of no puts or damage
    prev: Option<Marks>,
}

impl GroupRecords {
    /// Before the records of the next block: `marks` of a block of puts ([Marks::of_puts]), None for any other block.
    pub(crate) fn begin(&mut self, marks: Option<Marks>) -> Begin {
        let mut out = Begin {
            take: Take::Now,
            whole: Vec::new(),
            voided: Vec::new(),
        };
        let link = link(self.prev, marks);
        self.prev = marks;
        match link {
            Link::Within => {
                out.take = match self.chain {
                    Chain::Group => Take::Hold,
                    // Within needs an open block before: of a group or of a voided one
                    Chain::Void | Chain::Apart => Take::Void,
                };
                return out;
            }
            Link::Breaks => {
                out.voided = core::mem::take(&mut self.held);
                self.chain = Chain::Apart;
            }
            Link::Apart => {
                if self.chain == Chain::Group {
                    out.whole = core::mem::take(&mut self.held);
                }
                self.chain = Chain::Apart;
            }
        }
        match marks {
            Some(m) if m.group && m.start => {
                self.chain = Chain::Group;
                out.take = Take::Hold;
            }
            Some(m) if m.group => {
                self.chain = Chain::Void;
                out.take = Take::Void;
            }
            _ => {}
        }
        out
    }

    /// Hold the state record `item` in `slot` at `at`, of the block at `block_at`, until its group ends whole.
    pub(crate) fn hold(&mut self, slot: Slot, at: Option<ClockTime>, item: &[u8], block_at: u64) {
        self.held.push(Held {
            slot,
            at,
            item: item.to_vec(),
            block_at,
        });
    }

    /// After the records of the block of [Self::begin]: the records of the group it ends whole, to take.
    pub(crate) fn end(&mut self) -> Vec<Held> {
        if self.prev.is_some_and(|m| m.with_next) {
            return Vec::new();
        }
        let chain = core::mem::take(&mut self.chain);
        match chain {
            Chain::Group => core::mem::take(&mut self.held),
            Chain::Void | Chain::Apart => Vec::new(),
        }
    }

    /// Damage: a block that doesn't read or bytes no block explains. The records held of the group it voids.
    pub(crate) fn damage(&mut self) -> Vec<Held> {
        self.chain = Chain::Apart;
        self.prev = None;
        core::mem::take(&mut self.held)
    }

    /// Where the replay ends: the records of a group still open, which no block ended, to void.
    pub(crate) fn finish(&mut self) -> Vec<Held> {
        self.damage()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FLAG_GROUP, FLAG_GROUP_START, FLAG_REPEAT, FLAG_WITH_NEXT};

    const T: Slot = Slot::State {
        track: crate::TrackNum::new(3),
    };

    fn puts(flags: u16) -> Option<Marks> {
        Marks::of_puts(FLAG_REPEAT | flags)
    }

    const FIRST: u16 = FLAG_GROUP | FLAG_GROUP_START | FLAG_WITH_NEXT;

    #[test]
    fn a_group_holds_its_records_to_its_last_block() {
        let mut g = GroupRecords::default();
        assert_eq!(g.begin(puts(0)).take, Take::Now);
        assert!(g.end().is_empty());
        assert_eq!(g.begin(puts(FIRST)).take, Take::Hold);
        g.hold(T, None, b"a", 0);
        assert!(g.end().is_empty());
        assert_eq!(g.begin(puts(FLAG_GROUP)).take, Take::Hold);
        g.hold(T, None, b"b", 0);
        assert_eq!(g.end().len(), 2);
    }

    #[test]
    fn a_block_of_no_puts_ends_a_marked_group_whole() {
        let mut g = GroupRecords::default();
        g.begin(puts(FIRST));
        g.hold(T, None, b"a", 0);
        g.end();
        let b = g.begin(None);
        assert_eq!((b.take, b.whole.len(), b.voided.len()), (Take::Now, 1, 0));
    }

    #[test]
    fn damage_voids_the_group_up_to_its_end() {
        let mut g = GroupRecords::default();
        g.begin(puts(FIRST));
        g.hold(T, None, b"a", 0);
        g.end();
        assert_eq!(g.damage().len(), 1);
        assert_eq!(g.begin(puts(FLAG_GROUP | FLAG_WITH_NEXT)).take, Take::Void);
        assert!(g.end().is_empty());
        assert_eq!(g.begin(puts(FLAG_GROUP)).take, Take::Void);
        assert!(g.end().is_empty());
        // a group starting after the damage is whole
        assert_eq!(
            g.begin(puts(FLAG_GROUP | FLAG_GROUP_START)).take,
            Take::Hold
        );
        g.hold(T, None, b"c", 0);
        assert_eq!(g.end().len(), 1);
    }

    #[test]
    fn a_block_of_puts_outside_an_open_group_voids_it() {
        let mut g = GroupRecords::default();
        g.begin(puts(FIRST));
        g.hold(T, None, b"a", 0);
        g.end();
        let b = g.begin(puts(FLAG_WITH_NEXT));
        assert_eq!((b.take, b.voided.len()), (Take::Now, 1));
        g.end();
        // the rest of the group after it: no start, voided
        assert_eq!(g.begin(puts(FLAG_GROUP)).take, Take::Void);
        assert!(g.end().is_empty());
    }

    #[test]
    fn a_start_tears_an_open_group() {
        let mut g = GroupRecords::default();
        g.begin(puts(FIRST));
        g.hold(T, None, b"a", 0);
        g.end();
        let b = g.begin(puts(FLAG_GROUP | FLAG_GROUP_START));
        assert_eq!((b.take, b.voided.len()), (Take::Hold, 1));
    }

    #[test]
    fn a_group_open_at_the_end_is_voided() {
        let mut g = GroupRecords::default();
        g.begin(puts(FIRST));
        g.hold(T, None, b"a", 0);
        g.end();
        assert_eq!(g.finish().len(), 1);
    }
}
