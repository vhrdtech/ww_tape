//! What describe records say: the producer, clocks, clock maps, tracks, API bundles, schemas and typed items.

use exact_units::units::Item;
use exact_units::units::{Per, Second, Tick};
use exact_units::{Bytes, Count, MagnitudeOf, Ratio64, Rounding};
use shrink_wrap::prelude::*;

#[cfg(feature = "std")]
use crate::FullVersionOwned;
use crate::series::GridDef;
#[cfg(feature = "std")]
use crate::series::GridDefOwned;
use crate::series::SeriesValue;
use crate::{
    ClockKey, ClockNum, ElementDesc, FNV_OFFSET, FullVersion, SampleShape, TrackId, TrackKey,
    TrackNum, Version, fnv1a,
};

/// A definition: one per record on track 0, its item's whole bytes.
#[derive_shrink_wrap(owned(feature = "std"), derive(Debug, PartialEq, Clone), ww_repr = u4)]
pub enum Describe<'i> {
    Producer(Producer<'i>),
    Track(TrackDef<'i>),
    /// An API bundle (`ww_self` `ApiBundle` in its ShrinkWrap encoding) by its hash, the hash as the producer,
    /// a track's [Items::WireWeaver] and [TypeRef::bundle] know it (wire_weaver's `ApiHash`). Written once per hash in a describe
    /// set; a live feed may leave it out and let the consumer look the hash up.
    Bundle {
        hash: RefVec<'i, u8>,
        bytes: RefVec<'i, u8>,
    },
    Clock(ClockDef<'i>),
    Map(ClockMap<'i>),
    /// Any wire_weaver value, typed by its bundle and type path: a run record, a media reference, settings.
    Item(TypedItem<'i>),
    /// How the file's blocks are placed; a file that defines none is not aligned.
    Layout(Layout),
    /// A schema a [Foreign] format names by its hash.
    Schema(Schema<'i>),
}

/// How blocks are placed in a file or on a medium.
#[derive_shrink_wrap(
    borrowed,
    owned(feature = "std"),
    derive(Debug, PartialEq, Eq, Clone, Copy),
    validate = Layout::validate
)]
pub struct Layout {
    /// 0, or [ALIGNMENT]: every chunk block's payload starts at a multiple of it in the file, its head and smaller
    /// blocks filling the gap before it. Series files are aligned, event files not.
    pub alignment: Bytes<u32>,
    /// Longest stretch a writer may leave blank or damaged and go on after (a failed program, a bad block, a torn
    /// unit): a reader looks this far past a blank or bad stretch for the run's next block before it takes the
    /// stretch's start as the end of data. 0 on files: the first such stretch is the end.
    pub gap_max: Bytes<u32>,
    /// Where a writer resumes after such a stretch: at a multiple of it in the file or medium, 1 and up (a sector,
    /// a NAND page). A reader probes these places and the first non-blank byte.
    pub resume_align: Bytes<u32>,
    /// How track 0, the definitions, is repeated: what [TrackRepeat::State] says of a state track.
    pub definitions: StatePolicy,
}

/// The alignment of an aligned file: 4 KiB, a page and a direct I/O block.
pub const ALIGNMENT: u32 = 4096;
/// [Layout::gap_max] of an SD card: a 4 MiB segment, part of an allocation unit (SD 9.10 §4.13.1 has them up to
/// 64 MB); a reader reads at most 8192 sector headers past a blank or bad stretch.
pub const SD_GAP_MAX: u32 = 4 << 20;
/// Largest [Layout::gap_max] a valid layout has: 64 MiB, an SD card's largest allocation unit and two erase blocks
/// of large NAND.
pub const GAP_MAX_MAX: u32 = 64 << 20;
/// Most resume places in one gap, `gap_max / resume_align` of a valid layout: past a blank or bad stretch a reader
/// reads at most this many block headers, and the first non-blank byte's.
pub const GAP_PROBES_MAX: u32 = 8192;
/// Most blocks past one blank or bad stretch a reader checks whole (CRC over up to [crate::MAX_BLOCK_LEN] each):
/// those whose header says they go on with the data. Past them the stretch is the end of data.
pub const GAP_CHECKS_MAX: u32 = 4;
/// [Layout::resume_align] of an SD card: a 512-byte sector.
pub const SD_RESUME_ALIGN: u32 = 512;

impl Layout {
    /// Not aligned, a file: no gaps.
    pub const UNALIGNED: Self = Self {
        alignment: Bytes::new(0),
        gap_max: Bytes::new(0),
        resume_align: Bytes::new(1),
        definitions: StatePolicy::DEFAULT,
    };
    /// Chunk payloads on 4 KiB boundaries, a file: no gaps.
    pub const ALIGNED: Self = Self {
        alignment: Bytes::new(ALIGNMENT),
        ..Self::UNALIGNED
    };

    /// This layout on an SD card or eMMC: gaps up to [SD_GAP_MAX], resumed on sectors.
    pub const fn on_sd_card(self) -> Self {
        Self {
            gap_max: Bytes::new(SD_GAP_MAX),
            resume_align: Bytes::new(SD_RESUME_ALIGN),
            ..self
        }
    }

    /// This layout on raw NAND with erase blocks of `erase_block` and pages of `page` bytes: gaps up to two erase
    /// blocks, resumed on pages (ONFI 6.0 §3.1).
    ///
    /// A writer on raw NAND programs each page once, in page order: at a sync it pads the rest of the page with FFh
    /// and starts the next block on the next page, never appending into a programmed page. A reader takes the padding
    /// as a blank stretch followed by the run's next block, a gap, not the end; the cost is up to one page per sync.
    /// On MLC NAND a power cut can also corrupt an earlier, synced page paired with the one being programmed: keeping
    /// paired pages of unsynced writes apart from synced data is the medium layer's, not the format's.
    pub const fn on_raw_nand(self, erase_block: u32, page: u32) -> Self {
        Self {
            gap_max: Bytes::new(erase_block.saturating_mul(2)),
            resume_align: Bytes::new(page),
            ..self
        }
    }

    /// This layout with the definitions repeated as `policy` says.
    pub const fn with_definitions(self, policy: StatePolicy) -> Self {
        Self {
            definitions: policy,
            ..self
        }
    }

    /// Whether a reader takes this layout: alignment 0 or [ALIGNMENT], `resume_align` 1 and up, `gap_max` at most
    /// [GAP_MAX_MAX] and at most [GAP_PROBES_MAX] resume places in it. A describe record of another layout doesn't
    /// decode.
    pub const fn is_valid(&self) -> bool {
        let a = self.alignment.get();
        let (gap, align) = (self.gap_max.get(), self.resume_align.get());
        (a == 0 || a == ALIGNMENT)
            && align >= 1
            && gap <= GAP_MAX_MAX
            && match gap.checked_div(align) {
                Some(places) => places <= GAP_PROBES_MAX,
                None => false,
            }
    }

    fn validate(&self) -> Result<(), shrink_wrap::Error> {
        if self.is_valid() {
            Ok(())
        } else {
            Err(shrink_wrap::Error::SubtypeOutOfRange)
        }
    }
}

/// Who writes the blocks.
#[derive_shrink_wrap(owned(feature = "std"), derive(Debug, PartialEq, Clone))]
pub struct Producer<'i> {
    /// What it is, for people (`ww link tap`).
    pub name: &'i str,
}

/// A clock: what record times of the tracks on it count.
///
/// A device restart is a new clock (same counter, next epoch in its [key](Self::key)); how clocks relate is said only
/// by [ClockMap]s. Wall time is a clock too: origin [ClockOrigin::UnixEpoch], key [UNIX_CLOCK_KEY].
#[derive_shrink_wrap(owned(feature = "std"), derive(Debug, PartialEq, Clone))]
pub struct ClockDef<'i> {
    /// Number among the definitions, 1 and up: [TrackDef::clock] and [ClockMap] name it, and record blocks list it.
    /// [Definitions::define_clock](crate::Definitions::define_clock) gives it; it stays the same for the producer's
    /// life.
    pub number: ClockNum,
    /// Stable identity across files and runs, see [clock_key].
    pub key: ClockKey,
    /// Ticks per second, exact (`60000/1001`, `125 MHz`).
    pub rate: ClockRate,
    /// Seconds between two values the clock can really give, exact: `1/32768` for a 32 kHz counter read as
    /// nanoseconds. At least one tick.
    pub resolution: ClockResolution,
    pub origin: ClockOrigin,
    /// A name for people (`host`, `dev[3].boot[17]`).
    pub label: &'i str,
}

/// A clock as an application defines it: a [ClockDef] without its number, which
/// [Definitions::define_clock](crate::Definitions::define_clock) gives it.
#[derive(Debug, PartialEq, Clone)]
pub struct ClockSpec<'i> {
    /// Stable identity across files and runs, see [clock_key]: defining the same key again gives the same number.
    pub key: ClockKey,
    /// Ticks per second, exact.
    pub rate: ClockRate,
    /// Seconds between two values the clock can really give, exact; at least one tick.
    pub resolution: ClockResolution,
    pub origin: ClockOrigin,
    /// A name for people.
    pub label: &'i str,
}

