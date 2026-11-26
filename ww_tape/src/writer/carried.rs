//! The definitions a piece carries for the tracks its head carries (TRACK-5): how they are cut into blocks, and a
//! bound on the state entries they take, which a writer checks before a track joins the next head.
//!
//! The blocks follow the piece head, the layout and the producer. Bundles come first, each once, packed into blocks
//! of at most [CARRIED_BLOCK_LEN]; a longer bundle has a block of its own. Then the tracks, grouped by the clocks and
//! maps they need (their clock chain): each block holds that chain, then as many of the group's tracks as fit in
//! [CARRIED_BLOCK_LEN], in the order of the head rows. A damaged block so costs only its own tracks, never another
//! block's clock; a lost bundle block costs its tracks their bundle.
//!
//! A state track carries its newest item ([super::kept]) right after its definition, in the same block: a reader of
//! the piece alone finds the state in force as it finds the track. Its record takes one state entry more.
//!
//! A keyed state track carries every live key ([super::kept::Keys]) after its definition: one record per key, its
//! newest value, in key order, in blocks of at most [CARRIED_BLOCK_LEN] of their own (a track's keys may take
//! several). Each takes a state entry.
//!
//! A definition written twice at once ([TrackDef::defined_twice](crate::TrackDef::defined_twice)) is carried twice:
//! its block of carried definitions is preceded by a block of its own holding those records again, and each record
//! takes its state entry ([weight]).
//!
//! A piece carries every definition while the chain's tracks fit the index policy's state entries and their blocks fit
//! the run they open ([RUN_BLOCKS]). Past that room, [fit] carries what fits, by priority, and the next head names
//! each track left out, those with the oldest data. A cut is refused only when the keyed tracks and the call's tracks
//! alone pass [RUN_BLOCKS] ([InputError::CarryTooLong](super::InputError::CarryTooLong)). A keyed track is never left out: it comes first, with every key, and the writer
//! refuses a put that would make its keys too many for a piece ([CarryLimit](super::CarryLimit)). The tracks of the
//! call the cut is made for come next, so a call the writer admitted ([entries]) always fits the piece a cut made
//! for it.

use crate::TrackNum;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec;
use alloc::vec::Vec;

use crate::Definitions;
use crate::open::EMPTY_LEN;
use crate::piece::TrackStart;
use crate::state::Slot;

/// Most bytes of a block of carried definitions, but for a bundle or a track whose definitions alone are longer.
pub const CARRIED_BLOCK_LEN: usize = 4 << 10;

/// Most blocks of puts the carried definitions take. They belong with the first block of data, in one run of at most
/// [RUN_BLOCKS_MAX](crate::RUN_BLOCKS_MAX) blocks: before them the layout and the producer, a block of each of
/// [PARTS] at most, after them the call's own puts and block
/// ([FIRST_COPY_RUN_RESERVE](super::FIRST_COPY_RUN_RESERVE)).
pub(crate) const RUN_BLOCKS: usize = crate::RUN_BLOCKS_MAX - super::FIRST_COPY_RUN_RESERVE - PARTS;

/// The kinds of blocks of puts the records of one block of carried definitions go into, at most: those written twice
/// at once, those of a bundle and the others.
const PARTS: usize = 3;

/// The blocks of puts that carried blocks `out` take, at most: one of each of [PARTS] their records go into.
pub(crate) fn run_blocks(defs: &Definitions, out: &[Vec<Slot>]) -> usize {
    out.iter()
        .map(|b| {
            let twice = b.iter().any(|s| defs.defined_twice(s));
            let own = b.iter().any(|s| matches!(s, Slot::Own { .. }));
            let rest = b.iter().any(|s| !matches!(s, Slot::Own { .. }));
            usize::from(twice)
                .saturating_add(usize::from(own))
                .saturating_add(usize::from(rest))
        })
        .fold(0usize, usize::saturating_add)
}

/// Bytes the record of `item_len` bytes takes in a block, at most.
fn record_len(item_len: usize) -> usize {
    item_len.saturating_add(crate::MAX_RECORD_HEAD)
}

/// The state entries the record of `slot` takes when carried: two for a definition written twice at once.
fn weight(defs: &Definitions, slot: &Slot) -> usize {
    if defs.defined_twice(slot) { 2 } else { 1 }
}

