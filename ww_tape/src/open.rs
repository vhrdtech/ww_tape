//! [Definitions]: the definitions an application works with, which a writer checks data against and puts before it.
//!
//! The application keeps the definitions of what it works on in a [Definitions]: each track's, and the clocks, clock
//! maps and bundles they name. It defines a track or a clock without a number ([crate::TrackSpec],
//! [crate::ClockSpec]); [Definitions::define_track] and [Definitions::define_clock] give it one, which [Numbers]
//! keeps, and return it. It passes them with every call that writes data or puts an item; the writer checks the
//! data against them and, before a block of data of a track, puts the track's definition and what it names
//! ([Definitions::needs]) where those don't lie close behind ([state](crate::state)). Which definitions are held is
//! the application's state, not the file's: [Definitions::close] drops a track, and a clock, map or bundle goes with
//! the last track that names it; nothing is written for it. The producer and the layout are the writer's own
//! ([crate::WriterOptions]).
//!
//! [Definitions::len] and [Definitions::bytes] say how much is held, for the application to bound as it likes. Every
//! item fits a block alone ([STATE_ITEM_MAX]), so it can always be put.

use crate::{ClockKey, ItemNum, RelationSet, TrackId};
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use shrink_wrap::prelude::*;

use crate::state::{DefinitionKind, ItemHash, Slot};
use crate::{
    Block, BlockId, BlockKind, BlockWriter, CRC_LEN, ClockMap, ClockNum, ClockOrigin, ClockSpec,
    ClockTime, Describe, HEADER_LEN, Items, MAX_BLOCK_CLOCKS, MAX_BLOCK_LEN, MAX_CLOCK_TABLE,
    MAX_RECORD_HEAD, MapSegment, RecordTrack, StatePolicy, TrackNum, TrackSpec,
};

/// Bytes of a record block before its records: header, the longest clock table and CRC.
pub(crate) const EMPTY_LEN: usize = HEADER_LEN + MAX_CLOCK_TABLE + CRC_LEN;

/// Longest item a definition or a state record may have: one that long fills a block alone.
pub const STATE_ITEM_MAX: usize = MAX_BLOCK_LEN - EMPTY_LEN - MAX_RECORD_HEAD;

/// Why a definition, a put or a block is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DescribeError {
    /// A definition doesn't encode.
    Encode(shrink_wrap::Error),
    /// A track-0 item doesn't read as a [Describe].
    Decode(shrink_wrap::Error),
    /// A [Describe] variant this writer doesn't know: it can't check it.
    Unknown,
    /// A track's dimensions aren't in ascending order of name, or a name repeats.
    Dimensions { track: TrackNum },
    /// A track or map names a clock that isn't open.
    UnknownClock { clock: ClockNum },
    /// A clock's resolution is under one tick (`resolution · rate < 1`), or not a time step at all.
    Resolution { clock: ClockNum },
    /// A record, chunk, summary or put on a track that isn't open.
    UnknownTrack { track: TrackNum },
    /// A record on another clock than its track's.
    WrongClock {
        track: TrackNum,
        clock: ClockNum,
        track_clock: ClockNum,
    },
    /// A map that isn't strictly increasing over its segments.
    MapNotIncreasing { from: ClockNum, to: ClockNum },
    /// An [Epoch](crate::ClockOrigin::Epoch) clock without its epoch link: a map from it to the previous epoch's
    /// clock ([ClockMap::is_epoch_link](crate::ClockMap::is_epoch_link)).
    EpochUnmapped { clock: ClockNum },
    /// A record longer than a block can be, or a definition or state item longer than [STATE_ITEM_MAX].
    TooLong { len: usize },
    /// A producer or layout definition: those are the writer's own ([crate::WriterOptions]).
    WriterOwned,
    /// A track or clock definition given with its number: [Definitions::define_track] and
    /// [Definitions::define_clock] number them.
    Numbered,
    /// Every track or clock number is taken.
    NumbersSpent,
    /// A track's relations break a rule ([TrackDef::relations_canonical](crate::TrackDef::relations_canonical)).
    Relations { track: TrackNum },
    /// A relation's name of `len` bytes, longer than [RELATION_NAME_MAX](crate::RELATION_NAME_MAX).
    RelationName { track: TrackNum, len: usize },
    /// A track declares a relation of a kind this crate doesn't write yet: index-only values, a postings or
    /// dictionary index ([TrackDef::relations_built](crate::TrackDef::relations_built)).
    RelationNotBuilt { track: TrackNum },
    /// A track's kind breaks a rule ([TrackDef::check_kind](crate::TrackDef::check_kind)).
    Kind {
        track: TrackNum,
        error: crate::SeriesError,
    },
    /// A [Schema](crate::Schema) whose hash isn't BLAKE3 of its bytes, or whose media type isn't valid.
    Schema,
    /// A track's items are of a kind this crate doesn't know, or break a rule
    /// ([TrackDef::items_valid](crate::TrackDef::items_valid)).
    Items { track: TrackNum },
    /// A record or an events chunk on a series track, or a series chunk on an events track.
    WrongKind { track: TrackNum },
    /// An events chunk whose relation columns are not one per stored relation of its track, or one in a codec its
    /// relation doesn't take (a monotone relation's values in [DURATIONS](crate::times::DURATIONS)).
    RelationColumns { track: TrackNum },
    /// An events chunk's column of relation `relation` doesn't read for every item of the chunk.
    RelationColumnRead {
        track: TrackNum,
        relation: u16,
        error: crate::times::TimesError,
    },
    /// An events chunk whose first value of monotone relation `relation`, at item `item`, is below the newest value
    /// placed before it on the track ([RelationDef::monotone](crate::RelationDef::monotone)).
    RelationDecrease {
        track: TrackNum,
        relation: u16,
        item: ItemNum,
    },
    /// An events chunk of a track whose newest value of monotone relation `relation` lies in a chunk that doesn't read
    /// (damaged, kept by a resume or the last of the track in a piece before): the chunk can't be checked, so it is
    /// refused rather than written unchecked.
    RelationUnread { track: TrackNum, relation: u16 },
    /// A break accepted for relation `relation` of track `track`, which is not a monotone relation with stored values
    /// ([TapeWriter::accept_relation_break](crate::TapeWriter::accept_relation_break)).
    NotMonotone { track: TrackNum, relation: u16 },
    /// A chunk or summary whose head names track `track` with id `id`, not the id `defined` its held definition
    /// gives ([TrackDef::id](crate::TrackDef::id)).
    TrackId {
        track: TrackNum,
        id: TrackId,
        defined: TrackId,
    },
    /// An events chunk of a state track: its items are put one by one.
    StateChunk { track: TrackNum },
    /// A record of a state track in a caller's block: its items are put ([TapeWriter::put](crate::TapeWriter::put)).
    StateRecord { track: TrackNum },
    /// A put on a track whose records are events.
    NotState { track: TrackNum },
    /// A relayed definition other than the one held in its slot: a relay holds a block's definitions before it
    /// puts the block ([crate::TapeWriter::relay_block]).
    NotHeld { slot: Slot },
    /// A caller's record block holding definitions: those are kept in [Definitions] and put by the writer.
    DefinitionBlock,
    /// A [RecordRef] whose time doesn't fit its track: a data record without one, or a definition with one.
    RecordTime { track: RecordTrack },
    /// A block doesn't start: its clocks break a block's rules.
    Start(crate::BlockStartError),
    /// A record doesn't go into the block sized for it.
    Push(crate::PushError),
    /// A block doesn't read.
    Block(crate::BlockError),
}