impl<'i> ClockSpec<'i> {
    /// The definition of this clock as number `number`.
    pub fn numbered(self, number: ClockNum) -> ClockDef<'i> {
        ClockDef {
            number,
            key: self.key,
            rate: self.rate,
            resolution: self.resolution,
            origin: self.origin,
            label: self.label,
        }
    }
}

impl<'i> ClockDef<'i> {
    /// The clock without its number.
    pub fn spec(&self) -> ClockSpec<'i> {
        ClockSpec {
            key: self.key,
            rate: self.rate,
            resolution: self.resolution,
            origin: self.origin,
            label: self.label,
        }
    }
}

/// A clock's rate: ticks per second, exact. On the wire exactly its [Magnitude](exact_units::Magnitude).
pub type ClockRate = MagnitudeOf<Per<Tick, Second>>;

/// A clock's resolution: seconds between two values it can really give, exact. On the wire exactly its
/// [Magnitude](exact_units::Magnitude).
pub type ClockResolution = MagnitudeOf<Second>;

/// What tick 0 of a clock is, as an identity, never as a number.
#[derive_shrink_wrap(borrowed, owned(feature = "std"), derive(Debug, PartialEq, Eq, Clone, Copy), ww_repr = u4)]
pub enum ClockOrigin {
    /// A counter with no known start.
    Free,
    /// A device's boot; the boot count is the epoch in the clock's key. A restart is a new clock of this origin with
    /// no exact relation to the one before: only an estimated [ClockMap] relates them.
    Boot,
    /// The Unix epoch, 1970-01-01T00:00:00 UTC.
    UnixEpoch,
    /// The start of a media file or stream (its time base).
    MediaStart,
    /// The start of the producer's session.
    SessionStart,
    /// The next epoch of the same counter: a long capture at fine ticks is cut into clock epochs so ticks stay in
    /// i64. Its epoch link fixes tick 0: a [ClockMap] from this clock to the previous epoch's clock with one segment
    /// from tick 0 to the end, slope 1, error 0 and `b0 > 0`, so tick 0 here is tick `b0` there
    /// ([ClockMap::is_epoch_link]); a writer requires it ([Current::check_batch](crate::Current::check_batch)). Unlike a restart
    /// ([Self::Boot]), the two clocks are one counter.
    Epoch,
}