/// The state entries the records of `slots` take when carried ([weight]).
fn weights<'s>(defs: &Definitions, slots: impl IntoIterator<Item = &'s Slot>) -> usize {
    slots
        .into_iter()
        .map(|s| weight(defs, s))
        .fold(0usize, usize::saturating_add)
}

/// What a track needs carried: its bundles, its clock chain and its own records ([Source::split]).
type Needs = (Vec<Slot>, Vec<Slot>, Vec<Slot>);

/// What a cut carries from: the definitions held, the length of the newest item of each state track it carries
/// ([super::kept]), by track number, and the live keys of each keyed track it carries, in key order, with the
/// length of each key's item.
#[derive(Copy, Clone)]
pub(crate) struct Source<'a> {
    pub(crate) defs: &'a Definitions,
    pub(crate) states: &'a BTreeMap<TrackNum, usize>,
    pub(crate) keys: &'a BTreeMap<TrackNum, Vec<Slot>>,
    pub(crate) key_lens: &'a BTreeMap<Slot, usize>,
}

impl Source<'_> {
    /// The bytes a record of `slot` takes: a definition `defs` holds, a state item or a key carried; 0 for another.
    fn slot_len(&self, slot: &Slot) -> usize {
        match slot {
            Slot::State { track } => self.states.get(track).map_or(0, |l| record_len(*l)),
            Slot::Keyed { .. } => self.key_lens.get(slot).map_or(0, |l| record_len(*l)),
            _ => self.defs.get(slot).map_or(0, |i| record_len(i.len())),
        }
    }

    /// The keys of tracks `numbers` carry, in the order of the numbers, each a group of its own.
    fn key_items(&self, numbers: &[TrackNum]) -> Vec<Vec<Slot>> {
        numbers
            .iter()
            .filter_map(|n| self.keys.get(n))
            .flatten()
            .map(|s| vec![*s])
            .collect()
    }

    /// What track `number` needs carried ([Definitions::needs]): its bundles, its clock chain (clocks, then maps) and
    /// its own records, its definition then its newest state; None for a track `defs` doesn't hold.
    fn split(&self, number: TrackNum) -> Option<Needs> {
        let mut bundles = Vec::new();
        let mut chain = Vec::new();
        let mut track = None;
        for s in self.defs.needs(number) {
            match s {
                Slot::Own { .. } => bundles.push(s),
                Slot::Track { .. } => track = Some(s),
                _ => chain.push(s),
            }
        }
        let mut own = vec![track?];
        if self.states.contains_key(&number) {
            own.push(Slot::State { track: number });
        }
        Some((bundles, chain, own))
    }
}

/// The numbers of the tracks of head rows `rows` whose definition `defs` holds under the row's number with the row's
/// id and owner, in the order of the rows; another row is passed over.
pub(crate) fn numbers(defs: &Definitions, rows: &[TrackStart]) -> Vec<TrackNum> {
    rows.iter()
        .filter(|r| defs.track_identity(r.track_number) == Some((r.track_id, r.owner.0)))
        .map(|r| r.track_number)
        .collect()
}

/// The blocks of definitions tracks `numbers` (held in `defs`, [numbers]) carry, each as the slots it holds, in the
/// order they are written ([module docs](self)).
pub(crate) fn blocks(from: Source<'_>, numbers: &[TrackNum]) -> Vec<Vec<Slot>> {
    let mut bundles = Vec::new();
    let mut seen = BTreeSet::new();
    let mut groups: BTreeMap<Vec<Slot>, Vec<Vec<Slot>>> = BTreeMap::new();
    for n in numbers {
        let Some((b, chain, own)) = from.split(*n) else {
            continue;
        };
        bundles.extend(b.into_iter().filter(|s| seen.insert(*s)).map(|s| vec![s]));
        groups.entry(chain).or_default().push(own);
    }
    let mut out = Vec::new();
    pack(from, &[], &bundles, &mut out);
    for (chain, tracks) in &groups {
        pack(from, chain, tracks, &mut out);
    }
    pack(from, &[], &from.key_items(numbers), &mut out);
    out
}

/// The state entries the keys of tracks `numbers` take when carried.
fn key_entries(from: Source<'_>, numbers: &[TrackNum]) -> usize {
    numbers
        .iter()
        .filter_map(|n| from.keys.get(n))
        .fold(0usize, |n, k| n.saturating_add(k.len()))
}

