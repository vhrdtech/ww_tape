//! The definitions a piece carries for the tracks its head carries (WRITE-9): how they are cut into blocks, and a
//! bound on the state entries they take, which a writer checks before a track joins the next head.
//!
//! The blocks follow the piece head, the layout and the producer. Bundles come first, each once, packed into blocks
//! of at most [CARRIED_BLOCK_LEN]; a longer bundle has a block of its own. Then the tracks, grouped by the clocks and
//! maps they need (their clock chain): each block holds that chain, then as many of the group's tracks as fit in
//! [CARRIED_BLOCK_LEN], in the order of the head rows. A damaged block so costs only its own tracks, never another
//! block's clock; a lost bundle block costs its tracks their bundle.
//!
//! A piece carries every definition while the chain's tracks fit the index policy's state entries. A cut is never
//! refused for them: past that room, [fit] carries what fits, by priority, and the next head names each track left
//! out, those with the oldest data. The tracks of the call the cut is made for come first, so a call the writer
//! admitted ([entries]) always fits the piece a cut made for it.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use crate::Definitions;
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

/// The state entries the definitions data on `tracks` needs take together ([Definitions::needs], each slot once):
/// what a piece holding only them lists past its layout and producer.
pub(crate) fn entries(defs: &Definitions, tracks: &[u32]) -> usize {
    let mut seen = BTreeSet::new();
    for t in tracks {
        seen.extend(defs.needs(*t));
    }
    seen.len()
}

/// The blocks of definitions of tracks `numbers` (held in `defs`, [numbers], in the order of the head rows) that take
/// at most `room` state entries, and the tracks left out. Room stays for the state records of the call the cut is
/// made for, on the tracks of `wanted`: their definitions put again before its data, and a put's record ([entries]
/// plus one). All of them in [blocks] when those fit beside it. Else once each, no block repeating a chain, in this
/// priority while room lasts:
///
/// 1. the tracks of `wanted`, each with its bundles, its chain and its definition;
/// 2. the clocks and maps the other tracks need;
/// 3. each track of `ranked` (newest data first) with the bundles it needs, when its whole chain is in.
///
/// A track that doesn't fit is left out. Bundles, chains, then tracks in the order of the head rows, packed into
/// blocks of at most [CARRIED_BLOCK_LEN]. A call admitted by the writer so fits the piece a cut made for it.
pub(crate) fn fit(
    defs: &Definitions,
    numbers: &[u32],
    wanted: &[u32],
    ranked: &[u32],
    room: usize,
) -> (Vec<Vec<Slot>>, Vec<u32>) {
    let reserve = entries(defs, wanted).saturating_add(1);
    let all = blocks(defs, numbers);
    if all
        .iter()
        .map(Vec::len)
        .sum::<usize>()
        .saturating_add(reserve)
        <= room
    {
        return (all, Vec::new());
    }
    let needs: BTreeMap<u32, (Vec<Slot>, Vec<Slot>, Slot)> = numbers
        .iter()
        .filter_map(|n| Some((*n, split(defs, *n)?)))
        .collect();
    let mut left = room.saturating_sub(reserve);
    let mut held: BTreeSet<Slot> = BTreeSet::new();
    let mut chains = Vec::new();
    let mut taken = BTreeSet::new();
    // 1. the wanted tracks whole
    for n in wanted {
        let Some((b, chain, track)) = needs.get(n) else {
            continue;
        };
        let new = b
            .iter()
            .chain(chain)
            .filter(|s| !held.contains(*s))
            .count()
            .saturating_add(1);
        if taken.contains(n) || new > left {
            continue;
        }
        chains.extend(chain.iter().filter(|s| !held.contains(*s)).copied());
        held.extend(b.iter().chain(chain).copied());
        held.insert(*track);
        taken.insert(*n);
        left = left.saturating_sub(new);
    }
    // 2. the clocks and maps
    for n in ranked {
        let Some((_, chain, _)) = needs.get(n) else {
            continue;
        };
        for s in chain {
            if left > 0 && held.insert(*s) {
                chains.push(*s);
                left = left.saturating_sub(1);
            }
        }
    }
    // 3. the tracks by rank, with their bundles
    for n in ranked {
        let Some((b, chain, track)) = needs.get(n) else {
            continue;
        };
        if taken.contains(n) {
            continue;
        }
        let new = b.iter().filter(|s| !held.contains(*s)).count();
        if chain.iter().all(|s| held.contains(s)) && new.saturating_add(1) <= left {
            held.extend(b.iter().copied());
            held.insert(*track);
            taken.insert(*n);
            left = left.saturating_sub(new.saturating_add(1));
        }
    }
    let mut bundles = Vec::new();
    let mut seen = BTreeSet::new();
    let mut tracks = Vec::new();
    let mut dropped = Vec::new();
    for n in numbers {
        let Some((b, _, track)) = needs.get(n) else {
            continue;
        };
        if taken.contains(n) {
            bundles.extend(b.iter().filter(|s| seen.insert(**s)).copied());
            tracks.push(*track);
        } else {
            dropped.push(*n);
        }
    }
    let mut out = Vec::new();
    pack(defs, &[], &bundles, &mut out);
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