/// A map from clock [from](Self::from) to clock [to](Self::to): segments in ascending, non-overlapping order.
///
/// A map is strictly increasing over all its segments, or item order would flip between the clocks: each segment
/// starts above the last value the one before maps to. A tick in no segment is unmapped. A forward jump (NTP step,
/// drift correction) starts a new segment; a backward step is a gap or a new clock, never a decreasing segment. A
/// map written here is known when it is defined (a wall clock reading at the start); a map estimated
/// over time is a track whose items are [MapSegment]s.
#[derive_shrink_wrap(owned(feature = "std"), derive(Debug, PartialEq, Clone), validate = map_increasing)]
pub struct ClockMap<'i> {
    pub from: ClockNum,
    pub to: ClockNum,
    pub segments: RefVec<'i, MapSegment>,
}

/// `b = b0 + (a - a0) · slope` for `a` in `[a0, a1)` (`a1` None: to the end), within `± error` ticks of `to`.
///
/// The slope is a positive reduced fraction of `to` ticks per `from` tick ([Ratio64]), so a map is strictly
/// increasing. It is not a [Magnitude](exact_units::Magnitude), since a fitted ratio such as `1000000007/999999931` has primes a `Magnitude`
/// can't hold.
#[derive_shrink_wrap(
    borrowed,
    owned(feature = "std"),
    derive(Debug, PartialEq, Eq, Clone, Copy),
    validate = MapSegment::validate
)]
pub struct MapSegment {
    pub a0: i64,
    pub a1: Option<i64>,
    pub b0: i64,
    pub slope: Ratio64,
    /// Bound of the error, in ticks of `to`.
    pub error: u64,
}

impl MapSegment {
    /// A segment; None for an empty range.
    pub fn new(a0: i64, a1: Option<i64>, b0: i64, slope: Ratio64, error: u64) -> Option<Self> {
        if a1.is_some_and(|a1| a1 <= a0) {
            return None;
        }
        Some(Self {
            a0,
            a1,
            b0,
            slope,
            error,
        })
    }

    /// An offset: `b = b0 + (a - a0)`, slope 1, exact.
    pub const fn offset(a0: i64, b0: i64) -> Self {
        Self {
            a0,
            a1: None,
            b0,
            slope: Ratio64::ONE,
            error: 0,
        }
    }

    /// The last value the segment maps to, at `a1 - 1`, rounded down; None for a segment to the end or past i64.
    pub fn last(&self) -> Option<i64> {
        self.map(self.a1?.checked_sub(1)?, Rounding::Floor)
    }

    /// Whether `b = b0 + (a - a0)` exactly: slope 1, error 0.
    pub fn is_exact_offset(&self) -> bool {
        self.slope == Ratio64::ONE && self.error == 0
    }

    /// `a` mapped, rounded by `rounding`; None outside the segment or when `a - a0` or the result leaves i64.
    pub fn map(&self, a: i64, rounding: Rounding) -> Option<i64> {
        if a < self.a0 || self.a1.is_some_and(|a1| a >= a1) {
            return None;
        }
        let d = a.checked_sub(self.a0)?;
        self.b0.checked_add(self.slope.apply_rounded(d, rounding)?)
    }

    fn validate(&self) -> Result<(), shrink_wrap::Error> {
        match self.a1 {
            Some(a1) if a1 <= self.a0 => Err(shrink_wrap::Error::SubtypeOutOfRange),
            _ => Ok(()),
        }
    }
}

/// Whether segments in this order make a strictly increasing map: each starts at or after the end of the one before
/// (which has an end) and above the last value it maps to.
pub fn segments_increasing(segments: impl IntoIterator<Item = MapSegment>) -> bool {
    increasing(segments.into_iter().map(Some))
}

/// [segments_increasing] over segments read one by one, None for one that didn't read.
fn increasing(segments: impl Iterator<Item = Option<MapSegment>>) -> bool {
    let mut prev: Option<MapSegment> = None;
    for s in segments {
        let Some(s) = s else { return false };
        if let Some(p) = prev {
            let (Some(a1), Some(last)) = (p.a1, p.last()) else {
                return false;
            };
            if s.a0 < a1 || s.b0 <= last {
                return false;
            }
        }
        prev = Some(s);
    }
    true
}

/// The segments of a [ClockMap], borrowed or owned, for its check on decode.
trait MapSegments {
    fn segment_list(&self) -> impl Iterator<Item = Option<MapSegment>>;
}

impl MapSegments for ClockMap<'_> {
    fn segment_list(&self) -> impl Iterator<Item = Option<MapSegment>> {
        self.segments.iter().map(Result::ok)
    }
}

#[cfg(feature = "std")]
impl MapSegments for ClockMapOwned {
    fn segment_list(&self) -> impl Iterator<Item = Option<MapSegment>> {
        self.segments.iter().copied().map(Some)
    }
}

impl ClockMap<'_> {
    /// Whether the map is strictly increasing over all its segments ([segments_increasing]).
    pub fn is_increasing(&self) -> bool {
        increasing(self.segment_list())
    }

    /// Whether the map is an epoch link ([ClockOrigin::Epoch]): from the new epoch's clock to the previous one's,
    /// one segment from tick 0 to the end, slope 1, error 0, `b0 > 0`.
    pub fn is_epoch_link(&self) -> bool {
        let mut segments = self.segment_list();
        match (segments.next(), segments.next()) {
            (Some(Some(s)), None) => {
                s.is_exact_offset()
                    && s.a0 == 0
                    && s.a1.is_none()
                    && s.b0 > 0
                    && self.from != self.to
            }
            _ => false,
        }
    }
}

