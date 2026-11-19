//! Summary blocks: the stored levels of a track's summary tree, read in place.
//!
//! A track's items (an event track's items, a series' samples) are grouped into **nodes** over item numbers, not over
//! time: a node at level 0 holds `2^shift` items, a node one level up the nodes of 16 nodes below. Node `n` of a level
//! covers items `[n << shift, (n + 1) << shift)` of the track, numbered from the track's first item, so nodes of two
//! tape pieces of one track line up, and a piece's first and last node may hold fewer items (a *partial* node).
//!
//! - **Event item tree** ([Tree::Events]): nodes of 2048 items at level 0. A node holds the first item's start, the
//!   last item's start, the OR of the items' flags and, for spans, the latest end of any item. Counts are implicit.
//!   An overview of a track reads only the level whose nodes are about as many as the pixels, never the items.
//! - **Series summaries** ([Tree::Series]): nodes of `2^base_shift` samples at level 0 ([SeriesSummarySpec]). A node
//!   holds per channel the statistics the track declares ([SeriesStats]); no ticks, since a series' sample ticks
//!   follow from the grids its chunks carry and its period.
//!
//! Every statistic merges: a parent is its 16 children merged ([EventNode::merge], [ChannelStats::merge]), in item
//! order, so a reader building coarser bins (a view cache) gets what the writer would have written.
//!
//! # Scales
//!
//! A series chunk may change the scale of the stored numbers (an autoranging device changes prefix), and nodes run
//! over sample indexes, so a node may hold samples of two scales. Such a node is kept at their common scale
//! ([common_scale]: the largest scale both are whole multiples of, `10⁻⁶` for `10⁻³` and `10⁻⁶`): every number of the
//! coarser samples is multiplied by a whole factor, so minimum and maximum stay exact integers and sums don't mix
//! scales ([ChannelStats::rescaled], [SeriesNode::merge]). A summary block states its scale: the common scale of
//! all its nodes, which it stores at that scale, so a block is read alone, without the chunks. Merging nodes
//! of two blocks (a view cache across a change of scale) goes through [SeriesNode::merge], which brings both to their
//! common scale first; two scales with no common scale (a ratio that isn't a fraction of integers) are refused, never
//! mixed.
//!
//! A summary block ([BlockKind::Summary]) holds a batch of consecutive nodes of one level of one track. The block
//! header's count (bytes 12..16) is the number of nodes; bytes 28..32 stay reserved, zero:
//!
//! ```text
//! off size field
//!  32 u64  track id (TrackDef::id)
//!  40 u32  track number (TrackDef::number)
//!  44  u8  tree: 1 events, 2 series (Tree)
//!  45  u8  level, 0 and up
//!  46  u8  node shift: a node holds 2^shift items (events: 11 + 4 · level; series: base_shift + 4 · level)
//!  47  u8  element of a series' samples (Element code), 0 for events
//!  48 u16  fields: the statistics stored (EventFields, SeriesStats bits)
//!  50 u16  channels of a series, 1 and up; 0 for events
//!  52 u16  scale length: bytes of the scale after the head, 1 to SCALE_LEN_MAX for a series, 0 for events
//!  54 u16  first channel or leaf of this block: reserved, 0 (the block's channels start at channel 0); a reader
//!          refuses another value
//!  56 u64  first item: the first item the block covers (the first node's first item, or later in a partial node)
//!  64 u64  items the block covers, 1 and up; the nodes are those holding items [first, first + items)
//!  72 i64  first tick: the first covered item's start
//!  80 i64  last tick: the last covered item's start, at least the first tick
//!  88 i64  latest end: the largest end of a covered item, at least the last tick
//!  96 ...  scale of a series: what a stored minimum, maximum or sum is worth in the sample type's unit, as the
//!          shrink_wrap bytes of an exact_units Magnitude (10⁻³ is fa 00); squares and powers are at its square
//! 96+s ... body: columns of n values each, n the node count, little-endian, back to back
//! len-4 u32 CRC over bytes 0..len-4, as in every block
//! ```
//!
//! Event body: first ticks (i64), last ticks (i64), then flags ORs (u32) when [EventFields::flags], then latest ends
//! (i64) when [EventFields::latest_end].
//!
//! Series body: for each statistic the block holds, in the order of [Stat::ALL], for each channel, for each component
//! of a sample (2 for a complex element's minimum, maximum, sum and sum of squares, else 1), one column. Value widths:
//! minimum and maximum 8 bytes (i64, u64 or f64, by the element's kind: [Value]), sum and sum of squares f64, clipped
//! count u64, flags u32, mean and maximum power f32.
//!
//! Summary blocks are small (a few KiB) and rebuilt from the data when lost. In an aligned file the writer places them
//! in the gap before the next aligned chunk payload ([TapeWriter::defer_block](crate::TapeWriter::defer_block)).

