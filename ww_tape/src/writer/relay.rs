//! The groups of puts a relay holds, one per producer, until each group's last block (WRITE-17).
//!
//! A relay passes on the blocks of several producers interleaved: two relays into one mux, or a relay beside the
//! writer's own puts. Each producer's group is held under the session tag of its blocks, so a block of another
//! producer between a group's blocks goes through and leaves the group whole.
use alloc::collections::BTreeMap;

use alloc::vec::Vec;

use super::{Candidate, RelayGroup};
use crate::SessionTag;

/// The producers whose groups of puts a relay holds at once ([TapeWriter::relay_block](super::TapeWriter::relay_block)).
pub const RELAY_GROUPS_MAX: usize = 16;

/// The bytes all the groups of puts a relay holds at once cost.
///
/// That is two groups of [GROUP_BYTES_MAX](super::GROUP_BYTES_MAX); a record held costs its bytes and
/// [RELAY_RECORD_OVERHEAD].
pub const RELAY_HELD_BYTES_MAX: usize = 2 * super::GROUP_BYTES_MAX;

/// The bytes a record of a group a relay holds costs besides its own.
///
/// They cover the record's place in the group, at most twice over while the group's list grows, and what the group
/// notes of its track and definitions.
pub const RELAY_RECORD_OVERHEAD: usize = 512;

const _: () = {
    assert!(
        3 * core::mem::size_of::<super::Candidate<'static>>() + 128 <= RELAY_RECORD_OVERHEAD,
        "a record's overhead covers its place in a growing list and the group's notes"
    );
};

/// The groups held, by the session tag of their producer's blocks; and the whole groups that wait for a caller's open
/// unit to end ([TapeWriter::put_group](super::TapeWriter::put_group)), each with what it costs held. Both count
/// toward [RELAY_GROUPS_MAX] and [RELAY_HELD_BYTES_MAX].
#[derive(Clone, Debug, Default)]
pub(super) struct RelayGroups {
    held: BTreeMap<SessionTag, RelayGroup>,
    waiting: Vec<(Vec<Candidate<'static>>, usize)>,
}

impl RelayGroups {
    /// Take the group held of `session` out, to add a block or drop it.
    pub(super) fn take(&mut self, session: SessionTag) -> Option<RelayGroup> {
        self.held.remove(&session)
    }

    /// Hold `group` of `session`.
    pub(super) fn hold(&mut self, session: SessionTag, group: RelayGroup) {
        self.held.insert(session, group);
    }

    /// The groups held, those waiting included.
    pub(super) fn len(&self) -> usize {
        self.held.len().saturating_add(self.waiting.len())
    }

    /// What all the groups held cost ([RELAY_HELD_BYTES_MAX]), those waiting included.
    pub(super) fn held(&self) -> usize {
        let held = self
            .held
            .values()
            .fold(0usize, |n, g| n.saturating_add(g.held));
        self.waiting
            .iter()
            .fold(held, |n, (_, cost)| n.saturating_add(*cost))
    }

    /// Hold the whole group `cands`, costing `cost`, until the open unit ends; after those waiting already.
    pub(super) fn wait(&mut self, cands: Vec<Candidate<'static>>, cost: usize) {
        self.waiting.push((cands, cost));
    }

    /// Whether a group waits.
    pub(super) fn has_waiting(&self) -> bool {
        !self.waiting.is_empty()
    }

    /// The oldest group waiting, taken out to write.
    pub(super) fn next_waiting(&mut self) -> Option<(Vec<Candidate<'static>>, usize)> {
        (!self.waiting.is_empty()).then(|| self.waiting.remove(0))
    }

    /// Put `group` back as the oldest waiting: it could not be written yet.
    pub(super) fn wait_first(&mut self, group: (Vec<Candidate<'static>>, usize)) {
        self.waiting.insert(0, group);
    }
}