impl core::fmt::Display for DescribeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Encode(e) => write!(f, "definition doesn't encode: {e:?}"),
            Self::Decode(e) => write!(f, "track-0 item doesn't read: {e:?}"),
            Self::Unknown => write!(f, "a definition of a kind this writer doesn't know"),
            Self::Dimensions { track } => {
                write!(
                    f,
                    "track {track}: dimensions not in ascending order of name"
                )
            }
            Self::Relations { track } => {
                write!(
                    f,
                    "track {track}: relations break a rule (names, values, monotone, index)"
                )
            }
            Self::RelationName { track, len } => write!(
                f,
                "track {track}: a relation name of {len} bytes, longer than {}",
                crate::RELATION_NAME_MAX
            ),
            Self::RelationNotBuilt { track } => write!(
                f,
                "track {track}: a relation of a kind not written yet (index-only values, postings, dictionary)"
            ),
            Self::UnknownClock { clock } => write!(f, "clock {clock} is not open"),
            Self::Resolution { clock } => {
                write!(f, "clock {clock}: resolution under one tick")
            }
            Self::UnknownTrack { track } => write!(f, "track {track} is not open"),
            Self::MapNotIncreasing { from, to } => {
                write!(
                    f,
                    "map from clock {from} to {to} is not strictly increasing"
                )
            }
            Self::EpochUnmapped { clock } => write!(
                f,
                "epoch clock {clock}: no epoch link (an exact offset map from it to the previous epoch's clock)"
            ),
            Self::WrongClock {
                track,
                clock,
                track_clock,
            } => write!(
                f,
                "track {track}: record on clock {clock}, the track counts clock {track_clock}"
            ),
            Self::TooLong { len } => write!(
                f,
                "item of {len} bytes is longer than a block, or than a state item may be ({STATE_ITEM_MAX})"
            ),
            Self::WriterOwned => write!(
                f,
                "the producer and the layout are the writer's own (WriterOptions), not definitions to hold"
            ),
            Self::Numbered => write!(
                f,
                "a track or clock is defined without a number: define_track and define_clock number it"
            ),
            Self::NumbersSpent => write!(f, "every track or clock number is taken"),
            Self::Kind { track, error } => write!(f, "track {track}: {error}"),
            Self::Schema => write!(
                f,
                "a schema whose hash isn't BLAKE3 of its bytes, or whose media type isn't valid"
            ),
            Self::Items { track } => write!(
                f,
                "track {track}: items of an unknown kind, a foreign format with a bad name, version, media type or \
                 schema hash, or numbers of a bad element"
            ),
            Self::WrongKind { track } => write!(
                f,
                "track {track}: a record or events chunk on a series track, or a series chunk on an events track"
            ),
            Self::RelationColumns { track } => write!(
                f,
                "track {track}: an events chunk's relation columns don't match its stored relations"
            ),
            Self::RelationColumnRead {
                track,
                relation,
                error,
            } => write!(
                f,
                "track {track}: the column of relation {relation} doesn't read: {error}"
            ),
            Self::RelationDecrease {
                track,
                relation,
                item,
            } => write!(
                f,
                "track {track}: item {item}'s value of monotone relation {relation} is below the item's before it"
            ),
            Self::NotMonotone { track, relation } => write!(
                f,
                "track {track}: relation {relation} is not a monotone relation with stored values"
            ),
            Self::RelationUnread { track, relation } => write!(
                f,
                "track {track}: the newest value of monotone relation {relation} is in a chunk that doesn't read"
            ),
            Self::TrackId { track, id, defined } => write!(
                f,
                "a chunk or summary of track {track} under id {id}: its definition's id is {defined}"
            ),
            Self::StateChunk { track } => write!(
                f,
                "track {track} is a state track: its items are put, not written in events chunks"
            ),
            Self::StateRecord { track } => write!(
                f,
                "track {track} is a state track: its items are put, not written in a caller's block"
            ),
            Self::NotHeld { slot } => write!(
                f,
                "a relayed definition in {slot:?} isn't the one held: hold the block's definitions first"
            ),
            Self::NotState { track } => {
                write!(
                    f,
                    "track {track} is not a state track: nothing of it is put"
                )
            }
            Self::DefinitionBlock => write!(
                f,
                "a caller's block holds definitions: keep them in Definitions, the writer puts them"
            ),
            Self::RecordTime { track } => write!(
                f,
                "record on track {track}: a data record has a time, a definition (track 0) none"
            ),
            Self::Start(e) => write!(f, "block doesn't start: {e:?}"),
            Self::Push(e) => write!(f, "record doesn't fit its block: {e:?}"),
            Self::Block(e) => write!(f, "block doesn't read: {e:?}"),
        }
    }
}

impl core::error::Error for DescribeError {}

