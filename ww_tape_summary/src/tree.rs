//! The item tree of an event track as a writer builds it.

use super::levels::{BuildError, LevelNode, Levels};
use super::{
    EVENT_BASE_SHIFT, EventFields, EventNode, Layout, SummaryHead, Tree, summary_len,
    write_event_summary,
};
use exact_units::Bytes;
use ww_tape::{ItemNum, TrackId, TrackNum};

impl LevelNode for EventNode {
    fn merge(&mut self, next: &Self, _items: u64, _next_items: u64) -> Result<(), BuildError> {
        Self::merge(self, next);
        Ok(())
    }
}

/// Default most bytes of an event tree block.
pub const EVENT_BLOCK_BYTES: Bytes<u32> = Bytes::new(4096);

/// A run of consecutive items whose summary is known (a completed group of the time codec): count, starts, flags OR,
/// latest end.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ItemRun {
    /// Items in the run, 1 and up.
    pub count: u64,
    /// The first item's start.
    pub first_tick: i64,
    /// The last item's start.
    pub last_tick: i64,
    /// OR of the items' flags.
    pub flags: u32,
    /// The largest end of any item, at least `last_tick`.
    pub latest_end: i64,
}

/// Builds the item tree of one event track as items are completed, and encodes its summary blocks.
///
/// Items go in in item order ([Self::push] one by one, [Self::push_run] a run inside one level-0 node). Finished
/// blocks wait in [Self::take_blocks] for [TapeWriter::defer_block](ww_tape::TapeWriter::defer_block); [Self::finish]
/// writes the partial nodes at the end.
pub struct EventTree {
    track_id: TrackId,
    track_number: TrackNum,
    fields: EventFields,
    origin: u64,
    next_item: u64,
    /// The open level-0 node
    acc: Option<EventNode>,
    levels: Levels<EventNode>,
    blocks: Vec<Vec<u8>>,
    /// Last start of the last closed level-0 node
    last_tick: Option<i64>,
}

impl EventTree {
    /// The tree of track `track_id` (number `track_number`) from item `first_item` on; `fields` says whether the
    /// track has flags and spans.
    pub fn new(
        track_id: TrackId,
        track_number: TrackNum,
        fields: EventFields,
        first_item: ItemNum,
    ) -> Self {
        Self {
            track_id,
            track_number,
            fields,
            origin: first_item.get(),
            next_item: first_item.get(),
            acc: None,
            levels: Levels::new(
                first_item.get(),
                EVENT_BASE_SHIFT,
                fields.node_len(),
                crate::block_room(EVENT_BLOCK_BYTES),
            ),
            blocks: Vec::new(),
            last_tick: None,
        }
    }

    /// Keep blocks within `bytes` (default [EVENT_BLOCK_BYTES]); call before the first item.
    pub fn set_max_block_bytes(&mut self, bytes: Bytes<u32>) {
        self.levels = Levels::new(
            self.origin,
            EVENT_BASE_SHIFT,
            self.fields.node_len(),
            crate::block_room(bytes),
        );
    }

    /// The next item's number.
    pub fn next_item(&self) -> ItemNum {
        ItemNum::new(self.next_item)
    }

    /// Add the next item: its start, end (the start for an instant) and flags. Items come in order of start.
    pub fn push(&mut self, start: i64, end: i64, flags: u32) -> Result<(), BuildError> {
        self.push_run(ItemRun {
            count: 1,
            first_tick: start,
            last_tick: start,
            flags,
            latest_end: end.max(start),
        })
    }

    /// Add the next `run.count` items at once; the run must lie inside one level-0 node (2048 items).
    pub fn push_run(&mut self, run: ItemRun) -> Result<(), BuildError> {
        if run.count == 0 {
            return Err(BuildError::Invalid("a run of no items"));
        }
        if run.last_tick < run.first_tick || run.latest_end < run.last_tick {
            return Err(BuildError::Invalid("run ticks out of order"));
        }
        if !self.fields.latest_end && run.latest_end != run.last_tick {
            return Err(BuildError::Invalid("a span in a tree of instants"));
        }
        let shift = EVENT_BASE_SHIFT;
        let end = self
            .next_item
            .checked_add(run.count)
            .ok_or(BuildError::Invalid("item numbers past u64"))?;
        let last = end
            .checked_sub(1)
            .ok_or(BuildError::Invalid("a run of no items"))?;
        if (self.next_item >> shift) != (last >> shift) {
            return Err(BuildError::Invalid("a run across a level-0 node"));
        }
        let node = EventNode {
            first_tick: run.first_tick,
            last_tick: run.last_tick,
            flags: if self.fields.flags { run.flags } else { 0 },
            latest_end: run.latest_end,
        };
        if let Some(acc) = &mut self.acc {
            if run.first_tick < acc.last_tick {
                return Err(BuildError::Invalid("items out of order of start"));
            }
            acc.merge(&node);
        } else {
            if let Some(prev) = self.last_tick
                && run.first_tick < prev
            {
                return Err(BuildError::Invalid("items out of order of start"));
            }
            self.acc = Some(node);
        }
        self.next_item = end;
        if end.is_multiple_of(1 << shift) {
            self.close_node()?;
            self.encode()?;
        }
        Ok(())
    }

