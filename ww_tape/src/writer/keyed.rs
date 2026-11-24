//! The checks a writer makes before it puts keyed state records ([super::kept::Keys]): no two live keys of a track
//! share a [KeyId], and the keys every cut carries fit a piece ([CarryLimit]).

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use super::kept::Kept;
use super::{CarryLimit, HEAD_ENTRIES, InputError, WriteError, carried};
use crate::index::TrackIdent;
use crate::state::{KeyId, KeyedItem};
use crate::{Definitions, DescribeError, TrackNum};

/// A keyed state record to put: its track, the track it stands for, and its item.
#[derive(Copy, Clone)]
pub(crate) struct KeyPut<'a> {
    pub(crate) track: TrackNum,
    pub(crate) ident: TrackIdent,
    pub(crate) item: KeyedItem<'a>,
    /// Bytes of the record's item
    pub(crate) len: usize,
}

/// The limits the keys a cut carries keep to: the index policy's state entries and the bytes the writer keeps.
#[derive(Copy, Clone)]
pub(crate) struct Limits {
    pub(crate) entries_max: usize,
    pub(crate) bytes_max: usize,
}

/// The keyed state tracks of `kept` that `defs` holds as keyed under the number and id they stand for.
fn keyed_tracks(kept: &Kept, defs: &Definitions) -> BTreeSet<TrackNum> {
    kept.keyed_tracks()
        .filter(|(n, ident)| defs.track_key(*n) == Some((ident.0, ident.1.0)))
        .filter(|(n, _)| defs.track(*n).is_some_and(|f| f.keyed))
        .map(|(n, _)| n)
        .collect()
}

/// The state entries every cut carries for the keyed tracks of `kept`, with the definitions `tracks` need: the
/// definitions of both, each once, and a record per live key.
pub(crate) fn carry_entries(kept: &Kept, defs: &Definitions, tracks: &[TrackNum]) -> usize {
    let keyed = keyed_tracks(kept, defs);
    let live = kept
        .keyed_tracks()
        .filter(|(n, _)| keyed.contains(n))
        .filter_map(|(n, ident)| kept.keys(n, ident))
        .fold(0usize, |n, k| n.saturating_add(k.count()));
    let all: Vec<TrackNum> = keyed.into_iter().chain(tracks.iter().copied()).collect();
    carried::entries(defs, &all).saturating_add(live)
}

/// Refuse `puts`, in order, when one has the [KeyId] of another key live then ([DescribeError::KeyCollision]), or
/// when they bring the live keys of the keyed tracks past `limits` ([InputError::CarryFull]): a record per key, the
/// definitions of their tracks, the layout, the producer and one put more must fit a piece, and the keys' items the
/// bytes kept. Puts that make the keys fewer or shorter are never refused for the limits.
pub(crate) fn check<E>(
    kept: &Kept,
    defs: &Definitions,
    puts: &[KeyPut<'_>],
    limits: Limits,
) -> Result<(), WriteError<E>> {
    if puts.is_empty() {
        return Ok(());
    }
    // the puts' keys as they stand after each: their item length, None once deleted
    let mut after: BTreeMap<(TrackNum, &[u8]), Option<usize>> = BTreeMap::new();
    // the keys the puts made live, by id
    let mut ids: BTreeMap<(TrackNum, KeyId), &[u8]> = BTreeMap::new();
    let mut live_delta: isize = 0;
    let mut bytes_delta: isize = 0;
    let mut tracks = keyed_tracks(kept, defs);
    for p in puts {
        tracks.insert(p.track);
        let keys = kept.keys(p.track, p.ident);
        let key = p.item.key;
        let before = match after.get(&(p.track, key)) {
            Some(b) => *b,
            None => keys.and_then(|k| k.live.get(key)).map(|s| s.item.len()),
        };
        let now = p.item.value.map(|_| p.len);
        if now.is_some()
            && let Some(id) = KeyId::of(key)
        {
            let other_put = ids.get(&(p.track, id)).is_some_and(|k| *k != key);
            let other_kept = keys.is_some_and(|k| k.collides(key))
                && keys
                    .and_then(|k| k.id_key(id))
                    .is_some_and(|k| after.get(&(p.track, k)).is_none_or(Option::is_some));
            if other_put || other_kept {
                return Err(DescribeError::KeyCollision { track: p.track }.into());
            }
            ids.insert((p.track, id), key);
        } else if let Some(id) = KeyId::of(key)
            && ids.get(&(p.track, id)) == Some(&key)
        {
            ids.remove(&(p.track, id));
        }
        live_delta = live_delta
            .saturating_add(isize::from(now.is_some()))
            .saturating_sub(isize::from(before.is_some()));
        bytes_delta = bytes_delta
            .saturating_add(now.map_or(0, signed))
            .saturating_sub(before.map_or(0, signed));
        after.insert((p.track, key), now);
    }
    let tracks: Vec<TrackNum> = tracks.into_iter().collect();
    let live = kept
        .keyed_tracks()
        .filter(|(n, _)| tracks.contains(n))
        .filter_map(|(n, ident)| kept.keys(n, ident))
        .fold(0usize, |n, k| n.saturating_add(k.count()));
    let needed = carried::entries(defs, &tracks)
        .saturating_add_signed(live_delta)
        .saturating_add(live)
        .saturating_add(1)
        .saturating_add(HEAD_ENTRIES);
    if live_delta > 0 && needed > limits.entries_max {
        return Err(WriteError::Input(InputError::CarryFull {
            needed,
            max: limits.entries_max,
            limit: CarryLimit::StateEntries,
        }));
    }
    let bytes = kept.keyed_bytes().saturating_add_signed(bytes_delta);
    if bytes_delta > 0 && bytes > limits.bytes_max {
        return Err(WriteError::Input(InputError::CarryFull {
            needed: bytes,
            max: limits.bytes_max,
            limit: CarryLimit::KeptBytes,
        }));
    }
    Ok(())
}

/// `n`, the length of a slice, as a signed count: a slice is at most [isize::MAX] bytes long.
fn signed(n: usize) -> isize {
    isize::try_from(n).unwrap_or(isize::MAX)
}