/// What a writer needs of a held track to check its records and put its items.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TrackFacts {
    /// [TrackDef::clock](crate::TrackDef::clock).
    pub clock: ClockNum,
    /// [TrackDef::repeat](crate::TrackDef::repeat)'s policy, None for a stream.
    pub state: Option<StatePolicy>,
    /// The period of a series track, in ticks per sample; None for an events track.
    pub period: Option<crate::Ratio64>,
    /// Which relations, by index, keep their values in a column.
    pub stored: RelationSet,
    /// Which relations' values never decrease.
    pub monotone: RelationSet,
    /// The relation whose values' smallest and largest go into each chunk's index entry.
    pub range: Option<u16>,
    /// The tiles of a grid's fixed axes, whose tile number goes into each chunk's index entry;
    /// [TileLayout::SERIES](crate::TileLayout::SERIES) for a series or an events track.
    pub tiles: crate::TileLayout,
}

impl TrackFacts {
    /// The tile number a chunk or summary block of the track with tile coordinates `tile` goes into its index entry
    /// with; None for a series or events track, and for coordinates the grid doesn't hold.
    pub(crate) fn tile_number(&self, tile: &crate::TileCoords) -> Option<u128> {
        if self.tiles.axes() == 0 {
            return None;
        }
        self.tiles.pack(tile)
    }

    /// The facts of track definition `t`.
    pub(crate) fn of(t: &crate::TrackDef<'_>) -> Self {
        let relations = crate::times::StoredRelations::of(t);
        let range = t
            .relations
            .iter()
            .take(crate::RELATIONS_MAX)
            .position(|r| r.is_ok_and(|r| r.index == crate::RelationIndex::Range))
            .and_then(|i| u16::try_from(i).ok());
        Self {
            clock: t.clock,
            state: t.repeat.state(),
            period: t.series().map(|s| s.period),
            stored: relations.stored,
            monotone: relations.monotone,
            range,
            // a held definition was checked, its layout with it
            tiles: t
                .series()
                .and_then(|s| s.tile_layout().ok())
                .unwrap_or(crate::TileLayout::SERIES),
        }
    }
}

/// A bundle by its API hash, or a schema by its hash: what a track names that a definition of its own holds.
type Named = (DefinitionKind, Vec<u8>);

/// A held track: its facts and the bundles and schema it names.
#[derive(Clone, Debug, PartialEq, Eq)]
struct HeldTrack {
    facts: TrackFacts,
    bundles: Vec<Named>,
    /// [TrackDef::id](crate::TrackDef::id) and [TrackDef::owner](crate::TrackDef::owner)
    key: (TrackId, [u8; 32]),
}

/// Check that every relation column of the events chunk `c` reads, one value for each of its items.
pub(crate) fn relation_columns_read(c: &crate::Chunk<'_>) -> Result<(), DescribeError> {
    let head = c.head();
    for col in c
        .columns()
        .filter(|col| col.kind == crate::ColumnKind::Relation)
    {
        let error = |error| DescribeError::RelationColumnRead {
            track: head.track_number,
            relation: col.param,
            error,
        };
        // a column that reads holds one value per item: its length is the head's count
        crate::times::RelationColumn::open(c, col.param, |_| false).map_err(error)?;
    }
    Ok(())
}

/// `d` encoded, into a buffer grown until it fits.
pub fn encode(d: &Describe<'_>) -> Result<Vec<u8>, DescribeError> {
    let mut buf = vec![0u8; 1024];
    loop {
        match d.to_ww_bytes(&mut buf).map(<[u8]>::len) {
            Ok(len) => {
                buf.truncate(len);
                return Ok(buf);
            }
            Err(_) if buf.len() < MAX_BLOCK_LEN => {
                let n = buf.len().saturating_mul(2);
                buf.resize(n, 0);
            }
            Err(e) => return Err(DescribeError::Encode(e)),
        }
    }
}

/// Whether `c`'s resolution is at least one tick: `resolution · rate >= 1`, exactly where both are rational.
fn resolution_ok(c: &crate::ClockDef<'_>) -> bool {
    let Ok(ticks) = c.rate.magnitude().checked_mul(c.resolution.magnitude()) else {
        return false;
    };
    match ticks.to_ratio() {
        Some((num, den)) => num >= den,
        // irrational (π in it) or too wide for u128: never exactly 1, so the approximation decides
        None => ticks.checked_to_f64().is_some_and(|t| t >= 1.0),
    }
}

/// Check what one definition says by itself.
///
/// A track's dimensions, items and kind, a clock's resolution, a map's order. A definition of a kind this crate
/// doesn't know is refused; a track whose [items](crate::TrackDef::items) are of such a kind is well formed when it
/// is an events track, its items stored as given.
pub fn check_definition(d: &Describe<'_>) -> Result<(), DescribeError> {
    match d {
        Describe::Track(t) => {
            if !t.dimensions_canonical() {
                return Err(DescribeError::Dimensions { track: t.number });
            }
            let long = t
                .relations
                .iter()
                .filter_map(Result::ok)
                .map(|r| r.name.len())
                .find(|len| *len > crate::RELATION_NAME_MAX);
            if let Some(len) = long {
                return Err(DescribeError::RelationName {
                    track: t.number,
                    len,
                });
            }
            if !t.relations_canonical() {
                return Err(DescribeError::Relations { track: t.number });
            }
            if !t.items_valid() && !matches!(t.items, Items::Unknown(_)) {
                return Err(DescribeError::Items { track: t.number });
            }
            t.check_kind().map_err(|error| DescribeError::Kind {
                track: t.number,
                error,
            })
        }
        Describe::Clock(c) if !resolution_ok(c) => {
            Err(DescribeError::Resolution { clock: c.number })
        }
        Describe::Schema(schema) if !schema.is_valid() => Err(DescribeError::Schema),
        Describe::Map(m) if !m.is_increasing() => Err(DescribeError::MapNotIncreasing {
            from: m.from,
            to: m.to,
        }),
        Describe::Unknown(_) => Err(DescribeError::Unknown),
        _ => Ok(()),
    }
}

