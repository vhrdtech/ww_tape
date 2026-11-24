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
//! - **Keyed state.** A keyed state track keeps every live key ([Keys]), never forgotten: a cut carries them all. The
//!   keys are complete when the writer saw every record of the track since its first, directly or through the
//!   pieces before ([Keys::complete]); a cut marks the track's row then ([TrackStart::keys_carried]). The keyed
//!   items count against the cap first, so the plain items are forgotten to make room for them; the writer refuses a
//!   put that would bring the keyed items alone past it ([CarryLimit::KeptBytes](super::CarryLimit::KeptBytes)).
//!
//! [TrackStart::keys_carried]: crate::piece::TrackStart::keys_carried

use crate::TrackNum;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::Arc;

use alloc::vec::Vec;

use crate::ClockTime;
use crate::index::TrackIdent;
use crate::state::{ItemHash, KeyId, KeyedItem};

/// Bytes of state items a writer keeps for its cuts by default ([NearPolicy::kept_bytes_max](super::NearPolicy)):
/// 64 MiB.
pub const KEPT_BYTES_MAX: exact_units::Bytes<u64> = exact_units::Bytes::new(64 << 20);

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

/// The live keys of one keyed state track, see the [module docs](self).
#[derive(Clone, Debug)]
pub(crate) struct Keys {
    /// The track the number stood for when its records were placed
    pub(crate) key: TrackIdent,
    /// The newest record of each live key, by key: its keyed item
    pub(crate) live: BTreeMap<Vec<u8>, State>,
    /// The live keys by their id: a key of another key's id collides
    ids: BTreeMap<KeyId, Vec<u8>>,
    /// The keys deleted while the keys are not complete: older keys taken after these ([Kept::after]) stay deleted
    gone: BTreeSet<Vec<u8>>,
    /// Bytes of the live keys' items
    bytes: usize,
    /// Every record of the track since its first was seen: the live keys are all of them
    pub(crate) complete: bool,
}

impl Keys {
    fn new(key: TrackIdent, complete: bool) -> Self {
        Self {
            key,
            live: BTreeMap::new(),
            ids: BTreeMap::new(),
            gone: BTreeSet::new(),
            bytes: 0,
            complete,
        }
    }

    /// The live key other than `key` that has its id ([KeyId]): a put of `key` would share its slot.
    pub(crate) fn collides(&self, key: &[u8]) -> bool {
        KeyId::of(key)
            .and_then(|id| self.ids.get(&id))
            .is_some_and(|k| k.as_slice() != key)
    }

    /// The live key of id `id`.
    pub(crate) fn id_key(&self, id: KeyId) -> Option<&[u8]> {
        self.ids.get(&id).map(Vec::as_slice)
    }

    /// The live keys.
    pub(crate) fn count(&self) -> usize {
        self.live.len()
    }

    /// Take the keyed record `item` at `at` (its key and value, or a deletion), placed as the `placed`th item.
    fn take(&mut self, keyed: KeyedItem<'_>, at: Option<ClockTime>, item: &[u8], placed: u64) {
        self.remove(keyed.key);
        if keyed.value.is_none() {
            if !self.complete {
                self.gone.insert(keyed.key.to_vec());
            }
            return;
        }
        self.gone.remove(keyed.key);
        if let Some(id) = KeyId::of(keyed.key) {
            self.ids.insert(id, keyed.key.to_vec());
        }
        self.bytes = self.bytes.saturating_add(item.len());
        self.live.insert(
            keyed.key.to_vec(),
            State {
                key: self.key,
                at,
                item: Arc::from(item),
                hash: ItemHash::of(item),
                placed,
            },
        );
    }

    fn remove(&mut self, key: &[u8]) {
        if let Some(old) = self.live.remove(key) {
            self.bytes = self.bytes.saturating_sub(old.item.len());
            if let Some(id) = KeyId::of(key)
                && self.ids.get(&id).is_some_and(|k| k.as_slice() == key)
            {
                self.ids.remove(&id);
            }
        }
    }

    /// These keys after `older`, the keys of the same track before them: these alone when complete, else `older`'s
    /// live keys less those these deleted, then these, complete when `older` is.
    fn after(self, older: Self) -> Self {
        if self.complete || older.key != self.key {
            return self;
        }
        let mut out = older;
        for k in &self.gone {
            out.remove(k);
        }
        if !out.complete {
            out.gone.extend(self.gone);
        }
        for (k, st) in self.live {
            out.remove(&k);
            out.gone.remove(&k);
            if let Some(id) = KeyId::of(&k) {
                out.ids.insert(id, k.clone());
            }
            out.bytes = out.bytes.saturating_add(st.item.len());
            out.live.insert(k, st);
        }
        if out.complete {
            out.gone.clear();
        }
        out
    }
}