fn map_increasing(m: &impl MapSegments) -> Result<(), shrink_wrap::Error> {
    if increasing(m.segment_list()) {
        Ok(())
    } else {
        Err(shrink_wrap::Error::SubtypeOutOfRange)
    }
}

/// A type in an API bundle: the bundle's hash, the crate the type is defined in and its path in that crate
/// (`units::Volt`, `RunRecord`), as `ww_self` names a definition.
#[derive_shrink_wrap(owned(feature = "std"), derive(Debug, PartialEq, Eq, Hash, Clone))]
pub struct TypeRef<'i> {
    /// Hash of the bundle holding the definition, as in [Describe::Bundle].
    pub bundle: RefVec<'i, u8>,
    pub krate: FullVersion<'i>,
    /// Modules from the crate root and the type's name, `::` between them.
    pub path: &'i str,
}

/// A wire_weaver value of a type in a bundle.
#[derive_shrink_wrap(owned(feature = "std"), derive(Debug, PartialEq, Eq, Hash, Clone))]
pub struct TypedItem<'i> {
    pub ty: TypeRef<'i>,
    /// The value in its ShrinkWrap encoding.
    pub value: RefVec<'i, u8>,
}

/// One track.
///
/// Its identity is [key](Self::key) plus [dimensions](Self::dimensions), see [track_id].
#[derive_shrink_wrap(owned(feature = "std"), derive(Debug, PartialEq, Clone))]
pub struct TrackDef<'i> {
    /// Number in records, 1 and up (track 0 holds the definitions, [RecordTrack::Definitions](crate::RecordTrack)).
    /// [Definitions::define_track](crate::Definitions::define_track) gives it; it stays the same for the producer's
    /// life.
    pub number: TrackNum,
    /// Stable key: the same track across files and producer runs, see [track_key].
    pub key: TrackKey,
    /// The [ClockDef::number] record times of this track count.
    pub clock: ClockNum,
    /// Name and value pairs that are part of the identity (`input=cam1`, `producer=vad`), in ascending order of
    /// name, names unique. Two variants of one track coexist when their dimensions differ.
    pub dimensions: RefVec<'i, Dimension<'i>>,
    /// What one item is: a WireWeaver stream's value, a ShrinkWrap value, plain bytes, numbers or a format the user
    /// names.
    pub items: Items<'i>,
    /// A name for people (`link[1].rx.frames`); readers go by [items](Self::items).
    pub label: &'i str,
    /// Whether the track's records are events or state. Always [TrackRepeat::Stream] for a series: its chunks are
    /// data and carry their own grid ([SeriesGrid](crate::series::SeriesGrid)).
    pub repeat: TrackRepeat,
    /// How the track's items are stored: events or a fixed-rate series.
    pub kind: TrackKind<'i>,
    /// Whose track this is, opaque to `ww_tape` (a run, a capture): with the [id](Self::id), it tells apart tracks of
    /// one identity written into one file by several owners. Not part of the identity: [Self::id] and
    /// [key](Self::key) don't hash it. All zeros: the owner of the piece ([OwnerId](crate::OwnerId)).
    #[default = [0; 32]]
    pub owner: [u8; 32],
    /// What the track's items relate to, each by its [name](RelationDef::name), unique, at most [RELATIONS_MAX]:
    /// per-item values, a target, an index. `ww_tape` gives the building blocks; what a relation means is the
    /// writer's (eventfull's `source`, `parent`, `context`, an entity key).
    #[default = Default::default()]
    pub relations: RefVec<'i, RelationDef<'i>>,
    /// Whether every definition the track needs (its own, its clock chain, its bundles) is written twice at once: a
    /// record in a block of puts and again in the next block, so one damaged block loses none of them, also in a
    /// piece too small for the copies spaced apart.
    #[default = false]
    pub defined_twice: bool,
}

/// A track as an application defines it: a [TrackDef] without its number, which
/// [Definitions::define_track](crate::Definitions::define_track) gives it.
///
/// Its identity is [key](Self::key) plus [dimensions](Self::dimensions) and its [owner](Self::owner): defining the
/// same identity again gives the same number.
#[derive(Debug, PartialEq, Clone)]
pub struct TrackSpec<'i> {
    /// Stable key: the same track across files and producer runs, see [track_key](crate::track_key).
    pub key: TrackKey,
    /// The clock record times of this track count, as [Definitions::define_clock](crate::Definitions::define_clock)
    /// numbered it.
    pub clock: ClockNum,
    /// Name and value pairs that are part of the identity, in ascending order of name, names unique.
    pub dimensions: RefVec<'i, Dimension<'i>>,
    /// What one item is.
    pub items: Items<'i>,
    /// A name for people.
    pub label: &'i str,
    /// Whether the track's records are events or state.
    pub repeat: TrackRepeat,
    /// How the track's items are stored: events or a fixed-rate series.
    pub kind: TrackKind<'i>,
    /// Whose track this is, see [TrackDef::owner]; all zeros: the owner of the piece.
    pub owner: [u8; 32],
    /// What the track's items relate to, see [TrackDef::relations].
    pub relations: RefVec<'i, RelationDef<'i>>,
    /// Whether the definitions the track needs are written twice at once, see [TrackDef::defined_twice].
    pub defined_twice: bool,
}