use exact_units::Magnitude;
use shrink_wrap::prelude::*;

use crate::bytes::{
    self, file_u8, file_u16, file_u32, file_u64, le_i64, le_u8, le_u16, le_u32, le_u64,
};
use crate::{BlockKind, BoundError, CRC_LEN, HEADER_LEN, MAX_BLOCK_LEN};

#[cfg(feature = "std")]
use ww_tape_codecs::kernel;
#[cfg(all(test, feature = "std"))]
mod kernel_tests;
mod series;
use crate::Element;
#[cfg(feature = "alloc")]
pub use series::SeriesNode;
pub use series::{ChannelStats, FACTOR_MAX, ScaleError, Value, common_scale, scale_factor};
#[cfg(feature = "std")]
pub use series::{SERIES_BLOCK_BYTES, Scalar, SeriesStart, SeriesSummaries};
mod spec;
pub use spec::{ClipRange, SeriesStats, SeriesSummarySpec, Stat, components, is_complex};
mod nodes;
#[cfg(feature = "std")]
use nodes::batch_nodes;
pub use nodes::{EventNode, StatColumn, Summary, write_event_summary, write_series_summary};

#[cfg(feature = "std")]
mod levels;
#[cfg(feature = "std")]
pub use levels::BuildError;

#[cfg(feature = "std")]
mod tree;
#[cfg(feature = "std")]
pub use tree::{EVENT_BLOCK_BYTES, EventTree, ItemRun};

/// Bytes of the summary head after the block header (32..96).
pub const SUMMARY_FIXED_LEN: usize = 64;
/// Where the scale starts: after the block header and the summary head.
const SCALE_AT: usize = HEADER_LEN + SUMMARY_FIXED_LEN;
/// Bytes of a summary block without scale and body.
pub const SUMMARY_MIN_LEN: usize = SCALE_AT + CRC_LEN;
/// Node shift of the event tree at level 0: 2048 items.
pub const EVENT_BASE_SHIFT: u8 = 11;
/// Log2 of the nodes of one level merged into one node of the level above: 16.
pub const FANOUT_SHIFT: u8 = 4;
/// Largest node shift: a node holds at most 2^62 items.
pub const NODE_SHIFT_MAX: u8 = 62;
pub use crate::series::SCALE_LEN_MAX;
/// Highest level of a tree: a node of the level above would hold more than `2^NODE_SHIFT_MAX` items, so neither tree
/// grows past it.
pub const LEVEL_MAX: u8 = NODE_SHIFT_MAX / FANOUT_SHIFT;

/// Node shift of `level` in a tree whose level-0 nodes hold `2^base` items; None past a u8.
pub fn level_shift(base: u8, level: u8) -> Option<u8> {
    FANOUT_SHIFT.checked_mul(level)?.checked_add(base)
}

/// `v >> shift`, 0 for a shift past 63: the quotient by `2^shift`.
fn shr(v: u64, shift: u8) -> u64 {
    v.checked_shr(u32::from(shift)).unwrap_or(0)
}

/// The items of node `n` (nodes of `2^shift` items, numbered from item 0) among items `[from, from + count)`: its
/// first item and how many; None when it holds none of them.
pub fn node_span(n: u64, shift: u8, from: u64, count: u64) -> Option<(u64, u64)> {
    let last = from.checked_add(count.checked_sub(1)?)?;
    let (first_node, last_node) = (shr(from, shift), shr(last, shift));
    if n < first_node || n > last_node {
        return None;
    }
    // n is past the first node, so shift < 64; n · 2^shift <= last, no bit lost
    let start = if n == first_node {
        from
    } else {
        n.checked_shl(u32::from(shift))?
    };
    let end = if n == last_node {
        last
    } else {
        // the node's last item: its start with every bit below the shift set
        start | !u64::MAX.checked_shl(u32::from(shift))?
    };
    Some((start, end.checked_sub(start)?.checked_add(1)?))
}

