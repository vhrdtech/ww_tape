//! The summaries of the typed handles' tracks on the writing thread: the leaves a handle computed go into a tree per
//! track and piece, whose blocks are deferred into the piece.
//!
//! # After a crash
//!
//! A resumed mux rebuilds the trees of the piece the crash tore before it writes anything ([Trees::rebuild]): each
//! chunk the piece kept is read back and its leaves computed again, as its handle computed them, into the trees as
//! they grew. The summary blocks the piece kept are not deferred again, those the crash lost are, in the order they
//! were made; so the piece goes on as it would have without the crash.

use std::collections::{BTreeMap, BTreeSet};

use shrink_wrap::DeserializeShrinkWrap;
use ww_tape::chunk::ChunkHead;
use ww_tape::index::IndexEntry;
use ww_tape::{
    Block, BlockKind, Definitions, Describe, FromFile, ItemNum, MAX_BLOCK_LEN, PieceChain,
    PieceStore, Ratio64, SampleShape, Slot, TapeSink, TrackId, TrackNum, WriteError,
};
use ww_tape_summary::{
    BuildError, EventFields, EventTree, ItemRun, SeriesLeaf, SeriesStart, SeriesSummarySpec,
    SeriesTree,
};

/// The leaves of one chunk's items, as its handle computed them.
pub(crate) enum ChunkSummary {
    Events {
        fields: EventFields,
        /// Runs within one level-0 node each, with their first items
        runs: Vec<(ItemNum, ItemRun)>,
    },
    Series {
        shape: SampleShape,
        period: Ratio64,
        spec: SeriesSummarySpec,
        leaves: Vec<SeriesLeaf>,
    },
}

enum Tree {
    Events(EventTree),
    Series(Box<SeriesTree>),
}

impl Tree {
    fn next_item(&self) -> ItemNum {
        match self {
            Self::Events(t) => t.next_item(),
            Self::Series(t) => t.next_item(),
        }
    }

    fn finish(&mut self) -> Result<Vec<Vec<u8>>, BuildError> {
        match self {
            Self::Events(t) => {
                t.finish()?;
                Ok(t.take_blocks())
            }
            Self::Series(t) => {
                t.finish()?;
                Ok(t.take_blocks())
            }
        }
    }

    fn take_blocks(&mut self) -> Vec<Vec<u8>> {
        match self {
            Self::Events(t) => t.take_blocks(),
            Self::Series(t) => t.take_blocks(),
        }
    }
}

/// What became of a chunk's summary.
pub(crate) enum Added {
    Done,
    /// The leaves didn't fit the track's tree: a handle's bug. The tree is dropped; the next chunk starts another.
    Refused,
}

/// A summary block by what it covers: its track, tree, level and items. A piece holds one block of each.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Covers {
    track_id: TrackId,
    number: u32,
    tree: u8,
    level: u8,
    first_item: u64,
    count: u64,
}

impl Covers {
    fn of(e: &IndexEntry) -> Option<Self> {
        (e.kind == BlockKind::Summary).then_some(Self {
            track_id: e.track_id,
            number: e.number,
            tree: e.tree,
            level: e.level,
            first_item: e.first_item.get(),
            count: e.count,
        })
    }

    fn of_block(block: &[u8]) -> Option<Self> {
        let b = Block::read(block).ok()?;
        Self::of(&IndexEntry::of_block(0, &b)?)
    }
}

/// The summary trees of the piece being written, one per track with leaves in it.
pub(crate) struct Trees {
    /// The piece the trees are of
    seq: u64,
    trees: BTreeMap<TrackNum, Tree>,
    /// Summary blocks of the piece a resume kept: placed before the crash, not deferred again
    kept: BTreeSet<Covers>,
    /// Trees a resume rebuilt of which the piece kept no block: the mux may not have built them (a track written
    /// without summaries), dropped when their track's next chunk comes without a summary
    unconfirmed: BTreeSet<TrackNum>,
    /// Tracks a handle without summaries was made for: a resume that comes after builds no tree for them
    plain: BTreeSet<TrackNum>,
}

impl Trees {
    pub(crate) fn new(seq: u64) -> Self {
        Self {
            seq,
            trees: BTreeMap::new(),
            kept: BTreeSet::new(),
            unconfirmed: BTreeSet::new(),
            plain: BTreeSet::new(),
        }
    }