impl<'i> TrackSpec<'i> {
    /// The definition of this track as number `number`.
    pub fn numbered(self, number: TrackNum) -> TrackDef<'i> {
        TrackDef {
            number,
            key: self.key,
            clock: self.clock,
            dimensions: self.dimensions,
            items: self.items,
            label: self.label,
            repeat: self.repeat,
            kind: self.kind,
            owner: self.owner,
            relations: self.relations,
            defined_twice: self.defined_twice,
        }
    }

    /// Id of the track's identity, see [track_id].
    pub fn id(&self) -> TrackId {
        track_id(self.key, self.dimensions.iter().filter_map(Result::ok))
    }
}

impl<'i> TrackDef<'i> {
    /// The track without its number.
    pub fn spec(&self) -> TrackSpec<'i> {
        TrackSpec {
            key: self.key,
            clock: self.clock,
            dimensions: self.dimensions.clone(),
            items: self.items.clone(),
            label: self.label,
            repeat: self.repeat,
            kind: self.kind.clone(),
            owner: self.owner,
            relations: self.relations.clone(),
            defined_twice: self.defined_twice,
        }
    }
}

/// What one item of a track is, see [TrackDef::items].
///
/// An events item's bytes are stored as given whatever the variant: the payload of a record or a chunk item, after
/// the duration and flags an [EventRecord](crate::EventRecord) puts before it. The variant only says how to read
/// them; `ww_tape` never reads a payload. A variant a reader doesn't know reads as `Unknown`, the rest of the
/// definition intact: a reader passes such items on as bytes, a relay holds such an events track and stores its items
/// as given ([Definitions::take_block](crate::Definitions::take_block)), and neither takes it for damage. A program
/// builds no `Unknown` of its own: [Definitions::define_track](crate::Definitions::define_track) refuses one. The variant is not
/// part of the track's identity: [TrackDef::key] is the producer's, see [track_key].
#[derive_shrink_wrap(owned(feature = "std"), derive(Debug, PartialEq, Clone), ww_repr = u4)]
pub enum Items<'i> {
    /// The values of a stream of a WireWeaver API, in their ShrinkWrap encoding.
    WireWeaver {
        /// The API the track's resource is in.
        api: FullVersion<'i>,
        /// Hash of that API's bundle, as in [Describe::Bundle]; empty when not known.
        bundle: RefVec<'i, u8>,
        /// Resource path of the track's stream in the API: `ww_trait` ids, array indices included.
        path: RefVec<'i, UNib32>,
        /// Keys of the elements of keyed arrays on `path`, in ascending order of position: what the track's
        /// [key](TrackDef::key) hashes instead of those indices. Empty when `path` crosses no keyed array.
        keys: RefVec<'i, KeySegment<'i>>,
        /// The item type when the path doesn't give it (a producer's result, a map track); None: the stream's type.
        item: Option<TypeRef<'i>>,
    },
    /// Values of a type in a bundle, in their ShrinkWrap encoding, with no API stream.
    ShrinkWrap(TypeRef<'i>),
    /// Plain bytes with no type: a frame as received, a file's contents.
    RawBytes,
    /// Numbers of one element each: a series' samples ([GridDef::channels](crate::GridDef::channels) of them),
    /// or an events track whose item is one element, little-endian in its container
    /// ([ElementDesc::container_bits]).
    Numbers {
        element: ElementDesc,
        /// The exact_units type of one value, its unit and scale (`Fixed<Ampere, Milli, i32>`); None: a plain number.
        unit: Option<TypeRef<'i>>,
    },
    /// Bytes of a format the user names.
    Foreign(Foreign<'i>),
}

impl<'i> Items<'i> {
    /// Numbers of `V`'s element ([SeriesValue::ELEMENT]), `unit` the exact_units type of one value.
    pub const fn numbers<V: SeriesValue>(unit: Option<TypeRef<'i>>) -> Self {
        Self::Numbers {
            element: V::ELEMENT,
            unit,
        }
    }
}

/// A format the user names for a track's items, see [Items::Foreign]. At least a name or a media type.
#[derive_shrink_wrap(owned(feature = "std"), derive(Debug, PartialEq, Eq, Clone, Hash))]
pub struct Foreign<'i> {
    /// The user's name for the format (`com.acme.frame`), at most [ENCODING_NAME_MAX] bytes of printable ASCII;
    /// empty when the media type says it all.
    pub name: &'i str,
    /// A media type (RFC 6838, parameters allowed): `application/vnd.tcpdump.pcap`, `application/cbor`,
    /// `application/x-protobuf; messageType=pkg.Msg`; empty for none. See [media_type_valid].
    pub media_type: &'i str,
    /// The format's version as the user writes it (`3`, `2.1`), at most [ENCODING_NAME_MAX] bytes of printable
    /// ASCII; empty for none.
    pub version: &'i str,
    /// The [hash](Schema::hash) of the [Schema] that describes the format, carried on track 0 with the track; empty
    /// for none.
    pub schema: RefVec<'i, u8>,
}

/// Longest media type of a [Foreign] format or a [Schema], in bytes: RFC 6838 allows 127 each for the type and the
/// subtype, and the parameters take the rest.
pub const MEDIA_TYPE_MAX: usize = 255;

/// Longest [name](Foreign::name) and [version](Foreign::version) of a foreign format, in bytes.
pub const ENCODING_NAME_MAX: usize = 255;

/// Bytes of a [Schema::hash]: a whole BLAKE3 hash.
pub const SCHEMA_HASH_LEN: usize = 32;

/// Whether `m` is a media type a writer takes: at most [MEDIA_TYPE_MAX] bytes of printable ASCII and spaces, a `/`
/// between a type and a subtype.
pub fn media_type_valid(m: &str) -> bool {
    m.len() <= MEDIA_TYPE_MAX
        && printable(m)
        && m.find('/')
            .is_some_and(|at| at > 0 && at < m.len().saturating_sub(1))
}