/// Which kind of tree a summary block belongs to.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Tree {
    /// The item tree of an event track.
    Events,
    /// The summaries of a fixed-rate series.
    Series,
}

impl Tree {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Tree::Events),
            2 => Some(Tree::Series),
            _ => None,
        }
    }

    pub(crate) fn to_u8(self) -> u8 {
        match self {
            Tree::Events => 1,
            Tree::Series => 2,
        }
    }
}

/// What an event tree stores besides the first and last tick of each node.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct EventFields {
    /// The OR of the items' flags words: the track has a flags column.
    pub flags: bool,
    /// The latest end of any item: the track holds spans.
    pub latest_end: bool,
}

impl EventFields {
    const FLAGS: u16 = 1;
    const LATEST_END: u16 = 2;

    fn bits(self) -> u16 {
        (if self.flags { Self::FLAGS } else { 0 })
            | (if self.latest_end { Self::LATEST_END } else { 0 })
    }

    fn from_bits(v: u16) -> Option<Self> {
        if v & !(Self::FLAGS | Self::LATEST_END) != 0 {
            return None;
        }
        Some(EventFields {
            flags: v & Self::FLAGS != 0,
            latest_end: v & Self::LATEST_END != 0,
        })
    }

    /// Bytes of one node: first and last tick, then the flags OR and the latest end when stored.
    fn node_len(self) -> usize {
        match (self.flags, self.latest_end) {
            (false, false) => 16,
            (true, false) => 20,
            (false, true) => 24,
            (true, true) => 28,
        }
    }
}

/// What a summary block covers: the fixed part of its head.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct SummaryHead {
    /// [TrackDef::id](crate::TrackDef::id).
    pub track_id: u64,
    /// [TrackDef::number](crate::TrackDef::number).
    pub track_number: u32,
    pub tree: Tree,
    pub level: u8,
    /// A node holds `2^node_shift` items.
    pub node_shift: u8,
    /// What the block stores per node.
    pub layout: Layout,
    /// The first item the block covers.
    pub first_item: u64,
    /// Items the block covers, 1 and up.
    pub items: u64,
    /// The first covered item's start.
    pub first_tick: i64,
    /// The last covered item's start.
    pub last_tick: i64,
    /// The largest end of a covered item; the last tick for instants and series.
    pub latest_end: i64,
}

/// What a summary block stores per node, by its tree.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[expect(
    clippy::large_enum_variant,
    reason = "a head is read once per block; a boxed scale would cost Copy"
)]
pub enum Layout {
    Events(EventFields),
    Series {
        element: Element,
        channels: u16,
        stats: SeriesStats,
        /// What a stored minimum, maximum or sum of every node in the block is worth in the sample type's unit.
        scale: Magnitude,
    },
}

impl SummaryHead {
    /// Number of the first node: the node holding [Self::first_item].
    pub fn first_node(&self) -> u64 {
        shr(self.first_item, self.node_shift)
    }

    /// Nodes in the block, 1 and up; 0 for a head the format refuses (no items, or items past u64).
    pub fn node_count(&self) -> u64 {
        self.items
            .checked_sub(1)
            .and_then(|k| self.first_item.checked_add(k))
            .and_then(|last| shr(last, self.node_shift).checked_sub(self.first_node()))
            .and_then(|k| k.checked_add(1))
            .unwrap_or(0)
    }

    /// The items of node `i` of the block (0 is [Self::first_node]): first item and count, fewer than `2^shift` in a
    /// partial node at either end; None when `i` is not below [Self::node_count].
    pub fn node_items(&self, i: u64) -> Option<(u64, u64)> {
        let n = self.first_node().checked_add(i)?;
        node_span(n, self.node_shift, self.first_item, self.items)
    }