/// The state entries the definitions data on `tracks` needs take together ([Definitions::needs], each slot once):
/// what a piece holding only them lists past its layout and producer.
pub(crate) fn entries(defs: &Definitions, tracks: &[TrackNum]) -> usize {
    // one track's needs are each once: no set to build
    if let [track] = tracks {
        let mut n = 0usize;
        defs.each_need(*track, |s| n = n.saturating_add(weight(defs, &s)));
        return n;
    }
    let mut seen = BTreeSet::new();
    for t in tracks {
        seen.extend(defs.needs(*t));
    }
    weights(defs, &seen)
}

/// The blocks of definitions of tracks `numbers` (held in `defs`, [numbers], in the order of the head rows) that take
/// at most `room` state entries, and the tracks left out. Room stays for the state records of the call the cut is
/// made for, on the tracks of `wanted`: their definitions put again before its data, and a put's record ([entries]
/// plus one). All of them in [blocks] when those fit beside it. Else once each, no block repeating a chain, in this
/// priority while room lasts:
///
/// 0. the keyed tracks (those [Source::keys] holds), each with its bundles, its chain, its definition and every
///    key, whatever room is left: the writer keeps them within it ([CarryLimit](super::CarryLimit));
/// 1. the tracks of `wanted`, each with its bundles, its chain and its definition;
/// 2. the clocks and maps the other tracks need;
/// 3. each track of `ranked` (newest data first) with the bundles it needs, when its whole chain is in.
///
/// A track that doesn't fit is left out. Bundles, chains, tracks in the order of the head rows, then the keys,
/// packed into blocks of at most [CARRIED_BLOCK_LEN]. A call admitted by the writer so fits the piece a cut made for
/// it.
///
/// The blocks take at most [RUN_BLOCKS] blocks of puts ([run_blocks]): 2 and 3 take a clock, a map or a track only
/// while the blocks of what is taken stay within it, counted as they are taken ([Packed]). The keyed tracks and those
/// of `wanted` are taken all the same; the caller refuses a carry of them alone past the bound. One pass over the
/// tracks: no fit is made twice.
///
/// When the bound leaves a track out, the bundles and the tracks are packed in the order taken, as counted, not in the
/// order of the head rows.
pub(crate) fn fit(
    from: Source<'_>,
    numbers: &[TrackNum],
    wanted: &[TrackNum],
    ranked: &[TrackNum],
    room: usize,
) -> (Vec<Vec<Slot>>, Vec<TrackNum>) {
    let reserve = entries(from.defs, wanted).saturating_add(1);
    let all = blocks(from, numbers);
    if weights(from.defs, all.iter().flatten()).saturating_add(reserve) <= room
        && run_blocks(from.defs, &all) <= RUN_BLOCKS
    {
        return (all, Vec::new());
    }
    choose(from, numbers, wanted, ranked, room)
}

/// One stream of items packed into blocks as [pack] packs them, counted as items are added: the blocks of puts the
/// stream's blocks take ([run_blocks]).
#[derive(Copy, Clone)]
struct Packed {
    /// Blocks of puts of the blocks closed
    closed: usize,
    /// Bytes of the open block
    len: usize,
    /// Whether the open block holds an item, one written twice at once, a bundle and another record
    any: bool,
    twice: bool,
    own: bool,
    rest: bool,
}

impl Packed {
    const EMPTY: Self = Self {
        closed: 0,
        len: EMPTY_LEN,
        any: false,
        twice: false,
        own: false,
        rest: false,
    };

    /// The blocks of puts of the open block.
    fn open(&self) -> usize {
        usize::from(self.twice)
            .saturating_add(usize::from(self.own))
            .saturating_add(usize::from(self.rest))
    }

    /// The blocks of puts of the stream.
    fn run(&self) -> usize {
        self.closed.saturating_add(self.open())
    }

    /// The stream with `item` added after what it holds.
    fn with(self, from: Source<'_>, item: &[Slot]) -> Self {
        let l = item
            .iter()
            .fold(0usize, |n, s| n.saturating_add(from.slot_len(s)));
        let mut p = self;
        if p.any && p.len.saturating_add(l) > CARRIED_BLOCK_LEN {
            p = Self {
                closed: p.run(),
                ..Self::EMPTY
            };
        }
        p.len = p.len.saturating_add(l);
        p.any = true;
        p.twice |= item.iter().any(|s| from.defs.defined_twice(s));
        p.own |= item.iter().any(|s| matches!(s, Slot::Own { .. }));
        p.rest |= item.iter().any(|s| !matches!(s, Slot::Own { .. }));
        p
    }