/// Whether `s` is printable ASCII and spaces only.
fn printable(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_graphic() || b == b' ')
}

impl Foreign<'_> {
    /// Whether a writer takes this format: a name or a media type, a name and a version of at most
    /// [ENCODING_NAME_MAX] bytes of printable ASCII, a media type empty or [valid](media_type_valid), a schema hash
    /// empty or [SCHEMA_HASH_LEN] bytes.
    pub fn is_valid(&self) -> bool {
        (!self.name.is_empty() || !self.media_type.is_empty())
            && self.name.len() <= ENCODING_NAME_MAX
            && printable(self.name)
            && self.version.len() <= ENCODING_NAME_MAX
            && printable(self.version)
            && (self.media_type.is_empty() || media_type_valid(self.media_type))
            && (self.schema.is_empty() || self.schema.len() == SCHEMA_HASH_LEN)
    }
}

/// A schema of a foreign format ([Foreign::schema]): any blob that describes a format, by its hash. A protobuf
/// descriptor set, a JSON schema, a Kaitai Struct file.
///
/// One definition per hash, carried on track 0 before the tracks that name it, as a [Describe::Bundle] is. At most
/// [STATE_ITEM_MAX](crate::STATE_ITEM_MAX) bytes whole, as any definition.
#[derive_shrink_wrap(owned(feature = "std"), derive(Debug, PartialEq, Eq, Clone))]
pub struct Schema<'i> {
    /// BLAKE3 of [bytes](Self::bytes), [SCHEMA_HASH_LEN] bytes.
    pub hash: RefVec<'i, u8>,
    /// What the bytes are, a [valid](media_type_valid) media type (`application/schema+json`).
    pub media_type: &'i str,
    pub bytes: RefVec<'i, u8>,
}

impl Schema<'_> {
    /// Whether a writer takes the schema: its hash is BLAKE3 of its bytes and its media type is valid.
    pub fn is_valid(&self) -> bool {
        self.hash.as_slice() == blake3::hash(self.bytes.as_slice()).as_bytes()
            && media_type_valid(self.media_type)
    }
}

/// Most relations one track declares.
pub const RELATIONS_MAX: usize = 16;

/// Longest [name](RelationDef::name) of a relation, in bytes.
pub const RELATION_NAME_MAX: usize = 64;

/// A relation of a track's items, see [TrackDef::relations].
#[derive_shrink_wrap(owned(feature = "std"), derive(Debug, PartialEq, Eq, Clone))]
pub struct RelationDef<'i> {
    /// The relation's name, not empty, at most [RELATION_NAME_MAX] bytes; the writer gives it meaning.
    pub name: &'i str,
    /// The track the values point into, by run and identity; None when the values are not references (keys).
    pub target: Option<RelationTarget>,
    /// Whether the items carry a value, and where it is kept.
    pub values: RelationValues,
    /// Whether the values never decrease along the track's item order (source ordinals 10, 10, 11, 15; not 10, 12,
    /// 11). A writer refuses a value below the item's before it.
    pub monotone: bool,
    /// The index the values feed.
    pub index: RelationIndex,
}

/// What a [RelationDef] points into: a track of a run, both named explicitly; never bound by position or default.
#[derive_shrink_wrap(
    borrowed,
    owned(feature = "std"),
    derive(Debug, PartialEq, Eq, Clone, Copy, Hash)
)]
pub struct RelationTarget {
    /// The run, opaque to `ww_tape` (eventfull's 16-byte run id), the same for a target in the run itself.
    pub run: [u8; 16],
    /// The target track's [id](TrackDef::id).
    pub track: TrackId,
}

/// Where a relation's per-item values are kept.
#[derive_shrink_wrap(borrowed, owned(feature = "std"), derive(Debug, PartialEq, Eq, Clone, Copy, Hash), ww_repr = u4)]
pub enum RelationValues {
    /// No values: the track is related as a whole (read together with the target).
    None,
    /// A column of the track's chunks ([ColumnKind::Relation](crate::ColumnKind::Relation)).
    Stored,
    /// Reserved: values given with each item only to feed the index, no column. Refused by writers until built.
    IndexOnly,
}

/// The index a relation's values feed.
#[derive_shrink_wrap(borrowed, owned(feature = "std"), derive(Debug, PartialEq, Eq, Clone, Copy, Hash), ww_repr = u4)]
pub enum RelationIndex {
    None,
    /// The values' smallest and largest per chunk in its index entry, for a monotone relation: a search of the
    /// chunks by value. One relation per track.
    Range,
    /// Reserved: per chunk, each distinct value with its items (block kind 11). Refused by writers until built.
    Postings,
    /// Reserved: per chunk, the distinct values and each item's index into them (block kind 12). Refused by writers
    /// until built.
    Dictionary,
}

/// What a track's records are to a reader that starts later, see [state](crate::state).
#[derive_shrink_wrap(borrowed, owned(feature = "std"), derive(Debug, PartialEq, Eq, Clone, Copy), ww_repr = u4)]
pub enum TrackRepeat {
    /// A stream of events: each record is an item, nothing of the track is repeated.
    Stream,
    /// Each record is the track's state from then on: a writer puts it ([TapeWriter::put](crate::TapeWriter::put))
    /// so that a copy lies at most `near` behind wherever it is used, and a reader takes the nearest record behind a
    /// place.
    State(StatePolicy),
    /// Each record is the state of one key from then on, many keys on one track: a writer puts a key's value
    /// ([TapeWriter::put_key](crate::TapeWriter::put_key)) or its deletion
    /// ([TapeWriter::delete_key](crate::TapeWriter::delete_key)) as [StatePolicy] says, per key, and a reader takes
    /// the newest record of each key behind a place ([state](crate::state)).
    Keyed(StatePolicy),
}