    /// Bytes of one node in the body; None past a usize.
    fn node_len(&self) -> Option<usize> {
        match self.layout {
            Layout::Events(f) => Some(f.node_len()),
            Layout::Series {
                element,
                channels,
                stats,
                ..
            } => stats.node_len(element).checked_mul(usize::from(channels)),
        }
    }

    /// Read and check the summary block `block` (exactly its bytes, CRC checked by the caller): its head, and its
    /// body, as long as the head says. A reader that decodes no node still checks a block this way.
    pub fn read(block: &[u8]) -> Result<(Self, &[u8]), SummaryError> {
        let short = SummaryError::Invalid("shorter than the head");
        let (head_scale_body, _crc) = block
            .split_last_chunk::<CRC_LEN>()
            .filter(|(b, _)| b.len() >= SCALE_AT)
            .ok_or(short)?;
        let b = head_scale_body;
        if le_u32(b, 28).ok_or(short)? != 0 || le_u16(b, 54).ok_or(short)? != 0 {
            return Err(SummaryError::Invalid("reserved bytes not zero"));
        }
        let tree = Tree::from_u8(le_u8(b, 44).ok_or(short)?)
            .ok_or(SummaryError::Invalid("unknown tree"))?;
        let element = le_u8(b, 47).ok_or(short)?;
        let fields = le_u16(b, 48).ok_or(short)?;
        let scale_len = file_u16(b, 52).ok_or(short)?.at_most(SCALE_LEN_MAX)?;
        let (scale_bytes, body) = b
            .get(SCALE_AT..)
            .and_then(|rest| rest.split_at_checked(scale_len))
            .ok_or(SummaryError::Invalid("scale past the end"))?;
        // a channel takes at least 4 bytes of every node
        let channels_max = u16::try_from(body.len()).unwrap_or(u16::MAX);
        let channels = file_u16(b, 50).ok_or(short)?.bounded(channels_max)?;
        let layout = match tree {
            Tree::Events => {
                if element != 0 || channels != 0 || scale_len != 0 {
                    return Err(SummaryError::Invalid(
                        "element, channels or scale in an event tree",
                    ));
                }
                Layout::Events(
                    EventFields::from_bits(fields)
                        .ok_or(SummaryError::Invalid("unknown fields"))?,
                )
            }
            Tree::Series => Layout::Series {
                element: Element::from_code(element)
                    .ok_or(SummaryError::Invalid("unknown element"))?,
                channels,
                stats: SeriesStats::from_bits(fields)
                    .ok_or(SummaryError::Invalid("unknown statistics"))?,
                scale: read_scale(scale_bytes)?,
            },
        };
        let first_item = le_u64(b, 56).ok_or(short)?;
        let head = SummaryHead {
            track_id: le_u64(b, 32).ok_or(short)?,
            track_number: le_u32(b, 40).ok_or(short)?,
            tree,
            level: file_u8(b, 45).ok_or(short)?.bounded(LEVEL_MAX)?,
            node_shift: file_u8(b, 46).ok_or(short)?.bounded(NODE_SHIFT_MAX)?,
            layout,
            first_item,
            // items past u64::MAX - first_item would number past u64
            items: file_u64(b, 64).ok_or(short)?.bounded(!first_item)?,
            first_tick: le_i64(b, 72).ok_or(short)?,
            last_tick: le_i64(b, 80).ok_or(short)?,
            latest_end: le_i64(b, 88).ok_or(short)?,
        };
        head.check()?;
        // a node takes at least 4 bytes of the body
        let nodes = file_u32(block, 12).ok_or(short)?.within(body.len())?;
        if u64::try_from(nodes).ok() != Some(head.node_count()) {
            return Err(SummaryError::Invalid(
                "node count differs from the head's items",
            ));
        }
        if head
            .node_len()
            .and_then(|len| nodes.checked_mul(len))
            .is_none_or(|len| len != body.len())
        {
            return Err(SummaryError::Invalid("body length differs from the head"));
        }
        Ok((head, body))
    }

