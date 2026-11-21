//! The newest item of each state track a [TapeWriter](super::TapeWriter) has placed, kept so that a cut carries it
//! into the next piece beside the track's definition ([super::carried]).
//!
//! A state put once and never again lies only in the piece that holds it. The next piece carries it, so a reader of
//! that piece alone finds the state in force, as it finds the track's definition.
//!
//! - **Learned from the blocks placed.** Every state record of every block placed, a resume's replay included, sets
//!   its track's newest item. A resumed writer so knows the states of its own piece, not those that only the pieces
//!   before it hold.
//! - **Cap.** The items kept take at most [NearPolicy::kept_bytes_max](super::NearPolicy::kept_bytes_max) bytes; past
//!   it the item placed longest ago is forgotten. A forgotten item isn't carried: the next put of its track writes it
//!   again, and [Closed::states_unknown](super::Closed::states_unknown) counts the tracks a cut found without one.

use crate::TrackNum;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::Arc;

use crate::ClockTime;
use crate::index::TrackIdent;
use crate::state::ItemHash;

/// Bytes of state items a writer keeps for its cuts by default ([NearPolicy::kept_bytes_max](super::NearPolicy)):
/// 64 MiB.
pub const KEPT_BYTES_MAX: usize = 64 << 20;

/// The newest item of one state track.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct State {
    /// The track the number stood for when the item was placed
    pub(crate) key: TrackIdent,
    pub(crate) at: Option<ClockTime>,
    pub(crate) item: Arc<[u8]>,
    pub(crate) hash: ItemHash,
    /// How many items were placed before it: its age
    placed: u64,
}

/// The newest item of each state track, by track number, see the [module docs](self).
#[derive(Clone, Debug, Default)]
pub(crate) struct Kept {
    states: BTreeMap<TrackNum, State>,
    /// The tracks by when their item was placed, oldest first
    by_age: BTreeSet<(u64, TrackNum)>,
    bytes: usize,
    placed: u64,
}

impl Kept {
    /// Item `item` at `at` is the newest of state track `number`, standing for `key`; at most `bytes_max` bytes stay
    /// kept, the items placed longest ago forgotten first.
    pub(crate) fn placed(
        &mut self,
        number: TrackNum,
        key: TrackIdent,
        at: Option<ClockTime>,
        item: &[u8],
        bytes_max: usize,
    ) {
        let hash = ItemHash::of(item);
        let same = self.states.get(&number).filter(|s| s.hash == hash);
        let item: Arc<[u8]> = same.map_or_else(|| Arc::from(item), |s| s.item.clone());
        self.forget(number);
        self.placed = self.placed.saturating_add(1);
        self.bytes = self.bytes.saturating_add(item.len());
        self.by_age.insert((self.placed, number));
        self.states.insert(
            number,
            State {
                key,
                at,
                item,
                hash,
                placed: self.placed,
            },
        );
        while self.bytes > bytes_max {
            let Some((_, oldest)) = self.by_age.first().copied() else {
                break;
            };
            self.forget(oldest);
        }
    }

    /// Forget the item of track `number`: its newest record lies in a block that doesn't read.
    pub(crate) fn forget(&mut self, number: TrackNum) {
        if let Some(old) = self.states.remove(&number) {
            self.by_age.remove(&(old.placed, number));
            self.bytes = self.bytes.saturating_sub(old.item.len());
        }
    }

    /// The newest item of track `number` while it stands for `key`.
    pub(crate) fn get(&self, number: TrackNum, key: TrackIdent) -> Option<&State> {
        self.states.get(&number).filter(|s| s.key == key)
    }

    /// Go on after `older`, the items kept before these: each of them, oldest first, then each of these, at most
    /// `bytes_max` bytes.
    pub(crate) fn after(&mut self, older: Self, bytes_max: usize) {
        let mut out = Self::default();
        for from in [older, core::mem::take(self)] {
            for (_, n) in &from.by_age {
                if let Some(st) = from.states.get(n) {
                    out.placed(*n, st.key, st.at, &st.item, bytes_max);
                }
            }
        }
        *self = out;
    }

    /// Keep only the items of the tracks `keep` holds by number and key: those a next piece's head carries.
    pub(crate) fn retain(&mut self, keep: &BTreeMap<TrackNum, TrackIdent>) {
        let gone: alloc::vec::Vec<TrackNum> = self
            .states
            .iter()
            .filter(|(n, s)| keep.get(n) != Some(&s.key))
            .map(|(n, _)| *n)
            .collect();
        for n in gone {
            self.forget(n);
        }
    }

    /// Bytes of the items kept.
    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{OwnerId, TrackId};

    const KEY: TrackIdent = (TrackId::new(7), OwnerId::NONE);

    #[test]
    fn the_newest_item_replaces_the_one_before() {
        let mut k = Kept::default();
        k.placed(TrackNum::new(1), KEY, None, b"idle", 100);
        k.placed(TrackNum::new(1), KEY, None, b"run", 100);
        assert_eq!(&*k.get(TrackNum::new(1), KEY).unwrap().item, b"run");
        assert_eq!(k.bytes(), 3);
        assert!(
            k.get(TrackNum::new(1), (TrackId::new(8), OwnerId::NONE))
                .is_none()
        );
    }

    #[test]
    fn past_the_cap_the_oldest_is_forgotten() {
        let mut k = Kept::default();
        k.placed(TrackNum::new(1), KEY, None, b"aaaa", 8);
        k.placed(TrackNum::new(2), KEY, None, b"bbbb", 8);
        k.placed(TrackNum::new(1), KEY, None, b"cccc", 8);
        k.placed(TrackNum::new(3), KEY, None, b"dd", 8);
        assert!(k.get(TrackNum::new(2), KEY).is_none());
        assert!(k.get(TrackNum::new(1), KEY).is_some() && k.get(TrackNum::new(3), KEY).is_some());
        assert_eq!(k.bytes(), 6);
    }

    #[test]
    fn retain_drops_tracks_of_another_key() {
        let mut k = Kept::default();
        k.placed(TrackNum::new(1), KEY, None, b"a", 8);
        k.placed(TrackNum::new(2), KEY, None, b"b", 8);
        k.retain(&BTreeMap::from([
            (TrackNum::new(1), KEY),
            (TrackNum::new(2), (TrackId::new(9), OwnerId::NONE)),
        ]));
        assert!(k.get(TrackNum::new(1), KEY).is_some());
        assert!(k.get(TrackNum::new(2), KEY).is_none());
        assert_eq!(k.bytes(), 1);
    }
}