/// How a writer repeats the items of a state track, or of track 0 ([Layout::definitions]).
#[derive_shrink_wrap(
    borrowed,
    owned(feature = "std"),
    derive(Debug, PartialEq, Eq, Clone, Copy)
)]
pub struct StatePolicy {
    /// An item used at a place has a copy at most this many bytes behind it, in the same piece: how far a reader
    /// looks back for it.
    pub near: Bytes<u32>,
    /// Copies an item gets after it is first written in a piece, spaced apart (a writer's
    /// [NearPolicy::copy_spacing](crate::writer::NearPolicy)), against damage around one place; 0 for none.
    pub copies: Count<Item, u8>,
}

impl StatePolicy {
    /// A copy at most [NEAR_DEFAULT] behind, one more copy at the first write.
    pub const DEFAULT: Self = Self {
        near: Bytes::new(NEAR_DEFAULT),
        copies: Count::new(1),
    };
}

/// [StatePolicy::near] of [StatePolicy::DEFAULT]: 32 MiB.
pub const NEAR_DEFAULT: u32 = 32 << 20;

impl TrackRepeat {
    /// A state track repeated as [StatePolicy::DEFAULT].
    pub const STATE: Self = Self::State(StatePolicy::DEFAULT);

    /// A keyed state track repeated as [StatePolicy::DEFAULT].
    pub const KEYED: Self = Self::Keyed(StatePolicy::DEFAULT);

    /// The state policy, None for a stream.
    pub const fn state(self) -> Option<StatePolicy> {
        match self {
            Self::State(p) | Self::Keyed(p) => Some(p),
            Self::Stream | Self::Unknown(_) => None,
        }
    }

    /// Whether the track is keyed ([Self::Keyed]).
    pub const fn is_keyed(self) -> bool {
        matches!(self, Self::Keyed(_))
    }
}

/// The key of the keyed-array element an [Items::WireWeaver] path index points at.
#[derive_shrink_wrap(owned(feature = "std"), derive(Debug, PartialEq, Eq, Clone))]
pub struct KeySegment<'i> {
    /// Position in the path of the index this key stands for.
    pub at: UNib32,
    /// The key in its shrink_wrap encoding.
    pub bytes: RefVec<'i, u8>,
}

/// How a track's items are stored.
#[derive_shrink_wrap(owned(feature = "std"), derive(Debug, PartialEq, Clone), ww_repr = u4)]
pub enum TrackKind<'i> {
    /// Items with a start tick, an optional end, an optional flags word and a payload: in record blocks or in events
    /// chunks ([BlockKind::Events](crate::BlockKind::Events)).
    Events(EventsDef),
    /// Samples at an exact period, no time per sample: in series chunks ([BlockKind::Series](crate::BlockKind::Series)),
    /// each timed and scaled by the [SeriesGrid](crate::series::SeriesGrid) its head carries.
    Series(GridDef<'i>),
}

/// An events track: what its chunks may hold besides starts and payloads.
#[derive_shrink_wrap(
    borrowed,
    owned(feature = "std"),
    derive(Debug, PartialEq, Eq, Clone, Copy, Hash)
)]
pub struct EventsDef {
    pub ends: Ends,
    /// Items carry a `u32` flags word (severity in its low bits), which summaries OR together.
    pub flags: bool,
}

impl EventsDef {
    /// Instants without flags: what a track of plain records is.
    pub const INSTANTS: Self = Self {
        ends: Ends::Instants,
        flags: false,
    };
}

/// Whether events have an end.
#[derive_shrink_wrap(borrowed, owned(feature = "std"), derive(Debug, PartialEq, Eq, Clone, Copy, Hash), ww_repr = u4)]
pub enum Ends {
    /// Every item is an instant: chunks have no ends ([EndMode::Absent](crate::EndMode::Absent)).
    Instants,
    /// Items are spans `[start, end)`; an instant among them has `end == start`.
    Spans,
}

impl Ends {
    /// Whether an events chunk of this track may store ends as `mode` says.
    pub fn allows(self, mode: crate::EndMode) -> bool {
        match self {
            Self::Instants => mode == crate::EndMode::Absent,
            Self::Spans => true,
            Self::Unknown(_) => false,
        }
    }
}

/// A name and value of a track's identity.
#[derive_shrink_wrap(owned(feature = "std"), derive(Debug, PartialEq, Eq, Clone))]
pub struct Dimension<'i> {
    pub name: &'i str,
    pub value: &'i str,
}

/// Key of the wall clock (origin Unix epoch), the same for every producer.
pub const UNIX_CLOCK_KEY: ClockKey = ClockKey::new(fnv1a(FNV_OFFSET, b"ww_tape unix epoch"));

/// Stable key of a clock: the [track_key](crate::track_key) of the name path of the thing that counts, then FNV-1a of
/// `epoch` (u64 LE, a boot count).
pub const fn clock_key(api_root: u64, segments: &[crate::Segment<'_>], epoch: u64) -> ClockKey {
    ClockKey::new(fnv1a(
        crate::track_key(api_root, segments).get(),
        &epoch.to_le_bytes(),
    ))
}

/// Id of a track's identity: its key without dimensions, else FNV-1a of the key (u64 LE) and of every dimension's
/// name and value, each followed by a 0 byte.
pub fn track_id<'a>(key: TrackKey, dimensions: impl IntoIterator<Item = Dimension<'a>>) -> TrackId {
    let mut h = None;
    for d in dimensions {
        let mut x = h.unwrap_or_else(|| fnv1a(FNV_OFFSET, &key.get().to_le_bytes()));
        x = fnv1a(fnv1a(x, d.name.as_bytes()), &[0]);
        x = fnv1a(fnv1a(x, d.value.as_bytes()), &[0]);
        h = Some(x);
    }
    h.map_or(key.id(), TrackId::new)
}