/// The bundles track `t` names by API hash (its API's, its item's and its series' sample type's), then the schema of
/// its item encoding; empty hashes (not known) left out.
fn track_bundles(t: &crate::TrackDef<'_>) -> Vec<Named> {
    let (api, item, schema) = match &t.items {
        Items::WireWeaver { bundle, item, .. } => {
            (Some(bundle), item.as_ref().map(|i| &i.bundle), None)
        }
        Items::ShrinkWrap(ty) => (None, Some(&ty.bundle), None),
        Items::Numbers { unit, .. } => (None, unit.as_ref().map(|u| &u.bundle), None),
        Items::Foreign(f) => (None, None, Some(&f.schema)),
        Items::RawBytes | Items::Unknown(_) => (None, None, None),
    };
    let mut out: Vec<Named> = Vec::new();
    let bundles = [api, item].into_iter().flatten();
    let schemas = schema.into_iter();
    for (kind, hash) in bundles
        .map(|h| (DefinitionKind::Bundle, h))
        .chain(schemas.map(|h| (DefinitionKind::Schema, h)))
    {
        let h = (kind, hash.as_slice().to_vec());
        if !h.1.is_empty() && !out.contains(&h) {
            out.push(h);
        }
    }
    out
}

/// What a batch of definitions adds over a [Definitions], while it is checked.
#[derive(Default)]
struct Layer {
    clocks: BTreeMap<ClockNum, bool>,
    /// Maps by (from, to), and whether each is an epoch link
    maps: BTreeMap<(ClockNum, ClockNum), bool>,
}

/// A definition held: its item, shared with whoever puts it, and the item's hash, computed once when it is defined.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Held {
    pub item: Arc<[u8]>,
    pub hash: ItemHash,
}

/// Which number stands for which track and clock: what [Definitions] numbers new ones by.
///
/// A track is known by its identity ([TrackDef::id](crate::TrackDef::id) and owner), a clock by its key. A number
/// once given, or read from a resumed piece ([crate::Resumed::numbers]), is never given to another: defining the
/// same identity again, also after [Definitions::close], gives it back, and a new one takes the lowest number not
/// taken.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Numbers {
    tracks: BTreeMap<(TrackId, [u8; 32]), TrackNum>,
    track_keys: BTreeMap<TrackNum, (TrackId, [u8; 32])>,
    clocks: BTreeMap<ClockKey, ClockNum>,
    clock_keys: BTreeMap<ClockNum, ClockKey>,
}

impl Numbers {
    /// None taken.
    pub fn new() -> Self {
        Self::default()
    }

    /// The number of the track of id `id` and owner `owner`.
    pub fn track(&self, id: TrackId, owner: [u8; 32]) -> Option<TrackNum> {
        self.tracks.get(&(id, owner)).copied()
    }

    /// The number of the clock of key `key`.
    pub fn clock(&self, key: ClockKey) -> Option<ClockNum> {
        self.clocks.get(&key).copied()
    }

    /// Track `number` stands for the track of id `id` and owner `owner`; a track it stood for before loses it.
    pub fn take_track(&mut self, number: TrackNum, id: TrackId, owner: [u8; 32]) {
        if let Some(old) = self.track_keys.insert(number, (id, owner)) {
            self.tracks.remove(&old);
        }
        if let Some(old) = self.tracks.insert((id, owner), number)
            && old != number
        {
            self.track_keys.remove(&old);
        }
    }

    /// Clock `number` stands for the clock of key `key`; a clock it stood for before loses it.
    pub fn take_clock(&mut self, number: ClockNum, key: ClockKey) {
        if let Some(old) = self.clock_keys.insert(number, key) {
            self.clocks.remove(&old);
        }
        if let Some(old) = self.clocks.insert(key, number)
            && old != number
        {
            self.clock_keys.remove(&old);
        }
    }

    /// Take every number `later` gives, over those taken here: a number it gives another track or clock loses the
    /// one it stood for here.
    pub(crate) fn take_all(&mut self, later: &Self) {
        for (number, (id, owner)) in &later.track_keys {
            self.take_track(*number, *id, *owner);
        }
        for (number, key) in &later.clock_keys {
            self.take_clock(*number, *key);
        }
    }

    /// The number of the track of id `id` and owner `owner`, else the lowest not taken.
    fn track_for(&self, id: TrackId, owner: [u8; 32]) -> Result<TrackNum, DescribeError> {
        if let Some(n) = self.track(id, owner) {
            return Ok(n);
        }
        lowest_free(self.track_keys.keys().map(|n| n.get()))
            .and_then(TrackNum::try_new)
            .ok_or(DescribeError::NumbersSpent)
    }

    /// The number of the clock of key `key`, else the lowest not taken.
    fn clock_for(&self, key: ClockKey) -> Result<ClockNum, DescribeError> {
        if let Some(n) = self.clock(key) {
            return Ok(n);
        }
        lowest_free(self.clock_keys.keys().map(|n| n.get()))
            .and_then(ClockNum::try_new)
            .ok_or(DescribeError::NumbersSpent)
    }
}

/// The lowest number from 1 up that `taken`, ascending, doesn't hold; None when all are.
fn lowest_free(taken: impl Iterator<Item = u32>) -> Option<u32> {
    let mut next = 1u32;
    for n in taken {
        if n != next {
            break;
        }
        next = next.checked_add(1)?;
    }
    Some(next)
}

/// The definitions an application works with, see the [module docs](self).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Definitions {
    /// Every definition's item by slot: tracks, clocks, maps, bundles
    defs: BTreeMap<Slot, Held>,
    tracks: BTreeMap<TrackNum, HeldTrack>,
    /// Clocks held, and whether each is of origin Epoch
    clocks: BTreeMap<ClockNum, bool>,
    /// Maps held that are epoch links, by (from, to)
    links: BTreeSet<(ClockNum, ClockNum)>,
    /// Bundles' slots by API hash, schemas' by hash
    bundles: BTreeMap<Named, Slot>,
    /// Bytes of the items held
    bytes: usize,
    /// The numbers given to tracks and clocks
    numbers: Numbers,
}

