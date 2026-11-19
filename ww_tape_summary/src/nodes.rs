//! The nodes of a summary block: read in place, and written into a block's body.

use exact_units::Magnitude;

#[cfg(feature = "std")]
use super::SUMMARY_MIN_LEN;
#[cfg(feature = "alloc")]
use super::SeriesNode;
use super::{
    ChannelStats, Layout, Stat, SummaryError, SummaryHead, SummaryWriter, Value, components,
};
use ww_tape::{Block, BlockKind, Element};
use ww_tape_codecs::bytes::{self, le_u32, le_u64};

/// The summary of a summary block, read in place.
#[derive(Copy, Clone, Debug)]
pub struct Summary<'b> {
    head: SummaryHead,
    body: &'b [u8],
    nodes: usize,
}

impl<'b> Summary<'b> {
    /// The summary of a summary block whose body this reader knows ([Block::body_known]); None for another block,
    /// or one whose head or body length breaks the format.
    pub fn of_block(block: &Block<'b>) -> Option<Self> {
        if !block.body_known() || block.kind != BlockKind::Summary {
            return None;
        }
        Self::read(block.bytes()).ok()
    }

    /// Read and check the summary block `block` (exactly its bytes, CRC checked by the caller).
    pub fn read(block: &'b [u8]) -> Result<Self, SummaryError> {
        let (head, body) = SummaryHead::read(block)?;
        let nodes = usize::try_from(head.node_count())
            .map_err(|_| SummaryError::Invalid("node count past usize"))?;
        Ok(Summary { head, body, nodes })
    }

    pub fn head(&self) -> SummaryHead {
        self.head
    }

    /// The scale of a series block's numbers, None in an event tree.
    pub fn scale(&self) -> Option<Magnitude> {
        match self.head.layout {
            Layout::Series { scale, .. } => Some(scale),
            Layout::Events(_) => None,
        }
    }

    /// Nodes in the block.
    pub fn node_count(&self) -> usize {
        self.nodes
    }

    /// The body, columns back to back.
    pub fn body(&self) -> &'b [u8] {
        self.body
    }

    /// The `N` bytes of value `i` of the column at byte `column_at`, None past the body.
    fn word<const N: usize>(&self, column_at: usize, i: usize) -> Option<[u8; N]> {
        bytes::array(self.body, column_at.checked_add(i.checked_mul(N)?)?)
    }

    /// Node `i` of an event tree (0 is the first node); None past the last or in a series.
    pub fn event_node(&self, i: usize) -> Option<EventNode> {
        let Layout::Events(f) = self.head.layout else {
            return None;
        };
        if i >= self.nodes {
            return None;
        }
        // the columns: first ticks and last ticks of 8 bytes, flags of 4, latest ends of 8
        let n = self.nodes;
        let last_at = n.checked_mul(8)?;
        let flags_at = n.checked_mul(16)?;
        let first_tick = i64::from_le_bytes(self.word(0, i)?);
        let last_tick = i64::from_le_bytes(self.word(last_at, i)?);
        let (flags, end_at) = if f.flags {
            let v = u32::from_le_bytes(self.word(flags_at, i)?);
            (v, flags_at.checked_add(n.checked_mul(4)?)?)
        } else {
            (0, flags_at)
        };
        let latest_end = if f.latest_end {
            i64::from_le_bytes(self.word(end_at, i)?)
        } else {
            last_tick
        };
        Some(EventNode {
            first_tick,
            last_tick,
            flags,
            latest_end,
        })
    }

    /// The column of `stat` for `channel` and `component` of a series, at the block's [scale](Self::scale); None
    /// when the block doesn't hold it.
    pub fn series_column(&self, stat: Stat, channel: u16, component: u8) -> Option<StatColumn<'b>> {
        let Layout::Series {
            element,
            channels,
            stats,
            ..
        } = self.head.layout
        else {
            return None;
        };
        if !stats.has(stat) || channel >= channels {
            return None;
        }
        let components = stat.components(element);
        if component >= components {
            return None;
        }
        // values per node before the column: the statistics before `stat`, then the channels and components before
        let mut before = 0usize;
        for s in Stat::ALL.into_iter().take_while(|s| *s != stat) {
            if stats.has(s) {
                let per_channel = s.width().checked_mul(usize::from(s.components(element)))?;
                before = before.checked_add(per_channel.checked_mul(usize::from(channels))?)?;
            }
        }
        let width = stat.width();
        let col = usize::from(channel)
            .checked_mul(usize::from(components))?
            .checked_add(usize::from(component))?;
        let from = before
            .checked_add(col.checked_mul(width)?)?
            .checked_mul(self.nodes)?;
        let bytes = self
            .body
            .get(from..)?
            .get(..width.checked_mul(self.nodes)?)?;
        Some(StatColumn {
            stat,
            element,
            bytes,
            nodes: self.nodes,
        })
    }

    /// The statistics of `channel` in node `i` of a series, at the block's [scale](Self::scale); None past the last
    /// node or channel, or in an event tree. Statistics the block doesn't hold read as their [ChannelStats::empty]
    /// values.
    pub fn channel_stats(&self, channel: u16, i: usize) -> Option<ChannelStats> {
        let Layout::Series {
            element, channels, ..
        } = self.head.layout
        else {
            return None;
        };
        if i >= self.nodes || channel >= channels {
            return None;
        }
        let mut s = ChannelStats::empty(element);
        let column = |stat, c| self.series_column(stat, channel, c);
        let get = |stat, c| column(stat, c).and_then(|col| col.get(i));
        let word = |stat| column(stat, 0).and_then(|col| col.word(i));
        let per_component = s
            .min
            .iter_mut()
            .zip(&mut s.max)
            .zip(s.sum.iter_mut().zip(&mut s.sum_sq));
        for (c, ((min, max), (sum, sum_sq))) in (0..components(element)).zip(per_component) {
            if let Some(v) = get(Stat::Min, c) {
                *min = v;
            }
            if let Some(v) = get(Stat::Max, c) {
                *max = v;
            }
            if let Some(Value::F64(v)) = get(Stat::Sum, c) {
                *sum = v;
            }
            if let Some(Value::F64(v)) = get(Stat::SumSq, c) {
                *sum_sq = v;
            }
        }
        if let Some(Value::U64(v)) = get(Stat::Clipped, 0) {
            s.clipped = v;
        }
        if let Some(v) = word(Stat::Flags) {
            s.flags = v;
        }
        if let Some(v) = word(Stat::PowerMean) {
            s.power_mean = f32::from_bits(v);
        }
        if let Some(v) = word(Stat::PowerMax) {
            s.power_max = f32::from_bits(v);
        }
        Some(s)
    }

    /// Node `i` of a series, every channel, at the block's scale; None past the last node or in an event tree.
    #[cfg(feature = "alloc")]
    pub fn series_node(&self, i: usize) -> Option<SeriesNode> {
        let Layout::Series {
            channels, scale, ..
        } = self.head.layout
        else {
            return None;
        };
        let channels = (0..channels)
            .map(|ch| self.channel_stats(ch, i))
            .collect::<Option<_>>()?;
        Some(SeriesNode { scale, channels })
    }
}