impl<'i> TrackDef<'i> {
    /// The series definition of a series track, None for an events track.
    pub fn series(&self) -> Option<&GridDef<'i>> {
        match &self.kind {
            TrackKind::Series(s) => Some(s),
            _ => None,
        }
    }

    /// The shape of a series track's samples: its items' element by its definition's channels; None for an events
    /// track or items that are no numbers.
    pub fn series_shape(&self) -> Option<SampleShape> {
        match (&self.items, self.series()) {
            (Items::Numbers { element, .. }, Some(s)) => Some(s.shape(*element)),
            _ => None,
        }
    }

    /// Check what the kind asks of the track: a series has the plain policy (its chunks are data and carry their
    /// grids), its items are numbers of an element the raw codec codes, its summaries fit the element (at most 2
    /// parts, power only of a pair) and its [GridDef] is valid.
    pub fn check_kind(&self) -> Result<(), crate::SeriesError> {
        use crate::SeriesError::Def;
        let Some(s) = self.series() else {
            return Ok(());
        };
        if self.repeat.state().is_some() {
            return Err(Def(
                "a series track takes the plain policy: its chunks are data",
            ));
        }
        let Items::Numbers { element, .. } = &self.items else {
            return Err(Def("a series' items are numbers"));
        };
        if !element.is_valid() {
            return Err(Def("an element no writer takes"));
        }
        if !element.is_coded() {
            return Err(Def("an element the raw codec doesn't code"));
        }
        if let Some(sum) = s.summaries {
            if element.parts.0 > 2 {
                return Err(Def("summaries of an element of more than 2 parts"));
            }
            if (sum.stats.power_mean || sum.stats.power_max) && element.parts.0 != 2 {
                return Err(Def("power of a real element"));
            }
        }
        s.check()
    }

    /// Whether [items](Self::items) is of a kind this crate writes and valid: a [Foreign] format
    /// [valid](Foreign::is_valid), numbers of a [valid](ElementDesc::is_valid) element, no unknown variant.
    pub fn items_valid(&self) -> bool {
        match &self.items {
            Items::WireWeaver { .. } | Items::ShrinkWrap(_) | Items::RawBytes => true,
            Items::Numbers { element, .. } => element.is_valid(),
            Items::Foreign(f) => f.is_valid(),
            Items::Unknown(_) => false,
        }
    }

    /// Id of the track's identity, see [track_id].
    pub fn id(&self) -> TrackId {
        track_id(self.key, self.dimensions.iter().filter_map(Result::ok))
    }

    /// Whether the relations are well formed by themselves: at most [RELATIONS_MAX], names not empty, at most
    /// [RELATION_NAME_MAX] bytes and unique,
    /// values only on an events track, monotone only with values, an index only with values, index-only values only
    /// with an index, a range index only on a monotone relation and on one relation at most.
    pub fn relations_canonical(&self) -> bool {
        let mut ranges = 0usize;
        for (n, r) in self.relations.iter().enumerate() {
            let Ok(r) = r else { return false };
            let values = r.values != RelationValues::None;
            let repeated = self
                .relations
                .iter()
                .take(n)
                .any(|o| o.is_ok_and(|o| o.name == r.name));
            if n >= RELATIONS_MAX
                || r.name.is_empty()
                || r.name.len() > RELATION_NAME_MAX
                || repeated
                || (values && self.series().is_some())
                || (r.monotone && !values)
                || (r.index != RelationIndex::None && !values)
                || (r.values == RelationValues::IndexOnly && r.index == RelationIndex::None)
                || (r.index == RelationIndex::Range && !r.monotone)
            {
                return false;
            }
            if r.index == RelationIndex::Range {
                ranges = ranges.saturating_add(1);
            }
        }
        ranges <= 1
    }

    /// Whether every relation is of a kind this crate writes: no index-only values, no postings or dictionary
    /// index (reserved, not built).
    pub fn relations_built(&self) -> bool {
        self.relations.iter().all(|r| {
            r.is_ok_and(|r| {
                r.values != RelationValues::IndexOnly
                    && matches!(r.index, RelationIndex::None | RelationIndex::Range)
            })
        })
    }

    /// Whether the dimensions are in ascending order of name, names unique.
    pub fn dimensions_canonical(&self) -> bool {
        let mut prev: Option<&str> = None;
        for d in self.dimensions.iter() {
            let Ok(d) = d else { return false };
            if prev.is_some_and(|p| p >= d.name) {
                return false;
            }
            prev = Some(d.name);
        }
        true
    }
}

/// The API [Describe] and the types it names are in: this crate.
///
/// A producer that wants its definitions read without this crate at hand adds the bundle of this API (`ww_tape` in
/// `wire_weaver_snapshots`) as a [Describe::Bundle].
pub fn api() -> FullVersion<'static> {
    // Cargo gives digits only, so a parse never falls back to 0
    let n = |s: &str| s.parse().unwrap_or(0);
    FullVersion::new(
        env!("CARGO_PKG_NAME"),
        Version::new(
            n(env!("CARGO_PKG_VERSION_MAJOR")),
            n(env!("CARGO_PKG_VERSION_MINOR")),
            n(env!("CARGO_PKG_VERSION_PATCH")),
        ),
    )
}
