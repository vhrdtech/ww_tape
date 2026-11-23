//! A region's timeline: the time pyramids of a grid's tiles in a box of cells, merged node by node.
//!
//! Each tile of a grid has a tree of its own ([summary](ww_tape::summary#tiles)): node `n` of a level covers the same
//! time steps in every tile. [region_timeline] reads the blocks of one level of the tiles a box meets
//! ([TapeReader::grid_summaries]) and merges each node over the tiles, weighting every tile by its cells.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::ops::Range;

use ww_tape::{ByteSource, ItemNum, ReadError, TapeReader, TileBox, TrackId};

use crate::{ScaleError, SeriesNode, Summary, SummaryError};

/// A node of a region's timeline: one node of a level, merged over the tiles of a box.
#[derive(Clone, Debug, PartialEq)]
pub struct RegionNode {
    /// The node's number in its level: it covers time steps `[node << shift, (node + 1) << shift)`.
    pub node: u64,
    /// The first time step a merged tile's node holds.
    pub first_item: ItemNum,
    /// Time steps from the first to the last any merged tile's node holds.
    pub items: u64,
    /// Tiles merged into it.
    pub tiles: u32,
    /// Samples of its statistics: the time steps of each merged tile's node times that tile's cells, summed.
    pub samples: u64,
    /// The statistics of every channel over those samples.
    pub stats: SeriesNode,
}

/// Why a region's timeline can't be read.
#[derive(Debug)]
#[non_exhaustive]
pub enum RegionError<E> {
    /// The reader failed to find or read a block.
    Read(ReadError<E>),
    /// The summary block at `offset` doesn't read.
    Summary { offset: u64, error: SummaryError },
    /// Nodes of two tiles have no common scale.
    Scale(ScaleError),
    /// A block names a tile the box's grid doesn't hold, or a count passes u64.
    Invalid(&'static str),
}

impl<E: core::fmt::Debug> core::fmt::Display for RegionError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Read(e) => write!(f, "{e:?}"),
            Self::Summary { offset, error } => write!(f, "summary block at {offset}: {error}"),
            Self::Scale(e) => write!(f, "{e}"),
            Self::Invalid(what) => write!(f, "{what}"),
        }
    }
}

impl<E: core::fmt::Debug> core::error::Error for RegionError<E> {}

impl<E> From<ReadError<E>> for RegionError<E> {
    fn from(e: ReadError<E>) -> Self {
        Self::Read(e)
    }
}

/// The timeline of the cells `tiles` of grid track `track_id` in piece `i` over the ticks `time`, at summary level
/// `level`: per node of the level, the nodes of every tile the box meets merged, in node order.
///
/// A node summarises a whole tile, so a tile partly inside the box counts whole: the timeline is at tile resolution,
/// the region it covers the box's tiles. Each tile weighs by its cells times its node's time steps, so an edge tile
/// of fewer cells weighs less. A tile without a node at a step adds nothing to it; a step no tile covers is absent.
/// Nodes of two scales merge at their common scale ([SeriesNode::merge]).
///
/// The blocks read are those [TapeReader::grid_summaries] finds: every block meeting `time` gives all its nodes, so
/// the first and last node may reach past `time`. Tiles merge in the order of their coordinates, then their blocks'
/// places, so the same tiles give the same bits.
pub fn region_timeline<S: ByteSource>(
    reader: &TapeReader<S>,
    i: usize,
    track_id: TrackId,
    time: Range<i64>,
    tiles: &TileBox,
    level: u8,
) -> Result<Vec<RegionNode>, RegionError<S::Error>> {
    let mut found = reader.grid_summaries(i, track_id, time, tiles, level)?;
    found.sort_by(|a, b| {
        (a.tile.as_slice(), a.entry.offset).cmp(&(b.tile.as_slice(), b.entry.offset))
    });
    let layout = tiles.layout();
    let past = || RegionError::Invalid("samples of a node past u64");
    let mut nodes: BTreeMap<u64, RegionNode> = BTreeMap::new();
    let mut buf = Vec::new();
    for t in found {
        let cells = layout
            .tile_cells(&t.tile)
            .ok_or(RegionError::Invalid("a tile the box's grid doesn't hold"))?;
        let offset = t.entry.offset;
        let block = reader.read_block(i, &t.entry, &mut buf)?;
        let summary =
            Summary::read(block.bytes()).map_err(|error| RegionError::Summary { offset, error })?;
        let head = summary.head();
        if head.tile != t.tile {
            return Err(RegionError::Summary {
                offset,
                error: SummaryError::Invalid("a tile other than its index entry's"),
            });
        }
        let bad = |what| RegionError::Summary {
            offset,
            error: SummaryError::Invalid(what),
        };
        for k in 0..summary.node_count() {
            let n = u64::try_from(k)
                .ok()
                .and_then(|k| head.first_node().checked_add(k))
                .ok_or(bad("node number past u64"))?;
            let (first, items) = head
                .node_items(u64::try_from(k).map_err(|_e| bad("node past u64"))?)
                .ok_or(bad("a node past its items"))?;
            let stats = summary
                .series_node(k)
                .ok_or(bad("a node of an event tree"))?;
            let samples = items.checked_mul(cells).ok_or_else(past)?;
            merge_into(&mut nodes, n, first, items, samples, stats)?;
        }
    }
    Ok(nodes.into_values().collect())
}

/// Merge a tile's node `n` (`items` time steps from `first`, `samples` samples) into the region's node `n`.
fn merge_into<E>(
    nodes: &mut BTreeMap<u64, RegionNode>,
    n: u64,
    first: u64,
    items: u64,
    samples: u64,
    stats: SeriesNode,
) -> Result<(), RegionError<E>> {
    let past = || RegionError::Invalid("time steps of a node past u64");
    let end = first.checked_add(items).ok_or_else(past)?;
    let Some(r) = nodes.get_mut(&n) else {
        nodes.insert(
            n,
            RegionNode {
                node: n,
                first_item: ItemNum::new(first),
                items,
                tiles: 1,
                samples,
                stats,
            },
        );
        return Ok(());
    };
    r.stats
        .merge(&stats, r.samples, samples)
        .map_err(RegionError::Scale)?;
    let start = r.first_item.get().min(first);
    let r_end = r.first_item.get().checked_add(r.items).ok_or_else(past)?;
    r.first_item = ItemNum::new(start);
    r.items = r_end.max(end).checked_sub(start).ok_or_else(past)?;
    r.tiles = r
        .tiles
        .checked_add(1)
        .ok_or(RegionError::Invalid("tiles of a node past u32"))?;
    r.samples = r
        .samples
        .checked_add(samples)
        .ok_or(RegionError::Invalid("samples of a node past u64"))?;
    Ok(())
}