/// One statistic of one channel and component over the nodes of a summary block.
#[derive(Copy, Clone, Debug)]
pub struct StatColumn<'b> {
    stat: Stat,
    element: Element,
    bytes: &'b [u8],
    nodes: usize,
}

impl StatColumn<'_> {
    /// Values in the column: the block's node count.
    pub fn len(&self) -> usize {
        self.nodes
    }

    pub fn is_empty(&self) -> bool {
        self.nodes == 0
    }

    /// The value of node `i`, None past the last: minimum and maximum by the element's kind, sums and powers as
    /// [Value::F64], counts and flags as [Value::U64].
    pub fn get(&self, i: usize) -> Option<Value> {
        let at = i.checked_mul(self.stat.width())?;
        Some(match self.stat {
            Stat::Min | Stat::Max => Value::from_bits(self.element, le_u64(self.bytes, at)?),
            Stat::Sum | Stat::SumSq => Value::F64(f64::from_bits(le_u64(self.bytes, at)?)),
            Stat::Clipped => Value::U64(le_u64(self.bytes, at)?),
            Stat::Flags => Value::U64(u64::from(self.word(i)?)),
            Stat::PowerMean | Stat::PowerMax => {
                Value::F64(f64::from(f32::from_bits(self.word(i)?)))
            }
        })
    }

    /// The 4 bytes of node `i` in a column of 4-byte values (flags, powers); None past the last or in another column.
    fn word(&self, i: usize) -> Option<u32> {
        if self.stat.width() != 4 {
            return None;
        }
        le_u32(self.bytes, i.checked_mul(4)?)
    }
}

/// A node of an event tree, or the merge of consecutive nodes.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct EventNode {
    /// Start of the node's first item.
    pub first_tick: i64,
    /// Start of the node's last item.
    pub last_tick: i64,
    /// OR of the items' flags; 0 when the track has none.
    pub flags: u32,
    /// The largest end of any item; the last tick when every item is an instant.
    pub latest_end: i64,
}

impl EventNode {
    /// A node of one item.
    pub const fn item(start: i64, end: i64, flags: u32) -> Self {
        EventNode {
            first_tick: start,
            last_tick: start,
            flags,
            latest_end: if end > start { end } else { start },
        }
    }

    /// Merge `next`, the node right after this one, into this one.
    pub fn merge(&mut self, next: &EventNode) {
        self.last_tick = next.last_tick;
        self.flags |= next.flags;
        self.latest_end = self.latest_end.max(next.latest_end);
    }
}