    /// Hand the open level-0 node up.
    fn close_node(&mut self) -> Result<(), BuildError> {
        let acc = self
            .acc
            .take()
            .ok_or(BuildError::Invalid("no open node to close"))?;
        self.last_tick = Some(acc.last_tick);
        let idx = self
            .last_node()
            .ok_or(BuildError::Invalid("no item to close"))?;
        self.levels.push(0, idx, acc, self.next_item)
    }

    /// The level-0 node of the last item; None before the first.
    fn last_node(&self) -> Option<u64> {
        Some(self.next_item.checked_sub(1)? >> EVENT_BASE_SHIFT)
    }

    /// Write the open nodes of every level, partial ones included, as blocks. Call it once, after the last item.
    pub fn finish(&mut self) -> Result<(), BuildError> {
        if self.acc.is_some() {
            self.close_node()?;
        }
        self.levels.finish(self.next_item)?;
        self.encode()
    }

    fn encode(&mut self) -> Result<(), BuildError> {
        for out in self.levels.take_output() {
            let empty = BuildError::Invalid("a batch holds nodes");
            let (Some(first), Some(last)) = (out.nodes.first(), out.nodes.last()) else {
                return Err(empty);
            };
            let head = SummaryHead {
                track_id: self.track_id,
                track_number: self.track_number,
                tree: Tree::Events,
                level: out.level,
                node_shift: self.levels.shift(out.level)?,
                layout: Layout::Events(self.fields),
                first_item: ItemNum::new(out.first_item),
                items: out.items,
                first_tick: first.first_tick,
                last_tick: last.last_tick,
                latest_end: out.nodes.iter().map(|n| n.latest_end).max().ok_or(empty)?,
                tile: ww_tape::TileCoords::NONE,
            };
            let mut buf = vec![0u8; summary_len(&head)?];
            let len = write_event_summary(
                &mut buf,
                ww_tape::BlockId::new(ww_tape::SessionTag::NONE, 0),
                &head,
                &out.nodes,
            )?;
            buf.truncate(len);
            self.blocks.push(buf);
        }
        Ok(())
    }

    /// Finished summary blocks, oldest first, for [TapeWriter::defer_block](ww_tape::TapeWriter::defer_block).
    pub fn take_blocks(&mut self) -> Vec<Vec<u8>> {
        core::mem::take(&mut self.blocks)
    }

    /// What a live reader takes from memory for `level`: the number of the first node not in a block yet, and the
    /// nodes from it on; the last may be partial (the open node).
    pub fn pending(&self, level: u8) -> (u64, Vec<EventNode>) {
        let (mut first, mut nodes) = self.levels.pending(level);
        if level == 0
            && let Some(acc) = self.acc
        {
            if nodes.is_empty()
                && let Some(n) = self.last_node()
            {
                first = n;
            }
            nodes.push(acc);
        }
        (first, nodes)
    }
}

/// The runs of an event track's items, one per level-0 node at most, as the producer of the items computes them:
/// for [EventTree::push_run] on the writing thread.
///
/// Items go in in order of start ([Self::push]). A run ends where its level-0 node ends and at [Self::complete], at the
/// end of a chunk; [Self::take_runs] gives the runs so far with their first items.
pub struct EventRuns {
    fields: EventFields,
    next_item: u64,
    /// The open run and its first item
    open: Option<(u64, ItemRun)>,
    /// Start of the last item, for the order of starts across runs
    last_start: Option<i64>,
    runs: Vec<(ItemNum, ItemRun)>,
}

impl EventRuns {
    /// Runs of items from `first_item` on of a track whose items have `fields`.
    pub fn new(fields: EventFields, first_item: ItemNum) -> Self {
        Self {
            fields,
            next_item: first_item.get(),
            open: None,
            last_start: None,
            runs: Vec::new(),
        }
    }

    /// The next item's number.
    pub fn next_item(&self) -> ItemNum {
        ItemNum::new(self.next_item)
    }

    /// What the tree the runs go into stores besides each node's ticks.
    pub fn fields(&self) -> EventFields {
        self.fields
    }