/// The newest item of each state track, by track number, see the [module docs](self).
#[derive(Clone, Debug, Default)]
pub(crate) struct Kept {
    states: BTreeMap<TrackNum, State>,
    /// The tracks by when their item was placed, oldest first
    by_age: BTreeSet<(u64, TrackNum)>,
    bytes: usize,
    placed: u64,
    /// The live keys of each keyed state track, by track number
    keyed: BTreeMap<TrackNum, Keys>,
    /// Bytes of the keyed items, [Keys::bytes] of every track
    keyed_bytes: usize,
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
        self.trim(bytes_max);
    }

    /// Forget the plain items placed longest ago until those and the keyed items take at most `bytes_max` bytes, or
    /// no plain item is left.
    fn trim(&mut self, bytes_max: usize) {
        while self.bytes.saturating_add(self.keyed_bytes) > bytes_max {
            let Some((_, oldest)) = self.by_age.first().copied() else {
                break;
            };
            self.forget(oldest);
        }
    }

    /// Keyed record `item` on keyed state track `number` at `at`, standing for `key`: a value or a deletion of its
    /// key. A track first seen here starts with the keys `complete` says ([Keys::complete]); at most `bytes_max`
    /// bytes stay kept, the plain items forgotten first. An item that isn't a keyed item is passed over.
    pub(crate) fn key_placed(
        &mut self,
        number: TrackNum,
        key: TrackIdent,
        at: Option<ClockTime>,
        item: &[u8],
        complete: bool,
        bytes_max: usize,
    ) {
        let Ok(keyed) = KeyedItem::read(item) else {
            return;
        };
        self.placed = self.placed.saturating_add(1);
        let placed = self.placed;
        let keys = self.keys_entry(number, key, complete);
        keys.take(keyed, at, item, placed);
        self.sum_keyed();
        self.trim(bytes_max);
    }

    /// The keys of track `number` standing for `key`, made with `complete` when the track has none or stood for
    /// another track.
    fn keys_entry(&mut self, number: TrackNum, key: TrackIdent, complete: bool) -> &mut Keys {
        let keys = self
            .keyed
            .entry(number)
            .or_insert_with(|| Keys::new(key, complete));
        if keys.key != key {
            *keys = Keys::new(key, complete);
        }
        keys
    }

    /// Whether the keys of keyed state track `number`, standing for `key`, are all of them: given an empty set of
    /// keys when it has none yet.
    pub(crate) fn keys_known(&mut self, number: TrackNum, key: TrackIdent, complete: bool) {
        self.keys_entry(number, key, complete).complete = complete;
    }

    /// The keys of track `number` may miss some: a record of it doesn't read.
    pub(crate) fn keys_doubted(&mut self, number: TrackNum, key: TrackIdent) {
        self.keys_entry(number, key, false).complete = false;
    }

    /// Every keyed track's keys may miss some: bytes no block explains may hold their records.
    pub(crate) fn all_keys_doubted(&mut self) {
        for k in self.keyed.values_mut() {
            k.complete = false;
        }
    }

    /// The live keys of keyed state track `number` while it stands for `key`.
    pub(crate) fn keys(&self, number: TrackNum, key: TrackIdent) -> Option<&Keys> {
        self.keyed.get(&number).filter(|k| k.key == key)
    }

    /// Each keyed state track with keys, by number, and the track it stands for.
    pub(crate) fn keyed_tracks(&self) -> impl Iterator<Item = (TrackNum, TrackIdent)> + '_ {
        self.keyed.iter().map(|(n, k)| (*n, k.key))
    }

    /// Bytes of the keyed items of every track.
    pub(crate) fn keyed_bytes(&self) -> usize {
        self.keyed_bytes
    }

    fn sum_keyed(&mut self) {
        self.keyed_bytes = self
            .keyed
            .values()
            .fold(0usize, |n, k| n.saturating_add(k.bytes));
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
    pub(crate) fn after(&mut self, mut older: Self, bytes_max: usize) {
        let mut out = Self::default();
        let mut newer = core::mem::take(self);
        out.keyed = core::mem::take(&mut newer.keyed);
        out.keys_after(older.take_keys());
        for from in [older, newer] {
            for (_, n) in &from.by_age {
                if let Some(st) = from.states.get(n) {
                    out.placed(*n, st.key, st.at, &st.item, bytes_max);
                }
            }
        }
        *self = out;
    }

    /// Go on with the keys of these after those of `older`, the keys before them ([Keys::after]); the plain items
    /// stay as they are.
    pub(crate) fn keys_after(&mut self, mut older: Self) {
        let mut keyed = core::mem::take(&mut older.keyed);
        for (n, k) in core::mem::take(&mut self.keyed) {
            let k = match keyed.remove(&n) {
                Some(old) => k.after(old),
                None => k,
            };
            keyed.insert(n, k);
        }
        self.keyed = keyed;
        self.sum_keyed();
    }

    /// The keys of every keyed track, taken out: these keep the plain items alone.
    pub(crate) fn take_keys(&mut self) -> Self {
        let keyed = core::mem::take(&mut self.keyed);
        self.sum_keyed();
        Self {
            keyed,
            ..Self::default()
        }
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
        self.keyed.retain(|n, k| keep.get(n) == Some(&k.key));
        self.sum_keyed();
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