impl Definitions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Definitions held.
    pub fn len(&self) -> usize {
        self.defs.len()
    }

    /// Whether none is held.
    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }

    /// Bytes of the items held.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// No definitions, numbering tracks and clocks as `numbers` does: those a resumed piece holds
    /// ([crate::Resumed::numbers]), so a track or clock defined again gets its number there.
    pub fn with_numbers(numbers: Numbers) -> Self {
        Self {
            numbers,
            ..Self::default()
        }
    }

    /// Which number stands for which track and clock.
    pub fn numbers(&self) -> &Numbers {
        &self.numbers
    }

    /// Define clock `c` and give its number: the one its key has ([Numbers]), else the lowest not taken. It is held
    /// as [Self::define] holds a definition, replacing the one of the same key; an epoch clock comes with its link
    /// through [Self::define_clock_with_maps].
    pub fn define_clock(&mut self, c: ClockSpec<'_>) -> Result<ClockNum, DescribeError> {
        self.define_clock_with_maps(c, &[])
    }

    /// Define clock `c` as [Self::define_clock] does, with a map from it to each held clock `to` of `maps` over its
    /// segments, checked as one batch: an [Epoch](ClockOrigin::Epoch) clock with its epoch link.
    pub fn define_clock_with_maps(
        &mut self,
        c: ClockSpec<'_>,
        maps: &[(ClockNum, &[MapSegment])],
    ) -> Result<ClockNum, DescribeError> {
        let number = self.numbers.clock_for(c.key)?;
        let mut batch = vec![Describe::Clock(c.numbered(number))];
        batch.extend(maps.iter().map(|(to, segments)| {
            Describe::Map(ClockMap {
                from: number,
                to: *to,
                segments: RefVec::Slice { slice: segments },
            })
        }));
        self.hold(&batch)?;
        Ok(number)
    }

    /// Define track `t` and give its number: the one its identity has ([Numbers]), else the lowest not taken. It is
    /// held as [Self::define] holds a definition, replacing the one of the same identity; its clock is held. A refused
    /// track takes no number; the error names the one it would have had.
    pub fn define_track(&mut self, t: TrackSpec<'_>) -> Result<TrackNum, DescribeError> {
        let number = self.numbers.track_for(t.id(), t.owner)?;
        self.hold(&[Describe::Track(t.numbered(number))])?;
        Ok(number)
    }

    /// Hold `d` in its slot ([Slot]: a map, a bundle or a schema by its hash), replacing the one there, checked
    /// against those held ([Self::check_batch]): a map names clocks held. A track or a clock is numbered by
    /// [Self::define_track] and [Self::define_clock] ([DescribeError::Numbered]). Nothing is written: a writer puts it
    /// before data that needs it. A typed item isn't held ([crate::TapeWriter::put_item] puts it).
    pub fn define(&mut self, d: &Describe<'_>) -> Result<(), DescribeError> {
        self.define_all(core::slice::from_ref(d))
    }

    /// Hold each of `defs` as [Self::define] does, checked as one batch in any order; none is held when one is
    /// refused.
    pub fn define_all(&mut self, defs: &[Describe<'_>]) -> Result<(), DescribeError> {
        if defs
            .iter()
            .any(|d| matches!(d, Describe::Track(_) | Describe::Clock(_)))
        {
            return Err(DescribeError::Numbered);
        }
        self.hold(defs)
    }

    /// Hold `defs`, checked as one batch.
    fn hold(&mut self, defs: &[Describe<'_>]) -> Result<(), DescribeError> {
        let items = defs.iter().map(encode).collect::<Result<Vec<_>, _>>()?;
        if let Some(t) = defs.iter().find_map(|d| match d {
            Describe::Track(t) if !t.relations_built() => Some(t),
            _ => None,
        }) {
            return Err(DescribeError::RelationNotBuilt { track: t.number });
        }
        if let Some(t) = defs.iter().find_map(|d| match d {
            Describe::Track(t) if matches!(t.items, Items::Unknown(_)) => Some(t),
            _ => None,
        }) {
            return Err(DescribeError::Items { track: t.number });
        }
        self.check_batch(defs.iter().zip(&items).map(|(d, i)| (d, i.as_slice())))?;
        for (d, item) in defs.iter().zip(&items) {
            self.insert(d, item);
        }
        Ok(())
    }

    /// Hold the definitions of the record block `b` (its body readable: [Block::decode]), as a capture tool takes a
    /// producer's: its track-0 records, checked as one batch; a producer, a layout, a typed item or a kind this crate
    /// doesn't know is passed over. How many are held.
    pub fn take_block(&mut self, b: &Block<'_>) -> Result<usize, DescribeError> {
        let mut batch: Vec<(Describe<'_>, &[u8])> = Vec::new();
        for r in b.records() {
            let r = r.map_err(DescribeError::Block)?;
            if !r.track.is_definitions() {
                continue;
            }
            let d = Describe::from_ww_bytes(r.item).map_err(DescribeError::Decode)?;
            if is_held(&d) {
                batch.push((d, r.item));
            }
        }
        self.check_batch(batch.iter().map(|(d, item)| (d, *item)))?;
        for (d, item) in &batch {
            self.insert(d, item);
        }
        Ok(batch.len())
    }

    /// Track `number`'s facts.
    pub fn track(&self, number: TrackNum) -> Option<TrackFacts> {
        self.tracks.get(&number).map(|t| t.facts)
    }

    /// The id and owner of held track `number`, as its definition says: all zeros for the piece's owner.
    #[cfg(feature = "alloc")]
    pub fn track_key(&self, number: TrackNum) -> Option<(TrackId, [u8; 32])> {
        self.tracks.get(&number).map(|t| t.key)
    }

    /// The held tracks' numbers, ascending.
    pub fn tracks(&self) -> impl Iterator<Item = TrackNum> + '_ {
        self.tracks.keys().copied()
    }

    /// Whether clock `number` is held.
    pub fn has_clock(&self, number: ClockNum) -> bool {
        self.clocks.contains_key(&number)
    }

    /// The item of the definition held in `slot`.
    pub fn get(&self, slot: &Slot) -> Option<&[u8]> {
        self.defs.get(slot).map(|h| &*h.item)
    }

    /// The definition held in `slot`, its item shared and its hash: what a writer or relay puts without copying or
    /// hashing it.
    pub fn held(&self, slot: &Slot) -> Option<&Held> {
        self.defs.get(slot)
    }

    /// The bundle of API hash `hash`: its slot and item.
    pub fn bundle(&self, hash: &[u8]) -> Option<(Slot, &[u8])> {
        self.named(DefinitionKind::Bundle, hash)
    }

    /// The [Schema](crate::Schema) of hash `hash`: its slot and item.
    pub fn schema(&self, hash: &[u8]) -> Option<(Slot, &[u8])> {
        self.named(DefinitionKind::Schema, hash)
    }

    /// The bundle or schema of `kind` and `hash`: its slot and item.
    fn named(&self, kind: DefinitionKind, hash: &[u8]) -> Option<(Slot, &[u8])> {
        let slot = self.bundles.get(&(kind, hash.to_vec()))?;
        Some((*slot, &*self.defs.get(slot)?.item))
    }

    /// Every definition held, by slot.
    pub fn definitions(&self) -> impl Iterator<Item = (&Slot, &[u8])> {
        self.defs.iter().map(|(s, h)| (s, &*h.item))
    }

    /// How items of state track `track` are repeated; None for a stream or a track not held.
    pub fn policy(&self, track: TrackNum) -> Option<StatePolicy> {
        self.track(track)?.state
    }

    /// The definitions data of track `number` needs, in the order a writer puts them: the clocks it counts through
    /// (its own, then those its clock maps to, on and on), the maps between them, its bundles, then its own; only
    /// those held. Empty for a track not held.
    pub fn needs(&self, number: TrackNum) -> Vec<Slot> {
        let Some(t) = self.tracks.get(&number) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut seen = BTreeSet::new();
        let mut todo = vec![t.facts.clock];
        let mut maps = Vec::new();
        while let Some(c) = todo.pop() {
            if !seen.insert(c) {
                continue;
            }
            out.push(Slot::Clock { number: c });
            for (slot, _) in self.maps_from(c) {
                if let Slot::Map { to, .. } = *slot {
                    maps.push(*slot);
                    todo.push(to);
                }
            }
        }
        out.extend(maps);
        out.extend(
            t.bundles
                .iter()
                .filter_map(|h| self.bundles.get(h))
                .copied(),
        );
        out.push(Slot::Track { number });
        out.retain(|s| self.defs.contains_key(s));
        out
    }

    /// The maps held from clock `c`.
    fn maps_from(&self, c: ClockNum) -> impl Iterator<Item = (&Slot, &Held)> {
        self.defs.range(
            Slot::Map {
                from: c,
                to: ClockNum::MIN,
            }..=Slot::Map {
                from: c,
                to: ClockNum::MAX,
            },
        )
    }

    /// Check a batch of definitions, in any order, before they are held: each by itself ([check_definition]) and its
    /// item at most [STATE_ITEM_MAX] long, no producer or layout ([DescribeError::WriterOwned]); then, with the whole
    /// batch over those held, every clock a track or map names is held and every epoch clock touched has its link.
    pub fn check_batch<'a, 'd: 'a>(
        &self,
        batch: impl IntoIterator<Item = (&'a Describe<'d>, &'a [u8])>,
    ) -> Result<(), DescribeError> {
        let mut layer = Layer::default();
        let mut named = Vec::new();
        let mut touched = BTreeSet::new();
        for (d, item) in batch {
            check_definition(d)?;
            if matches!(d, Describe::Producer(_) | Describe::Layout(_)) {
                return Err(DescribeError::WriterOwned);
            }
            let len = item.len();
            if len > STATE_ITEM_MAX {
                return Err(DescribeError::TooLong { len });
            }
            match d {
                Describe::Clock(c) => {
                    layer
                        .clocks
                        .insert(c.number, c.origin == ClockOrigin::Epoch);
                    touched.insert(c.number);
                }
                Describe::Map(m) => {
                    layer.maps.insert((m.from, m.to), m.is_epoch_link());
                    touched.insert(m.from);
                }
                _ => {}
            }
            named.push(d);
        }
        for d in named {
            let clocks = match d {
                Describe::Track(t) => [Some(t.clock), None],
                Describe::Map(m) => [Some(m.from), Some(m.to)],
                _ => [None, None],
            };
            for clock in clocks.into_iter().flatten() {
                if !layer.clocks.contains_key(&clock) && !self.clocks.contains_key(&clock) {
                    return Err(DescribeError::UnknownClock { clock });
                }
            }
        }
        for c in touched {
            let epoch = layer.clocks.get(&c).or_else(|| self.clocks.get(&c));
            if epoch == Some(&true) && !self.linked(&layer, c) {
                return Err(DescribeError::EpochUnmapped { clock: c });
            }
        }
        Ok(())
    }

    /// Whether an epoch link maps from clock `c`, with `layer` over the open maps.
    fn linked(&self, layer: &Layer, c: ClockNum) -> bool {
        let range = (c, ClockNum::MIN)..=(c, ClockNum::MAX);
        if layer.maps.range(range.clone()).any(|(_, link)| *link) {
            return true;
        }
        self.links
            .range(range)
            .any(|key| !layer.maps.contains_key(key))
    }

    /// Hold the definition `d` read from `item` in its slot, replacing the one there; a bundle by its API hash. Not
    /// checked ([Self::check_batch]); a producer, a layout, a typed item or a kind this crate doesn't know is not
    /// held. Its slot, None when not held.
    fn insert(&mut self, d: &Describe<'_>, item: &[u8]) -> Option<Slot> {
        let hash = ItemHash::of(item);
        let slot = Slot::of_definition(d, || hash);
        match d {
            Describe::Track(t) => {
                self.numbers.take_track(t.number, t.id(), t.owner);
                let bundles = track_bundles(t);
                self.tracks.insert(
                    t.number,
                    HeldTrack {
                        facts: TrackFacts::of(t),
                        bundles,
                        key: (t.id(), t.owner),
                    },
                );
            }
            Describe::Clock(c) => {
                self.numbers.take_clock(c.number, c.key);
                self.clocks.insert(c.number, c.origin == ClockOrigin::Epoch);
            }
            Describe::Map(m) => {
                let key = (m.from, m.to);
                if m.is_epoch_link() {
                    self.links.insert(key);
                } else {
                    self.links.remove(&key);
                }
            }
            Describe::Bundle { hash, .. } => {
                let h = (DefinitionKind::Bundle, hash.as_slice().to_vec());
                if let Some(old) = self.bundles.insert(h, slot) {
                    self.remove(&old);
                }
            }
            Describe::Schema(schema) => {
                let h = (DefinitionKind::Schema, schema.hash.as_slice().to_vec());
                if let Some(old) = self.bundles.insert(h, slot) {
                    self.remove(&old);
                }
            }
            Describe::Producer(_)
            | Describe::Layout(_)
            | Describe::Item(_)
            | Describe::Unknown(_) => {
                return None;
            }
        }
        let held = Held {
            item: Arc::from(item),
            hash,
        };
        let old = self.defs.insert(slot, held).map_or(0, |o| o.item.len());
        self.bytes = self.bytes.saturating_sub(old).saturating_add(item.len());
        Some(slot)
    }

    /// Drop `slot`'s definition, its bytes no longer held.
    fn remove(&mut self, slot: &Slot) {
        if let Some(old) = self.defs.remove(slot) {
            self.bytes = self.bytes.saturating_sub(old.item.len());
        }
    }

    /// Drop track `number`: its definition goes, and with it the clocks, maps and bundles no held track names any
    /// more. Nothing is written. False when it wasn't held.
    pub fn close(&mut self, number: TrackNum) -> bool {
        let Some(t) = self.tracks.remove(&number) else {
            return false;
        };
        self.remove(&Slot::Track { number });
        let named: BTreeSet<&Named> = self.tracks.values().flat_map(|t| &t.bundles).collect();
        let gone: Vec<Named> = t
            .bundles
            .into_iter()
            .filter(|h| !named.contains(h))
            .collect();
        for h in gone {
            if let Some(slot) = self.bundles.remove(&h) {
                self.remove(&slot);
            }
        }
        // the clocks open tracks count through, and on through the maps from them
        let mut kept = BTreeSet::new();
        let mut todo: Vec<ClockNum> = self.tracks.values().map(|t| t.facts.clock).collect();
        while let Some(c) = todo.pop() {
            if kept.insert(c) {
                todo.extend(self.maps_from(c).filter_map(|(s, _)| match s {
                    Slot::Map { to, .. } => Some(*to),
                    _ => None,
                }));
            }
        }
        let clocks: Vec<ClockNum> = self.clocks.keys().copied().collect();
        for c in clocks.into_iter().filter(|c| !kept.contains(c)) {
            self.clocks.remove(&c);
            self.remove(&Slot::Clock { number: c });
            let maps: Vec<Slot> = self.maps_from(c).map(|(s, _)| *s).collect();
            for m in maps {
                self.remove(&m);
                if let Slot::Map { from, to } = m {
                    self.links.remove(&(from, to));
                }
            }
        }
        true
    }

    /// Check a caller's block of data (its body readable: [Block::decode]) before a writer writes it: no record of
    /// track 0 ([DescribeError::DefinitionBlock]), every record on a held track that isn't a state track, on the
    /// track's clock; a chunk or summary of a held track.
    pub fn check_block(&self, b: &Block<'_>) -> Result<(), DescribeError> {
        if b.kind != BlockKind::Records {
            return self.check_track_block(b);
        }
        for r in b.records() {
            let r = r.map_err(DescribeError::Block)?;
            let (RecordTrack::Data(track), Some(at)) = (r.track, r.at) else {
                return Err(DescribeError::DefinitionBlock);
            };
            let t = self.check_on_track(track, at)?;
            if t.period.is_some() {
                return Err(DescribeError::WrongKind { track });
            }
            if t.state.is_some() {
                return Err(DescribeError::StateRecord { track });
            }
        }
        Ok(())
    }

    /// The facts of held track `track` when `at` is on its clock.
    pub fn check_on_track(
        &self,
        track: TrackNum,
        at: ClockTime,
    ) -> Result<TrackFacts, DescribeError> {
        let t = self
            .track(track)
            .ok_or(DescribeError::UnknownTrack { track })?;
        if at.clock != t.clock {
            return Err(DescribeError::WrongClock {
                track,
                clock: at.clock,
                track_clock: t.clock,
            });
        }
        Ok(t)
    }

    /// Check a chunk or summary block's track before a writer writes it: held and under its definition's id, an
    /// events chunk on an events track
    /// that isn't a state track with its relation columns as its definition says, a series chunk on a series track
    /// with its ticks on its grid, a series summary block of a tile of its grid (none for a series).
    fn check_track_block(&self, b: &Block<'_>) -> Result<(), DescribeError> {
        if let Some(c) = b.chunk() {
            let head = c.head();
            self.check_chunk_head(&head)?;
            if head.kind != BlockKind::Events {
                return Ok(());
            }
            let specs: Vec<crate::ColumnSpec> = c
                .columns()
                .map(|col| crate::ColumnSpec {
                    kind: col.kind,
                    codec: col.codec,
                    param: col.param,
                    len: col.bytes.len(),
                })
                .collect();
            self.check_relation_columns(head.track_number, &specs)?;
            relation_columns_read(&c)
        } else if let Some(head) = b.summary_head() {
            let track = head.track_number;
            let t = self.check_track_id(track, head.track_id)?;
            // a grid's series trees are its tiles', one per tile of the grid; an event tree has no tile
            if head.tree == crate::summary::Tree::Series && !t.tiles.holds(&head.tile) {
                return Err(DescribeError::Kind {
                    track,
                    error: crate::SeriesError::TileOutside {
                        first_item: head.first_item,
                    },
                });
            }
            Ok(())
        } else {
            Ok(())
        }
    }

    /// Check the relation columns of an events chunk of track `track` against its definition: one column per stored
    /// relation, none for another, a monotone relation's in [STARTS](crate::times::STARTS).
    pub fn check_relation_columns(
        &self,
        track: TrackNum,
        columns: &[crate::ColumnSpec],
    ) -> Result<(), DescribeError> {
        let t = self
            .track(track)
            .ok_or(DescribeError::UnknownTrack { track })?;
        let mut seen = RelationSet::EMPTY;
        for c in columns
            .iter()
            .filter(|c| c.kind == crate::ColumnKind::Relation)
        {
            if !t.stored.contains(c.param) || seen.contains(c.param) {
                return Err(DescribeError::RelationColumns { track });
            }
            let monotone = t.monotone.contains(c.param);
            let codec_ok = c.codec == crate::times::STARTS
                || (!monotone && c.codec == crate::times::DURATIONS);
            if !codec_ok {
                return Err(DescribeError::RelationColumns { track });
            }
            seen.insert(c.param);
        }
        if seen != t.stored {
            return Err(DescribeError::RelationColumns { track });
        }
        Ok(())
    }

    /// Check that track `track` is held and named by its definition's id `id` in a chunk or summary head.
    fn check_track_id(&self, track: TrackNum, id: TrackId) -> Result<TrackFacts, DescribeError> {
        let held = self
            .tracks
            .get(&track)
            .ok_or(DescribeError::UnknownTrack { track })?;
        let defined = held.key.0;
        if id != defined {
            return Err(DescribeError::TrackId { track, id, defined });
        }
        Ok(held.facts)
    }

    /// Check a chunk head's track before a writer writes it, as [Self::check_block] does: held, under its
    /// definition's id, of its kind.
    pub fn check_chunk_head(&self, head: &crate::ChunkHead) -> Result<(), DescribeError> {
        let track = head.track_number;
        let t = self.check_track_id(track, head.track_id)?;
        match (head.kind, t.period) {
            (BlockKind::Events, None) if t.state.is_some() => {
                Err(DescribeError::StateChunk { track })
            }
            (BlockKind::Events, None) => Ok(()),
            (BlockKind::Series, Some(period)) => {
                let tile = match (
                    t.tiles.holds(&head.tile),
                    head.level <= t.tiles.overview_levels(),
                ) {
                    (true, true) => Ok(()),
                    (false, _) => Err(crate::SeriesError::TileOutside {
                        first_item: head.first_item,
                    }),
                    (true, false) => Err(crate::SeriesError::LevelOutside {
                        first_item: head.first_item,
                        level: head.level,
                    }),
                };
                tile.and_then(|()| crate::SeriesGrid::of_chunk(head, period).map(|_| ()))
                    .map_err(|error| DescribeError::Kind { track, error })
            }
            _ => Err(DescribeError::WrongKind { track }),
        }
    }
}

/// One record to write: its track, its time (None on track 0) and its item.
#[derive(Copy, Clone, Debug)]
pub struct RecordRef<'a> {
    pub track: RecordTrack,
    pub at: Option<ClockTime>,
    pub item: &'a [u8],
}

