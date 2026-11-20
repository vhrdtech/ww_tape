---
level: basic
---

# Summaries

A viewer that draws a track of a billion items over 2000 pixels must not read a billion items. **Summary blocks** hold a
coarse view of a track, built as the track is written, in a tree over item numbers.

A **node** of the tree covers a range of items. A track has one of two kinds of tree: an event tree for an event track,
or series summaries for a series.

This page explains the two kinds of tree, how their nodes merge, and which crate builds and reads them. It assumes
[Events and series](events_and_series.md) and [Series](series.md).

## Event trees

A level-0 node covers 2048 items. A node one level up covers 16 nodes of the level below: 32 768 items, then 524 288,
then 8.4 million.

A node holds these values:

- the first item's start and the last item's start;
- the bitwise OR of the items' flags;
- for [spans](events_and_series.md#chunks), the latest end.

Counts follow from the item range.

## Series summaries

A level-0 node covers a power of two of samples that the track chooses. Each level up again covers 16 nodes.

A node holds, per channel, the statistics the track declares (`SeriesSummarySpec`):

- minimum and maximum;
- sum and sum of squares;
- a count of clipped or NaN samples;
- a flags OR;
- for complex samples, the mean and maximum of `I² + Q²`.

A node holds no ticks, since the chunks' grids give them.

## How nodes merge

Nodes run over item numbers, not time. A sparse track therefore has no empty nodes, and the nodes of consecutive pieces
of one track line up.

Every statistic merges: a parent is its 16 children merged in item order. A reader that builds coarser bins of its own
gets exactly what the writer would have stored.

For example, a track of 1 billion events has about 1900 nodes at level 2, roughly one per pixel of a 2000-pixel row. An
overview reads those nodes and no items.

Zoomed in, a viewer reads the nodes of the visible range, then the chunks they point to.

## Which crate builds and reads summaries

Two crates share the work. The crate `ww_tape_summary` builds the trees as the items arrive (`EventTree`,
`SeriesSummaries`) and reads the nodes of a summary block (`Summary::of_block`).

The application hands the blocks it builds to the writer with `TapeWriter::defer_block`.

The crate `ww_tape` itself reads and checks only a summary block's head (`Block::summary_head`). The head is enough to
list the block in the index.

## What a summary block costs

Summary blocks are small, a few KiB each, and the index lists them like any block. A lost summary block costs nothing
that can't be rebuilt from the data.

The block layout is in the [summary module
docs](https://github.com/vhrdtech/ww_tape/blob/main/ww_tape/src/summary/mod.rs). The nodes are in the [ww_tape_summary
crate docs](https://github.com/vhrdtech/ww_tape/blob/main/ww_tape_summary/src/lib.rs).