    /// The stream with each of `items` added, in order.
    fn with_all(self, from: Source<'_>, items: &[Vec<Slot>]) -> Self {
        items.iter().fold(self, |p, i| p.with(from, i))
    }
}

/// What [choose] has taken: the bundles, the chains and the tracks in the order taken, and the blocks of puts of each
/// stream as [pack] makes them in that order.
struct Taken {
    bundles: (Vec<Vec<Slot>>, Packed),
    chains: (Vec<Vec<Slot>>, Packed),
    tracks: (Vec<Vec<Slot>>, Packed),
    keys: Packed,
}

impl Taken {
    /// The blocks of puts of all the streams.
    fn run(&self) -> usize {
        self.bundles
            .1
            .run()
            .saturating_add(self.chains.1.run())
            .saturating_add(self.tracks.1.run())
            .saturating_add(self.keys.run())
    }
}

/// [fit] past its room: the tracks taken in its order of priority while the state entries of `room` and
/// [RUN_BLOCKS] last, the rest left out.
fn choose(
    from: Source<'_>,
    numbers: &[TrackNum],
    wanted: &[TrackNum],
    ranked: &[TrackNum],
    room: usize,
) -> (Vec<Vec<Slot>>, Vec<TrackNum>) {
    let reserve = entries(from.defs, wanted).saturating_add(1);
    let needs: BTreeMap<TrackNum, Needs> = numbers
        .iter()
        .filter_map(|n| Some((*n, from.split(*n)?)))
        .collect();
    let mut left = room.saturating_sub(reserve);
    let mut held: BTreeSet<Slot> = BTreeSet::new();
    let mut taken = BTreeSet::new();
    let keyed: Vec<TrackNum> = numbers
        .iter()
        .filter(|n| from.keys.contains_key(n))
        .copied()
        .collect();
    let mut t = Taken {
        bundles: (Vec::new(), Packed::EMPTY),
        chains: (Vec::new(), Packed::EMPTY),
        tracks: (Vec::new(), Packed::EMPTY),
        keys: Packed::EMPTY.with_all(from, &from.key_items(&keyed)),
    };
    // the streams of `t` with a track and what it needs that isn't held yet: its bundles, its chain, its own
    // records; nothing is copied
    let grow = |t: &Taken, held: &BTreeSet<Slot>, (b, chain, own): &Needs| -> [Packed; 3] {
        let new = |s: &&Slot| !held.contains(*s);
        [
            b.iter()
                .filter(new)
                .fold(t.bundles.1, |p, s| p.with(from, &[*s])),
            chain
                .iter()
                .filter(new)
                .fold(t.chains.1, |p, s| p.with(from, &[*s])),
            t.tracks.1.with(from, own),
        ]
    };
    // take a track into `t` with the streams [grow] counted for it, then hold what it needs
    let take =
        |t: &mut Taken, held: &mut BTreeSet<Slot>, (b, chain, own): &Needs, packs: [Packed; 3]| {
            t.bundles
                .0
                .extend(b.iter().filter(|s| !held.contains(*s)).map(|s| vec![*s]));
            t.chains.0.extend(
                chain
                    .iter()
                    .filter(|s| !held.contains(*s))
                    .map(|s| vec![*s]),
            );
            t.tracks.0.push(own.clone());
            let [bundles, chains, tracks] = packs;
            (t.bundles.1, t.chains.1, t.tracks.1) = (bundles, chains, tracks);
            held.extend(b.iter().chain(chain).chain(own).copied());
        };
    // 0. the keyed tracks whole, with their keys
    for n in &keyed {
        let Some(need) = needs.get(n) else {
            continue;
        };
        let (b, chain, own) = need;
        let new = b
            .iter()
            .chain(chain)
            .filter(|s| !held.contains(*s))
            .count()
            .saturating_add(own.len());
        let packs = grow(&t, &held, need);
        take(&mut t, &mut held, need, packs);
        taken.insert(*n);
        left = left.saturating_sub(new);
    }
    left = left.saturating_sub(key_entries(from, &keyed));
    // 1. the wanted tracks whole
    for n in wanted {
        let Some(need) = needs.get(n) else {
            continue;
        };
        let (b, chain, own) = need;
        let new = weights(
            from.defs,
            b.iter().chain(chain).filter(|s| !held.contains(*s)),
        )
        .saturating_add(weights(from.defs, own));
        if taken.contains(n) || new > left {
            continue;
        }
        let packs = grow(&t, &held, need);
        take(&mut t, &mut held, need, packs);
        taken.insert(*n);
        left = left.saturating_sub(new);
    }
    // 2. the clocks and maps
    let ranked_chains = ranked
        .iter()
        .filter_map(|n| needs.get(n))
        .flat_map(|(_, chain, _)| chain);
    for s in ranked_chains {
        let w = weight(from.defs, s);
        if left < w || held.contains(s) {
            continue;
        }
        let chains = t.chains.1.with(from, &[*s]);
        let run = t.run().saturating_sub(t.chains.1.run());
        if run.saturating_add(chains.run()) > RUN_BLOCKS {
            continue;
        }
        held.insert(*s);
        t.chains.0.push(vec![*s]);
        t.chains.1 = chains;
        left = left.saturating_sub(w);
    }
    // 3. the tracks by rank, with their bundles
    for n in ranked {
        let Some(need) = needs.get(n) else {
            continue;
        };
        let (b, chain, own) = need;
        if taken.contains(n) {
            continue;
        }
        let new = weights(from.defs, b.iter().filter(|s| !held.contains(*s)))
            .saturating_add(weights(from.defs, own));
        if !chain.iter().all(|s| held.contains(s)) || new > left {
            continue;
        }
        let packs = grow(&t, &held, need);
        let run = packs
            .iter()
            .map(Packed::run)
            .fold(t.keys.run(), usize::saturating_add);
        if run > RUN_BLOCKS {
            continue;
        }
        take(&mut t, &mut held, need, packs);
        taken.insert(*n);
        left = left.saturating_sub(new);
    }
    // in the order of the head rows, as before the bound, when that packing fits it too
    let mut bundles = Vec::new();
    let mut seen = BTreeSet::new();
    let mut tracks = Vec::new();
    let mut dropped = Vec::new();
    for n in numbers {
        let Some((b, _, own)) = needs.get(n) else {
            continue;
        };
        if taken.contains(n) {
            bundles.extend(b.iter().filter(|s| seen.insert(**s)).map(|s| vec![*s]));
            tracks.push(own.clone());
        } else {
            dropped.push(*n);
        }
    }
    let keys = from.key_items(&keyed);
    let mut out = Vec::new();
    pack(from, &[], &bundles, &mut out);
    pack(from, &[], &t.chains.0, &mut out);
    pack(from, &[], &tracks, &mut out);
    pack(from, &[], &keys, &mut out);
    if run_blocks(from.defs, &out) > RUN_BLOCKS {
        out.clear();
        pack(from, &[], &t.bundles.0, &mut out);
        pack(from, &[], &t.chains.0, &mut out);
        pack(from, &[], &t.tracks.0, &mut out);
        pack(from, &[], &keys, &mut out);
    }
    (out, dropped)
}

/// Push onto `out` blocks of `prefix` then as many of `items` as fit in [CARRIED_BLOCK_LEN], at least one each, in
/// order. An item is a group of slots that stays in one block: a track's definition and its state.
fn pack(from: Source<'_>, prefix: &[Slot], items: &[Vec<Slot>], out: &mut Vec<Vec<Slot>>) {
    let base = prefix
        .iter()
        .fold(EMPTY_LEN, |n, s| n.saturating_add(from.slot_len(s)));
    let mut cur = prefix.to_vec();
    let mut len = base;
    let mut any = false;
    for item in items {
        let l = item
            .iter()
            .fold(0usize, |n, s| n.saturating_add(from.slot_len(s)));
        if any && len.saturating_add(l) > CARRIED_BLOCK_LEN {
            out.push(core::mem::replace(&mut cur, prefix.to_vec()));
            len = base;
        }
        cur.extend_from_slice(item);
        len = len.saturating_add(l);
        any = true;
    }
    if any {
        out.push(cur);
    }
}