    /// Rebuild the trees of the piece a resume kept the blocks `entries` of: each chunk of a track `defs` holds read
    /// back and added as its handle added it. The kept summary blocks are not deferred again; those lost are, in the
    /// order they were made. Before anything else is written after the resume. The chunks whose summaries didn't
    /// fit their tree, refused.
    pub(crate) fn rebuild<T: PieceStore>(
        &mut self,
        chain: &mut PieceChain<T>,
        defs: &Definitions,
        entries: &[IndexEntry],
    ) -> Result<u64, WriteError<T::Error>> {
        self.seq = chain.seq();
        self.kept = entries.iter().filter_map(Covers::of).collect();
        let mut refused = 0u64;
        let mut buf = Vec::new();
        let plain: BTreeSet<u32> = self.plain.iter().map(|t| t.get()).collect();
        for e in entries
            .iter()
            .filter(|e| matches!(e.kind, BlockKind::Events | BlockKind::Series))
            .filter(|e| !plain.contains(&e.number))
        {
            let Some((head, summary)) = read_back(chain.writer()?.sink(), defs, e, &mut buf)
                .map_err(ww_tape::sink::sink_err("reading a kept chunk back"))?
            else {
                continue;
            };
            if let Added::Refused = self.add(chain, defs, &head, summary)? {
                refused = refused.saturating_add(1);
            }
        }
        let kept_of: BTreeSet<u32> = self.kept.iter().map(|c| c.number).collect();
        self.unconfirmed = self
            .trees
            .keys()
            .filter(|t| !kept_of.contains(&t.get()))
            .copied()
            .collect();
        Ok(refused)
    }

    /// A handle of `track` that writes no summaries is made: the tree a resume rebuilt of it, whatever the piece
    /// kept, is none the run built.
    pub(crate) fn drop_rebuilt(&mut self, track: TrackNum) {
        self.plain.insert(track);
        self.unconfirmed.remove(&track);
        self.trees.remove(&track);
    }

    /// A chunk of `track` placed without a summary: a tree rebuilt at the resume that no kept block confirms is
    /// dropped, as the mux didn't build it.
    pub(crate) fn plain(&mut self, track: TrackNum) {
        if self.unconfirmed.remove(&track) {
            self.trees.remove(&track);
        }
    }

    /// Defer the summary blocks `blocks` into the piece, but those the piece kept at a resume.
    fn defer<T: PieceStore>(
        &mut self,
        chain: &mut PieceChain<T>,
        defs: &Definitions,
        blocks: Vec<Vec<u8>>,
    ) -> Result<(), WriteError<T::Error>> {
        for b in blocks {
            let kept =
                !self.kept.is_empty() && Covers::of_block(&b).is_some_and(|c| self.kept.remove(&c));
            if !kept {
                chain.defer_block(defs, b)?;
            }
        }
        Ok(())
    }

    /// Go on in piece `seq`: what a resume kept of another piece no longer counts.
    fn enter(&mut self, seq: u64) {
        if self.seq != seq {
            self.kept.clear();
            self.unconfirmed.clear();
        }
        self.seq = seq;
    }

    /// End every tree: their last blocks deferred into the piece. Before every cut and the close.
    pub(crate) fn end<T: PieceStore>(
        &mut self,
        chain: &mut PieceChain<T>,
        defs: &Definitions,
    ) -> Result<Vec<BuildError>, WriteError<T::Error>> {
        let mut refused = Vec::new();
        for (_, mut tree) in std::mem::take(&mut self.trees) {
            match tree.finish() {
                Ok(blocks) => self.defer(chain, defs, blocks)?,
                Err(e) => refused.push(e),
            }
        }
        self.enter(chain.seq());
        Ok(refused)
    }