impl<'a> RecordRef<'a> {
    /// A record of data track `track` at `at`.
    pub const fn data(track: TrackNum, at: ClockTime, item: &'a [u8]) -> Self {
        Self {
            track: RecordTrack::Data(track),
            at: Some(at),
            item,
        }
    }

    /// A definition: a record of track 0, no time.
    pub const fn definition(item: &'a [u8]) -> Self {
        Self {
            track: RecordTrack::Definitions,
            at: None,
            item,
        }
    }
}

/// Write `records` as record blocks of at most `block_len_max` bytes (a longer record gets a block of its own) with
/// header flags `flags`, each numbered by `next_id`, each handed to `out` as it is made.
pub fn write_records<'a, E: From<DescribeError>>(
    records: &[RecordRef<'a>],
    block_len_max: usize,
    flags: u16,
    mut next_id: impl FnMut() -> BlockId,
    mut out: impl FnMut(&[u8]) -> Result<(), E>,
) -> Result<(), E> {
    let mut rest = records;
    let mut buf = Vec::new();
    while !rest.is_empty() {
        let (taken, size, clocks) = next_block(rest, block_len_max)?;
        let (now, later) = rest.split_at_checked(taken).unwrap_or((rest, &[]));
        buf.clear();
        buf.resize(size, 0);
        let mut w = BlockWriter::new(&mut buf, BlockKind::Records, next_id(), &clocks)
            .map_err(DescribeError::Start)?;
        for e in now {
            let pushed = match (e.track, e.at) {
                (RecordTrack::Data(track), Some(at)) => w.push(track, at, &[e.item]),
                (RecordTrack::Definitions, None) => {
                    w.push_describe(&[e.item]).map_err(crate::PushError::from)
                }
                (track, _) => return Err(DescribeError::RecordTime { track }.into()),
            };
            pushed.map_err(DescribeError::Push)?;
        }
        w.set_flags(flags);
        let len = w.finish();
        out(buf.get(..len).unwrap_or_default())?;
        rest = later;
    }
    Ok(())
}

