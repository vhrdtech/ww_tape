//! The definitions a piece carries for the tracks its head carries (WRITE-9): how they are cut into blocks, and a
//! bound on the state entries they take, which a writer checks before a track joins the next head.
//!
//! The blocks follow the piece head, the layout and the producer. Bundles come first, each once, packed into blocks
//! of at most [CARRIED_BLOCK_LEN]; a longer bundle has a block of its own. Then the tracks, grouped by the clocks and
//! maps they need (their clock chain): each block holds that chain, then as many of the group's tracks as fit in
//! [CARRIED_BLOCK_LEN], in the order of the head rows. A damaged block so costs only its own tracks, never another
//! block's clock; a lost bundle block costs its tracks their bundle.
//!
//! A cut is never refused for them: when they don't fit the index policy's state entries, [fit] carries what fits, by
//! priority, and the next head names each track left out.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use crate::Definitions;
use crate::index::TrackKey;
use crate::open::EMPTY_LEN;
use crate::piece::TrackStart;
use crate::state::Slot;

/// Most bytes of a block of carried definitions, but for a bundle or a track whose definitions alone are longer.
pub const CARRIED_BLOCK_LEN: usize = 4 << 10;

/// Bytes the record of `item_len` bytes takes in a block, at most.
fn record_len(item_len: usize) -> usize {
    item_len.saturating_add(crate::MAX_RECORD_HEAD)
}

/// The bytes a record of the definition in `slot` takes, 0 for one `defs` doesn't hold.
fn slot_len(defs: &Definitions, slot: &Slot) -> usize {
    defs.get(slot).map_or(0, |i| record_len(i.len()))
}

/// The numbers of the tracks of head rows `rows` whose definition `defs` holds under the row's number with the row's
/// id and owner, in the order of the rows; another row is passed over.
pub(crate) fn numbers(defs: &Definitions, rows: &[TrackStart]) -> Vec<u32> {
    rows.iter()
        .filter(|r| defs.track_key(r.track_number) == Some((r.track_id, r.owner.0)))
        .map(|r| r.track_number)
        .collect()
}

/// What track `number` needs carried ([Definitions::needs]): its bundles, its clock chain (clocks, then maps) and its
/// own definition; None for a track `defs` doesn't hold.
fn split(defs: &Definitions, number: u32) -> Option<(Vec<Slot>, Vec<Slot>, Slot)> {
    let mut bundles = Vec::new();
    let mut chain = Vec::new();
    let mut track = None;
    for s in defs.needs(number) {
        match s {
            Slot::Own { .. } => bundles.push(s),
            Slot::Track { .. } => track = Some(s),
            _ => chain.push(s),
        }
    }
    Some((bundles, chain, track?))
}

/// The blocks of definitions tracks `numbers` (held in `defs`, [numbers]) carry, each as the slots it holds, in the
/// order they are written ([module docs](self)).
pub(crate) fn blocks(defs: &Definitions, numbers: &[u32]) -> Vec<Vec<Slot>> {
    let mut bundles = Vec::new();
    let mut seen = BTreeSet::new();
    let mut groups: BTreeMap<Vec<Slot>, Vec<Slot>> = BTreeMap::new();
    for n in numbers {
        let Some((b, chain, track)) = split(defs, *n) else {
            continue;
        };
        bundles.extend(b.into_iter().filter(|s| seen.insert(*s)));
        groups.entry(chain).or_default().push(track);
    }
    let mut out = Vec::new();
    pack(defs, &[], &bundles, &mut out);
    for (chain, tracks) in &groups {
        pack(defs, chain, tracks, &mut out);
    }
    out
}