    /// Check what a head can say on its own.
    fn check(&self) -> Result<(), SummaryError> {
        if self.items == 0 {
            return Err(SummaryError::Invalid("no items"));
        }
        if self.first_item.checked_add(self.items).is_none() {
            return Err(SummaryError::Invalid("item numbers past u64"));
        }
        if self.node_shift > NODE_SHIFT_MAX {
            return Err(SummaryError::Invalid("node shift past 62"));
        }
        if self.last_tick < self.first_tick || self.latest_end < self.last_tick {
            return Err(SummaryError::Invalid(
                "ticks out of order: first, last, latest end",
            ));
        }
        match (self.tree, self.layout) {
            (Tree::Events, Layout::Events(f)) => {
                if level_shift(EVENT_BASE_SHIFT, self.level) != Some(self.node_shift) {
                    return Err(SummaryError::Invalid("event node shift not 11 + 4 · level"));
                }
                if !f.latest_end && self.latest_end != self.last_tick {
                    return Err(SummaryError::Invalid(
                        "latest end of instants not the last tick",
                    ));
                }
            }
            (
                Tree::Series,
                Layout::Series {
                    element,
                    channels,
                    stats,
                    ..
                },
            ) => {
                if FANOUT_SHIFT
                    .checked_mul(self.level)
                    .is_none_or(|min| self.node_shift < min)
                {
                    return Err(SummaryError::Invalid("series node shift below 4 · level"));
                }
                if channels == 0 {
                    return Err(SummaryError::Invalid("a series has no channels"));
                }
                if stats.is_empty() {
                    return Err(SummaryError::Invalid("no statistics"));
                }
                if (stats.power_mean || stats.power_max) && !is_complex(element) {
                    return Err(SummaryError::Invalid("power of a real element"));
                }
                if self.latest_end != self.last_tick {
                    return Err(SummaryError::Invalid(
                        "latest end of a series not the last tick",
                    ));
                }
            }
            _ => return Err(SummaryError::Invalid("layout of another tree")),
        }
        Ok(())
    }
}

/// Why a summary block can't be written or read.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SummaryError {
    /// The buffer is shorter than the block ([summary_len]).
    Full { needed: usize },
    /// The block would be longer than [MAX_BLOCK_LEN].
    TooLong { len: usize },
    /// The head breaks a rule of the format, or the body is not as long as the head says.
    Invalid(&'static str),
    /// A series block's scale doesn't encode or decode.
    ScaleBytes(shrink_wrap::Error),
    /// A count, length or layout value of the block is over its cap or the block's length.
    Bound(BoundError),
}

impl core::fmt::Display for SummaryError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SummaryError::Full { needed } => {
                write!(f, "the buffer is short: {needed} bytes needed")
            }
            SummaryError::TooLong { len } => {
                write!(f, "a block of {len} bytes is over {MAX_BLOCK_LEN}")
            }
            SummaryError::Invalid(what) => write!(f, "{what}"),
            SummaryError::ScaleBytes(e) => write!(f, "scale bytes: {e}"),
            SummaryError::Bound(e) => write!(f, "{e}"),
        }
    }
}

impl core::error::Error for SummaryError {}

impl From<BoundError> for SummaryError {
    fn from(e: BoundError) -> Self {
        SummaryError::Bound(e)
    }
}

/// The scale bytes of a block with `head`: empty for an event tree.
fn scale_bytes<'a>(
    head: &SummaryHead,
    buf: &'a mut [u8; SCALE_LEN_MAX],
) -> Result<&'a [u8], SummaryError> {
    match head.layout {
        Layout::Events(_) => Ok(&[]),
        Layout::Series { scale, .. } => scale.to_ww_bytes(buf).map_err(SummaryError::ScaleBytes),
    }
}

/// Bytes of a summary block with `head`.
pub fn summary_len(head: &SummaryHead) -> Result<usize, SummaryError> {
    let mut scale = [0u8; SCALE_LEN_MAX];
    let scale_len = scale_bytes(head, &mut scale)?.len();
    usize::try_from(head.node_count())
        .ok()
        .zip(head.node_len())
        .and_then(|(n, len)| n.checked_mul(len))
        .and_then(|n| n.checked_add(SUMMARY_MIN_LEN))
        .and_then(|n| n.checked_add(scale_len))
        .ok_or(SummaryError::TooLong { len: usize::MAX })
}

/// A head field past the block the writer sized: never, every offset is inside [SUMMARY_MIN_LEN].
const OWN_LAYOUT: SummaryError = SummaryError::Invalid("summary head past the block it sized");

