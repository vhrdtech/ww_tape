//! The state records of a group of puts ([TapeWriter::put_group](super::TapeWriter::put_group)) a writer meets, held
//! until the group's last block: a reader takes a group whole or not at all ([crate::reader]'s runs), so a resume
//! takes its records as lying near, its items as in force and its keys only once the group is whole. A block that
//! doesn't read, or bytes no block explains, inside a group void it: its records held are dropped, and the tracks of
//! its keyed ones doubted; so are those of the blocks of a group without a start after the damage, up to the first
//! block not marked [FLAG_WITH_NEXT], as a reader drops them.

use alloc::vec::Vec;

use crate::state::Slot;
use crate::{ClockTime, FLAG_GROUP, FLAG_GROUP_START, FLAG_WITH_NEXT};

/// A state record of a group, held until the group's last block.
pub(crate) struct Held {
    pub(crate) slot: Slot,
    pub(crate) at: Option<ClockTime>,
    pub(crate) item: Vec<u8>,
    /// Where its block starts
    pub(crate) block_at: u64,
}

/// What a writer does with the state records of a block of puts ([GroupRecords::begin]).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Take {
    /// Takes them now: the block is none of a group's.
    Now,
    /// Holds them until the group's last block ([GroupRecords::hold]).
    Hold,
    /// Takes none, and doubts the tracks of its keyed ones: the block is of a group damage voided.
    Void,
}

/// The group of puts a writer is inside of, as the blocks it met say.
#[derive(Default)]
pub(crate) struct GroupRecords {
    /// The state records of the open group's blocks so far
    held: Vec<Held>,
    /// Whether a group's first block came and its last one not yet
    open: bool,
    /// Whether damage came since the last block not marked [FLAG_WITH_NEXT]
    after_damage: bool,
}

impl GroupRecords {
    /// Before the records of a block of puts marked `flags`: what to do with its state records, and the records held
    /// of a group it tears (a first block of a group while another is open), to drop.
    pub(crate) fn begin(&mut self, flags: u16) -> (Take, Vec<Held>) {
        if flags & FLAG_GROUP == 0 {
            return (Take::Now, Vec::new());
        }
        if flags & FLAG_GROUP_START != 0 {
            let torn = core::mem::take(&mut self.held);
            self.open = true;
            self.after_damage = false;
            return (Take::Hold, torn);
        }
        // a block of a group without its start: the rest of a group damage hit
        if self.open && !self.after_damage {
            (Take::Hold, Vec::new())
        } else {
            (Take::Void, Vec::new())
        }
    }

    /// Hold the state record `item` in `slot` at `at`, of the block at `block_at`, until its group's last block.
    pub(crate) fn hold(&mut self, slot: Slot, at: Option<ClockTime>, item: &[u8], block_at: u64) {
        self.held.push(Held {
            slot,
            at,
            item: item.to_vec(),
            block_at,
        });
    }

    /// After the records of a block of puts marked `flags`: the state records of the group it ends, whole, to take.
    pub(crate) fn end(&mut self, flags: u16) -> Vec<Held> {
        if flags & FLAG_WITH_NEXT != 0 {
            return Vec::new();
        }
        self.after_damage = false;
        if flags & FLAG_GROUP == 0 || !self.open {
            return Vec::new();
        }
        self.open = false;
        core::mem::take(&mut self.held)
    }

    /// Damage: a block that doesn't read or bytes no block explains. The records held of the group it voids, to drop.
    pub(crate) fn damage(&mut self) -> Vec<Held> {
        self.open = false;
        self.after_damage = true;
        core::mem::take(&mut self.held)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: Slot = Slot::State {
        track: crate::TrackNum::new(3),
    };

    #[test]
    fn a_group_holds_its_keys_to_its_last_block() {
        let mut g = GroupRecords::default();
        assert_eq!(g.begin(0).0, Take::Now);
        assert_eq!(
            g.begin(FLAG_GROUP | FLAG_GROUP_START | FLAG_WITH_NEXT).0,
            Take::Hold
        );
        g.hold(T, None, b"a", 0);
        assert!(
            g.end(FLAG_GROUP | FLAG_GROUP_START | FLAG_WITH_NEXT)
                .is_empty()
        );
        assert_eq!(g.begin(FLAG_GROUP).0, Take::Hold);
        g.hold(T, None, b"b", 0);
        assert_eq!(g.end(FLAG_GROUP).len(), 2);
    }

    #[test]
    fn damage_voids_the_group_up_to_its_end() {
        let mut g = GroupRecords::default();
        g.begin(FLAG_GROUP | FLAG_GROUP_START | FLAG_WITH_NEXT);
        g.hold(T, None, b"a", 0);
        assert_eq!(g.damage().len(), 1);
        assert_eq!(g.begin(FLAG_GROUP | FLAG_WITH_NEXT).0, Take::Void);
        assert!(g.end(FLAG_GROUP | FLAG_WITH_NEXT).is_empty());
        assert_eq!(g.begin(FLAG_GROUP).0, Take::Void);
        assert!(g.end(FLAG_GROUP).is_empty());
        // a group starting after the damage is whole
        assert_eq!(g.begin(FLAG_GROUP | FLAG_GROUP_START).0, Take::Hold);
        g.hold(T, None, b"c", 0);
        assert_eq!(g.end(FLAG_GROUP | FLAG_GROUP_START).len(), 1);
    }

    #[test]
    fn a_start_tears_an_open_group() {
        let mut g = GroupRecords::default();
        g.begin(FLAG_GROUP | FLAG_GROUP_START | FLAG_WITH_NEXT);
        g.hold(T, None, b"a", 0);
        let (take, torn) = g.begin(FLAG_GROUP | FLAG_GROUP_START);
        assert_eq!((take, torn.len()), (Take::Hold, 1));
    }
}