    /// Add the next item: its start, end (the start for an instant) and flags, as [EventTree::push] takes them.
    pub fn push(&mut self, start: i64, end: i64, flags: u32) -> Result<(), BuildError> {
        if self.last_start.is_some_and(|last| start < last) {
            return Err(BuildError::Invalid("items out of order of start"));
        }
        if end < start {
            return Err(BuildError::Invalid("run ticks out of order"));
        }
        if !self.fields.latest_end && end != start {
            return Err(BuildError::Invalid("a span in a tree of instants"));
        }
        let end_item = self
            .next_item
            .checked_add(1)
            .ok_or(BuildError::Invalid("item numbers past u64"))?;
        let flags = if self.fields.flags { flags } else { 0 };
        match &mut self.open {
            Some((_, run)) => {
                run.count = run.count.saturating_add(1);
                run.last_tick = start;
                run.flags |= flags;
                run.latest_end = run.latest_end.max(end);
            }
            None => {
                self.open = Some((
                    self.next_item,
                    ItemRun {
                        count: 1,
                        first_tick: start,
                        last_tick: start,
                        flags,
                        latest_end: end,
                    },
                ));
            }
        }
        self.last_start = Some(start);
        self.next_item = end_item;
        if end_item.is_multiple_of(1 << EVENT_BASE_SHIFT) {
            self.complete();
        }
        Ok(())
    }

    /// Add the next items as columns: their starts, ends (None: instants) and flags (None: all 0), each item as
    /// [Self::push] takes it, the same runs. Refused, adding none, when one would be refused or a column's length
    /// isn't the starts'.
    pub fn push_columns(
        &mut self,
        starts: &[i64],
        ends: Option<&[i64]>,
        flags: Option<&[u32]>,
    ) -> Result<(), BuildError> {
        let (Some(&first), Some(&last)) = (starts.first(), starts.last()) else {
            return Ok(());
        };
        if ends.is_some_and(|e| e.len() != starts.len())
            || flags.is_some_and(|f| f.len() != starts.len())
        {
            return Err(BuildError::Invalid("columns of different lengths"));
        }
        if self.last_start.is_some_and(|l| first < l) || !starts.is_sorted() {
            return Err(BuildError::Invalid("items out of order of start"));
        }
        if let Some(ends) = ends {
            if ends.iter().zip(starts).any(|(e, s)| e < s) {
                return Err(BuildError::Invalid("run ticks out of order"));
            }
            if !self.fields.latest_end && ends != starts {
                return Err(BuildError::Invalid("a span in a tree of instants"));
            }
        }
        let n = u64::try_from(starts.len())
            .map_err(|_e| BuildError::Invalid("item numbers past u64"))?;
        self.next_item
            .checked_add(n)
            .ok_or(BuildError::Invalid("item numbers past u64"))?;
        let flags = flags.filter(|_| self.fields.flags);
        let mut at = 0usize;
        while at < starts.len() {
            // the items up to the end of the open run's level-0 node
            let node_end = (self.next_item >> EVENT_BASE_SHIFT)
                .saturating_add(1)
                .saturating_mul(1 << EVENT_BASE_SHIFT);
            let room =
                usize::try_from(node_end.saturating_sub(self.next_item)).unwrap_or(usize::MAX);
            let to = at.saturating_add(room).min(starts.len());
            let (Some(seg), Some(&seg_first), Some(&seg_last)) = (
                starts.get(at..to),
                starts.get(at),
                to.checked_sub(1).and_then(|i| starts.get(i)),
            ) else {
                break;
            };
            let seg_flags = flags
                .and_then(|f| f.get(at..to))
                .map_or(0, |f| f.iter().fold(0, |a, &b| a | b));
            // at least the last start: starts don't decrease
            let seg_end = ends
                .and_then(|e| e.get(at..to))
                .map_or(seg_last, |e| e.iter().copied().fold(seg_last, i64::max));
            let count = u64::try_from(seg.len()).unwrap_or(u64::MAX);
            match &mut self.open {
                Some((_, run)) => {
                    run.count = run.count.saturating_add(count);
                    run.last_tick = seg_last;
                    run.flags |= seg_flags;
                    run.latest_end = run.latest_end.max(seg_end);
                }
                None => {
                    self.open = Some((
                        self.next_item,
                        ItemRun {
                            count,
                            first_tick: seg_first,
                            last_tick: seg_last,
                            flags: seg_flags,
                            latest_end: seg_end,
                        },
                    ));
                }
            }
            self.next_item = self.next_item.saturating_add(count);
            if self.next_item.is_multiple_of(1 << EVENT_BASE_SHIFT) {
                self.complete();
            }
            at = to;
        }
        self.last_start = Some(last);
        Ok(())
    }

    /// End the open run: at the end of a chunk.
    pub fn complete(&mut self) {
        if let Some((first, run)) = self.open.take() {
            self.runs.push((ItemNum::new(first), run));
        }
    }

    /// The runs so far, in order, each with its first item.
    pub fn take_runs(&mut self) -> Vec<(ItemNum, ItemRun)> {
        core::mem::take(&mut self.runs)
    }
}