/// The blocks of definitions of tracks `numbers` (held in `defs`, [numbers], in the order of the head rows) that take
/// at most `room` state entries, and the tracks left out. All of them in [blocks] when those fit. Else carried once
/// each, no block repeating a chain, in this priority while room lasts: the clocks and maps the tracks need, then
/// each track of `ranked` (the tracks of the next data, then newest data first) with the bundles it needs, when its
/// whole chain is in; a track that doesn't fit is left out. The tracks leave room for one more track's definition
/// and bundles: the first data on a track left out then fits the piece, which so never ends empty for it. Bundles,
/// chains, then tracks in the order of the head rows, packed into blocks of at most [CARRIED_BLOCK_LEN].
pub(crate) fn fit(
    defs: &Definitions,
    numbers: &[u32],
    ranked: &[u32],
    room: usize,
) -> (Vec<Vec<Slot>>, Vec<u32>) {
    let all = blocks(defs, numbers);
    if all.iter().map(Vec::len).sum::<usize>() <= room {
        return (all, Vec::new());
    }
    let needs: BTreeMap<u32, (Vec<Slot>, Vec<Slot>, Slot)> = numbers
        .iter()
        .filter_map(|n| Some((*n, split(defs, *n)?)))
        .collect();
    let mut left = room;
    let mut chains = Vec::new();
    let mut in_chain = BTreeSet::new();
    for n in ranked {
        let Some((_, chain, _)) = needs.get(n) else {
            continue;
        };
        for s in chain {
            if left > 0 && !in_chain.contains(s) {
                in_chain.insert(*s);
                chains.push(*s);
                left = left.saturating_sub(1);
            }
        }
    }
    // room for the definition and the bundles of a track left out, at its first data
    let reserve = needs
        .values()
        .map(|(b, _, _)| b.len().saturating_add(1))
        .max()
        .unwrap_or(0);
    left = left.saturating_sub(reserve);
    let mut bundles = BTreeSet::new();
    let mut taken = BTreeSet::new();
    for n in ranked {
        let Some((b, chain, _)) = needs.get(n) else {
            continue;
        };
        let new = b.iter().filter(|s| !bundles.contains(*s)).count();
        if chain.iter().all(|s| in_chain.contains(s)) && new.saturating_add(1) <= left {
            bundles.extend(b.iter().copied());
            taken.insert(*n);
            left = left.saturating_sub(new.saturating_add(1));
        }
    }
    let mut order = Vec::new();
    let mut seen = BTreeSet::new();
    let mut tracks = Vec::new();
    let mut dropped = Vec::new();
    for n in numbers {
        let Some((b, _, track)) = needs.get(n) else {
            continue;
        };
        if taken.contains(n) {
            order.extend(b.iter().filter(|s| seen.insert(**s)).copied());
            tracks.push(*track);
        } else {
            dropped.push(*n);
        }
    }
    let mut out = Vec::new();
    pack(defs, &[], &order, &mut out);
    pack(defs, &[], &chains, &mut out);
    pack(defs, &[], &tracks, &mut out);
    (out, dropped)
}

/// Push onto `out` blocks of `prefix` then as many of `items` as fit in [CARRIED_BLOCK_LEN], at least one each, in
/// order.
fn pack(defs: &Definitions, prefix: &[Slot], items: &[Slot], out: &mut Vec<Vec<Slot>>) {
    let base = prefix
        .iter()
        .fold(EMPTY_LEN, |n, s| n.saturating_add(slot_len(defs, s)));
    let mut cur = prefix.to_vec();
    let mut len = base;
    let mut any = false;
    for s in items {
        let l = slot_len(defs, s);
        if any && len.saturating_add(l) > CARRIED_BLOCK_LEN {
            out.push(core::mem::replace(&mut cur, prefix.to_vec()));
            len = base;
        }
        cur.push(*s);
        len = len.saturating_add(l);
        any = true;
    }
    if any {
        out.push(cur);
    }
}

/// The tracks of one clock chain the next head carries: how many, the bytes of their definitions' records, and the
/// bytes of a block holding only the chain.
#[derive(Clone, Copy, Debug, Default)]
struct Group {
    tracks: usize,
    bytes: usize,
    base: usize,
    chain: usize,
}