/// The records of the next block from the start of `records`: how many (at least one), the block's bytes at most,
/// and its clocks.
fn next_block(
    records: &[RecordRef<'_>],
    block_len_max: usize,
) -> Result<(usize, usize, Vec<ClockNum>), DescribeError> {
    let mut size = EMPTY_LEN;
    let mut clocks = BTreeSet::new();
    let mut taken = 0usize;
    for e in records {
        // a record over usize::MAX bytes is too long anyway
        let r = e.item.len().saturating_add(MAX_RECORD_HEAD);
        let new_clock = e.at.is_some_and(|at| !clocks.contains(&at.clock));
        let fits = size.checked_add(r).is_some_and(|n| n <= block_len_max)
            && !(new_clock && clocks.len() == MAX_BLOCK_CLOCKS);
        if taken > 0 && !fits {
            break;
        }
        let too_long = DescribeError::TooLong { len: e.item.len() };
        if EMPTY_LEN.checked_add(r).is_none_or(|n| n > MAX_BLOCK_LEN) {
            return Err(too_long);
        }
        if let Some(at) = e.at {
            clocks.insert(at.clock);
        }
        size = size.checked_add(r).ok_or(too_long)?;
        taken = taken.saturating_add(1);
    }
    Ok((taken, size, clocks.into_iter().collect()))
}

/// Whether `d` is a kind [Definitions] holds (a track, clock, map or bundle).
pub(crate) fn is_held(d: &Describe<'_>) -> bool {
    !matches!(
        DefinitionKind::of(d),
        DefinitionKind::Item
            | DefinitionKind::Unknown(_)
            | DefinitionKind::Producer
            | DefinitionKind::Layout
    )
}