    /// Add the summary of the chunk `head`, just placed.
    ///
    /// The trees are of one piece. The mux ends them before each cut it makes; a cut the chain made inside a write
    /// (a piece out of room for its state entries) ends them here, their last blocks then in the next piece.
    pub(crate) fn add<T: PieceStore>(
        &mut self,
        chain: &mut PieceChain<T>,
        defs: &Definitions,
        head: &ChunkHead,
        summary: ChunkSummary,
    ) -> Result<Added, WriteError<T::Error>> {
        if self.seq != chain.seq() && !self.trees.is_empty() {
            self.end(chain, defs)?;
        }
        self.enter(chain.seq());
        let track = head.track_number;
        self.unconfirmed.remove(&track);
        // a gap in the items (a chunk dropped) ends the track's tree: the next one starts past it
        if self
            .trees
            .get(&track)
            .is_some_and(|t| t.next_item() != head.first_item)
            && let Some(mut tree) = self.trees.remove(&track)
        {
            match tree.finish() {
                Ok(blocks) => self.defer(chain, defs, blocks)?,
                Err(_) => return Ok(Added::Refused),
            }
        }
        let r = self.grow(head, summary);
        let blocks = match (r, self.trees.get_mut(&track)) {
            (Ok(()), Some(tree)) => tree.take_blocks(),
            (Ok(()), None) => Vec::new(),
            (Err(_), _) => {
                self.trees.remove(&track);
                return Ok(Added::Refused);
            }
        };
        self.defer(chain, defs, blocks)?;
        Ok(Added::Done)
    }

    /// Put the leaves of the chunk `head` into its track's tree, started at the chunk when the piece has none.
    fn grow(&mut self, head: &ChunkHead, summary: ChunkSummary) -> Result<(), BuildError> {
        let track = head.track_number;
        match summary {
            ChunkSummary::Events { fields, runs } => {
                let tree = self.trees.entry(track).or_insert_with(|| {
                    Tree::Events(EventTree::new(
                        head.track_id,
                        track,
                        fields,
                        head.first_item,
                    ))
                });
                let Tree::Events(tree) = tree else {
                    return Err(BuildError::Invalid("an events chunk on a series tree"));
                };
                for (first, run) in runs {
                    if first != tree.next_item() {
                        return Err(BuildError::Invalid("a run out of order"));
                    }
                    tree.push_run(run)?;
                }
            }
            ChunkSummary::Series {
                shape,
                period,
                spec,
                leaves,
            } => {
                let grid = head
                    .grid
                    .ok_or(BuildError::Invalid("a series chunk without a grid"))?;
                let tree = match self.trees.entry(track) {
                    std::collections::btree_map::Entry::Occupied(o) => o.into_mut(),
                    std::collections::btree_map::Entry::Vacant(v) => {
                        let start = SeriesStart {
                            track_id: head.track_id,
                            track_number: track,
                            shape,
                            first_item: head.first_item,
                            grid,
                            period,
                            tile: ww_tape::TileCoords::NONE,
                            tile_cells: 1,
                        };
                        v.insert(Tree::Series(Box::new(SeriesTree::new(start, spec)?)))
                    }
                };
                let Tree::Series(tree) = tree else {
                    return Err(BuildError::Invalid("a series chunk on an events tree"));
                };
                tree.set_grid(grid)?;
                tree.samples(u64::from(head.count))?;
                for leaf in leaves {
                    tree.push_leaf(leaf)?;
                }
            }
        }
        Ok(())
    }
}

/// The chunk of entry `e` of the piece `sink` writes, read back into `buf`, and its summary as its handle computed
/// it; None for a block that doesn't read as a chunk of a track `defs` holds with summaries.
fn read_back<S: TapeSink>(
    sink: &S,
    defs: &Definitions,
    e: &IndexEntry,
    buf: &mut Vec<u8>,
) -> Result<Option<(ChunkHead, ChunkSummary)>, S::Error> {
    // the resume walked the block whole: its length is a block's
    let Ok(len) = FromFile::new(e.len).at_most(MAX_BLOCK_LEN) else {
        return Ok(None);
    };
    buf.clear();
    buf.resize(len, 0);
    sink.read_at(e.offset, buf)?;
    let Some(chunk) = Block::read(buf).ok().and_then(|b| b.chunk()) else {
        return Ok(None);
    };
    let head = chunk.head();
    let Some(bytes) = defs.get(&Slot::Track {
        number: head.track_number,
    }) else {
        return Ok(None);
    };
    let Ok(Describe::Track(def)) = Describe::from_ww_bytes(bytes) else {
        return Ok(None);
    };
    if def.id() != head.track_id {
        return Ok(None);
    }
    let summary = match head.kind {
        BlockKind::Events => crate::events::read_summary(&def, &chunk),
        _ => crate::series::read_summary(&def, &chunk),
    };
    Ok(summary.map(|s| (head, s)))
}