/// Writes one summary block into a buffer: the head at [Self::new], the columns ([Self::body_mut]), the CRC at
/// [Self::finish].
pub struct SummaryWriter<'b> {
    /// Block header, summary head and scale
    head: &'b [u8],
    body: &'b mut [u8],
    crc: &'b mut [u8; CRC_LEN],
    len: usize,
    nodes: usize,
}

impl<'b> SummaryWriter<'b> {
    /// Write the header and head of a summary block with `head`, its body zero until filled in.
    pub fn new(
        buf: &'b mut [u8],
        id: crate::BlockId,
        head: &SummaryHead,
    ) -> Result<Self, SummaryError> {
        head.check()?;
        let len = summary_len(head)?;
        if len > MAX_BLOCK_LEN {
            return Err(SummaryError::TooLong { len });
        }
        let too_long = |_| SummaryError::TooLong { len };
        let nodes = usize::try_from(head.node_count()).map_err(too_long)?;
        let count = u32::try_from(nodes).map_err(too_long)?;
        let buf = buf
            .get_mut(..len)
            .ok_or(SummaryError::Full { needed: len })?;
        buf.fill(0);
        crate::put_header(buf, BlockKind::Summary, len, count, id)
            .ok_or(SummaryError::TooLong { len })?;
        let (element, fields, channels) = match head.layout {
            Layout::Events(f) => (0, f.bits(), 0),
            Layout::Series {
                element,
                channels,
                stats,
                ..
            } => (element.code(), stats.bits(), channels),
        };
        let mut scale = [0u8; SCALE_LEN_MAX];
        let scale = scale_bytes(head, &mut scale)?;
        let scale_len = u16::try_from(scale.len()).map_err(|_| OWN_LAYOUT)?;
        let tree = [head.tree.to_u8(), head.level, head.node_shift, element];
        let written = [
            bytes::put_u64(buf, 32, head.track_id),
            bytes::put_u32(buf, 40, head.track_number),
            bytes::put(buf, 44, tree),
            bytes::put_u16(buf, 48, fields),
            bytes::put_u16(buf, 50, channels),
            bytes::put_u16(buf, 52, scale_len),
            bytes::put_u64(buf, 56, head.first_item),
            bytes::put_u64(buf, 64, head.items),
            bytes::put_i64(buf, 72, head.first_tick),
            bytes::put_i64(buf, 80, head.last_tick),
            bytes::put_i64(buf, 88, head.latest_end),
        ];
        if written.contains(&None) {
            return Err(OWN_LAYOUT);
        }
        let body_at = SCALE_AT.checked_add(scale.len()).ok_or(OWN_LAYOUT)?;
        let (head_bytes, rest) = buf.split_at_mut_checked(body_at).ok_or(OWN_LAYOUT)?;
        head_bytes
            .get_mut(SCALE_AT..)
            .ok_or(OWN_LAYOUT)?
            .copy_from_slice(scale);
        let (body, crc) = rest.split_last_chunk_mut::<CRC_LEN>().ok_or(OWN_LAYOUT)?;
        Ok(SummaryWriter {
            head: head_bytes,
            body,
            crc,
            len,
            nodes,
        })
    }

    /// The body: the columns back to back, [Summary::body] order.
    pub fn body_mut(&mut self) -> &mut [u8] {
        self.body
    }

    /// Nodes in the block.
    pub fn nodes(&self) -> usize {
        self.nodes
    }

    /// Write the CRC; returns the block's length.
    pub fn finish(self) -> usize {
        let mut crc = crate::CRC.digest();
        crc.update(self.head);
        crc.update(self.body);
        *self.crc = crc.finalize().to_le_bytes();
        self.len
    }
}

/// The scale of a series block: a Magnitude that takes exactly its bytes, written the one way it is written.
fn read_scale(bytes: &[u8]) -> Result<Magnitude, SummaryError> {
    if bytes.is_empty() {
        return Err(SummaryError::Invalid("a series block without a scale"));
    }
    crate::series::read_scale(bytes).map_err(SummaryError::ScaleBytes)
}

#[cfg(test)]
mod head_tests;
#[cfg(test)]
mod tests;