impl Group {
    /// Most blocks [pack] cuts the group into: any two blocks in a row hold more than a block's room past the chain,
    /// so at most twice the bytes over that room, plus one; never more than one per track.
    fn blocks_max(&self) -> usize {
        let room = CARRIED_BLOCK_LEN.saturating_sub(self.base);
        if room == 0 {
            return self.tracks;
        }
        self.tracks
            .min(
                (self
                    .bytes
                    .saturating_mul(2)
                    .checked_div(room)
                    .unwrap_or(self.tracks))
                .saturating_add(1),
            )
            .max(usize::from(self.tracks > 0))
    }

    /// Most state entries its blocks take: each track's definition, and the chain in each block.
    fn entries_max(&self) -> usize {
        self.tracks
            .saturating_add(self.chain.saturating_mul(self.blocks_max()))
    }
}

/// A bound on the state entries the definitions the next head carries take, over the tracks counted in so far:
/// what a writer checks before a track joins the next head, so a cut never runs out of room for them.
#[derive(Clone, Debug, Default)]
pub(crate) struct Plan {
    keys: BTreeSet<TrackKey>,
    /// The bundles counted in, and how many tracks need each
    bundles: BTreeMap<Slot, usize>,
    /// The tracks by clock chain
    groups: BTreeMap<Vec<Slot>, Group>,
    /// [Self::entries_max], kept as tracks are counted in
    entries: usize,
}

impl Plan {
    /// The plan of head rows `rows`, the tracks `defs` holds ([numbers]).
    pub(crate) fn of(defs: &Definitions, rows: &[TrackStart]) -> Self {
        let mut p = Self::default();
        for r in rows {
            p.add(defs, (r.track_id, r.owner), r.track_number);
        }
        p
    }

    /// Count in track `number` of `key`, as [numbers] takes it: once per key, nothing for a track `defs` doesn't hold
    /// under that key. Whether it was counted in now.
    pub(crate) fn add(&mut self, defs: &Definitions, key: TrackKey, number: u32) -> bool {
        self.change(defs, key, number, true)
    }

    /// Count track `number` of `key` out again, right after [Self::add] counted it in.
    pub(crate) fn remove(&mut self, defs: &Definitions, key: TrackKey, number: u32) {
        self.change(defs, key, number, false);
    }

    /// [Self::add] with `add`, else [Self::remove].
    fn change(&mut self, defs: &Definitions, key: TrackKey, number: u32, add: bool) -> bool {
        if defs.track_key(number) != Some((key.0, key.1.0)) || self.keys.contains(&key) != !add {
            return false;
        }
        let Some((bundles, chain, track)) = split(defs, number) else {
            return false;
        };
        if add {
            self.keys.insert(key);
        } else {
            self.keys.remove(&key);
        }
        for b in bundles {
            let n = self.bundles.entry(b).or_default();
            let was = *n;
            *n = if add {
                was.saturating_add(1)
            } else {
                was.saturating_sub(1)
            };
            match (was, *n) {
                (0, 1) => self.entries = self.entries.saturating_add(1),
                (1, 0) => {
                    self.bundles.remove(&b);
                    self.entries = self.entries.saturating_sub(1);
                }
                _ => {}
            }
        }
        let base = chain
            .iter()
            .fold(EMPTY_LEN, |n, s| n.saturating_add(slot_len(defs, s)));
        let len = slot_len(defs, &track);
        let chain_len = chain.len();
        let g = self.groups.entry(chain).or_default();
        self.entries = self.entries.saturating_sub(g.entries_max());
        if add {
            g.tracks = g.tracks.saturating_add(1);
            g.bytes = g.bytes.saturating_add(len);
        } else {
            g.tracks = g.tracks.saturating_sub(1);
            g.bytes = g.bytes.saturating_sub(len);
        }
        g.base = base;
        g.chain = chain_len;
        self.entries = self.entries.saturating_add(g.entries_max());
        true
    }

    /// Most state entries the carried definitions take: each bundle once, and each group's.
    pub(crate) fn entries_max(&self) -> usize {
        self.entries
    }
}
