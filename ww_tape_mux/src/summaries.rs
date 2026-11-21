//! The summaries of the typed handles' tracks on the writing thread: the leaves a handle computed go into a tree per
//! track and piece, whose blocks are deferred into the piece.

use std::collections::BTreeMap;

use ww_tape::chunk::ChunkHead;
use ww_tape::{
    Definitions, ItemNum, PieceChain, Ratio64, SampleShape, Tapes, TrackNum, WriteError,
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

/// The summary trees of the piece being written, one per track with leaves in it.
pub(crate) struct Trees {
    /// The piece the trees are of
    seq: u64,
    trees: BTreeMap<TrackNum, Tree>,
}

impl Trees {
    pub(crate) fn new(seq: u64) -> Self {
        Self {
            seq,
            trees: BTreeMap::new(),
        }
    }

    /// End every tree: their last blocks deferred into the piece. Before every cut and the close.
    pub(crate) fn end<T: Tapes>(
        &mut self,
        chain: &mut PieceChain<T>,
        defs: &Definitions,
    ) -> Result<Vec<BuildError>, WriteError<T::Error>> {
        let mut refused = Vec::new();
        for (_, mut tree) in std::mem::take(&mut self.trees) {
            match tree.finish() {
                Ok(blocks) => defer(chain, defs, blocks)?,
                Err(e) => refused.push(e),
            }
        }
        self.seq = chain.seq();
        Ok(refused)
    }

    /// Add the summary of the chunk `head`, just placed.
    ///
    /// The trees are of one piece. The mux ends them before each cut it makes; a cut the chain made inside a write
    /// (a piece out of room for its state entries) ends them here, their last blocks then in the next piece.
    pub(crate) fn add<T: Tapes>(
        &mut self,
        chain: &mut PieceChain<T>,
        defs: &Definitions,
        head: &ChunkHead,
        summary: ChunkSummary,
    ) -> Result<Added, WriteError<T::Error>> {
        if self.seq != chain.seq() && !self.trees.is_empty() {
            self.end(chain, defs)?;
        }
        self.seq = chain.seq();
        let track = head.track_number;
        // a gap in the items (a chunk dropped) ends the track's tree: the next one starts past it
        if self
            .trees
            .get(&track)
            .is_some_and(|t| t.next_item() != head.first_item)
            && let Some(mut tree) = self.trees.remove(&track)
        {
            match tree.finish() {
                Ok(blocks) => defer(chain, defs, blocks)?,
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
        defer(chain, defs, blocks)?;
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

/// Defer the summary blocks `blocks` into the piece.
fn defer<T: Tapes>(
    chain: &mut PieceChain<T>,
    defs: &Definitions,
    blocks: Vec<Vec<u8>>,
) -> Result<(), WriteError<T::Error>> {
    for b in blocks {
        chain.defer_block(defs, b)?;
    }
    Ok(())
}
