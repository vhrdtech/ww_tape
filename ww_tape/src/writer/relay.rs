//! The groups of puts a relay holds, one per producer, until each group's last block (WRITE-17).
//!
//! A relay passes on the blocks of several producers interleaved: two relays into one mux, or a relay beside the
//! writer's own puts. Each producer's group is held under the session tag of its blocks, so a block of another
//! producer between a group's blocks goes through and leaves the group whole.
use alloc::collections::BTreeMap;

use super::RelayGroup;
use crate::SessionTag;

/// The producers whose groups of puts a relay holds at once ([TapeWriter::relay_block](super::TapeWriter::relay_block)).
pub const RELAY_GROUPS_MAX: usize = 16;

/// The bytes of items in all the groups of puts a relay holds at once: two groups of
/// [GROUP_BYTES_MAX](super::GROUP_BYTES_MAX).
pub const RELAY_HELD_BYTES_MAX: usize = 2 * super::GROUP_BYTES_MAX;

/// The groups held, by the session tag of their producer's blocks.
#[derive(Clone, Debug, Default)]
pub(super) struct RelayGroups {
    held: BTreeMap<SessionTag, RelayGroup>,
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

    /// The groups held.
    pub(super) fn len(&self) -> usize {
        self.held.len()
    }

    /// The bytes of items of all the groups held.
    pub(super) fn bytes(&self) -> usize {
        self.held
            .values()
            .fold(0usize, |n, g| n.saturating_add(g.bytes))
    }
}