/// A column value past the body the writer sized: never, the body holds every column of every node.
const OWN_BODY: SummaryError = SummaryError::Invalid("column past the body it sized");

/// Writes the columns of a body one value after the other.
struct Columns<'a> {
    body: &'a mut [u8],
    at: usize,
}

impl Columns<'_> {
    fn put<const N: usize>(&mut self, v: [u8; N]) -> Result<(), SummaryError> {
        bytes::put(self.body, self.at, v).ok_or(OWN_BODY)?;
        self.at = self.at.checked_add(N).ok_or(OWN_BODY)?;
        Ok(())
    }

    /// Check that every byte was written.
    fn done(self) -> Result<(), SummaryError> {
        if self.at == self.body.len() {
            Ok(())
        } else {
            Err(OWN_BODY)
        }
    }
}

/// Encode an event tree block of `nodes` (one per node of `head`, in order) into `buf`; returns its length.
pub fn write_event_summary(
    buf: &mut [u8],
    id: ww_tape::BlockId,
    head: &SummaryHead,
    nodes: &[EventNode],
) -> Result<usize, SummaryError> {
    let Layout::Events(f) = head.layout else {
        return Err(SummaryError::Invalid("not an event tree"));
    };
    let mut w = SummaryWriter::new(buf, id, head)?;
    if nodes.len() != w.nodes() {
        return Err(SummaryError::Invalid("node count differs from the head"));
    }
    let mut out = Columns {
        body: w.body_mut(),
        at: 0,
    };
    for v in nodes {
        out.put(v.first_tick.to_le_bytes())?;
    }
    for v in nodes {
        out.put(v.last_tick.to_le_bytes())?;
    }
    if f.flags {
        for v in nodes {
            out.put(v.flags.to_le_bytes())?;
        }
    }
    if f.latest_end {
        for v in nodes {
            out.put(v.latest_end.to_le_bytes())?;
        }
    }
    out.done()?;
    Ok(w.finish())
}

/// Encode a series summary block into `buf`: `nodes[i][ch]` is channel `ch` of node `i` of `head`; returns its
/// length.
pub fn write_series_summary(
    buf: &mut [u8],
    id: ww_tape::BlockId,
    head: &SummaryHead,
    nodes: &[&[ChannelStats]],
) -> Result<usize, SummaryError> {
    let Layout::Series {
        element,
        channels,
        stats,
        ..
    } = head.layout
    else {
        return Err(SummaryError::Invalid("not a series"));
    };
    let mut w = SummaryWriter::new(buf, id, head)?;
    if nodes.len() != w.nodes() || nodes.iter().any(|c| c.len() != usize::from(channels)) {
        return Err(SummaryError::Invalid(
            "nodes or channels differ from the head",
        ));
    }
    let mut out = Columns {
        body: w.body_mut(),
        at: 0,
    };
    let missing = SummaryError::Invalid("nodes or channels differ from the head");
    for stat in Stat::ALL.into_iter().filter(|s| stats.has(*s)) {
        for ch in 0..usize::from(channels) {
            for c in 0..usize::from(stat.components(element)) {
                for node in nodes {
                    let s = node.get(ch).ok_or(missing)?;
                    let part = |v: &[Value; 2]| v.get(c).copied().ok_or(missing);
                    let sum = |v: &[f64; 2]| v.get(c).copied().ok_or(missing);
                    match stat {
                        Stat::Min => out.put(part(&s.min)?.to_bits().to_le_bytes()),
                        Stat::Max => out.put(part(&s.max)?.to_bits().to_le_bytes()),
                        Stat::Sum => out.put(sum(&s.sum)?.to_le_bytes()),
                        Stat::SumSq => out.put(sum(&s.sum_sq)?.to_le_bytes()),
                        Stat::Clipped => out.put(s.clipped.to_le_bytes()),
                        Stat::Flags => out.put(s.flags.to_le_bytes()),
                        Stat::PowerMean => out.put(s.power_mean.to_le_bytes()),
                        Stat::PowerMax => out.put(s.power_max.to_le_bytes()),
                    }?;
                }
            }
        }
    }
    out.done()?;
    Ok(w.finish())
}

/// Nodes per block of a level: the most, a power of two, whose block of `node_len` bytes per node stays within
/// `max_block_bytes`; at least 1. Batches start on multiples of it, so a block's nodes are the children of whole
/// parents when it is 16 or more.
#[cfg(feature = "std")]
pub(super) fn batch_nodes(node_len: usize, max_block_bytes: usize) -> u64 {
    let room = max_block_bytes.saturating_sub(SUMMARY_MIN_LEN);
    let fit = room.checked_div(node_len).unwrap_or(room).max(1);
    // the largest power of two at most `fit`, which is 1 and up
    let power = 1usize.checked_shl(fit.ilog2()).unwrap_or(1);
    u64::try_from(power).unwrap_or(u64::MAX)
}
