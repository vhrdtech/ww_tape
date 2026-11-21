---
level: basic
---

# Summaries

The engineer's viewer opens on the whole hour of `phase_a`, 90 million [[samples]], in a plot 600 pixels wide. Its
[[chunks]] hold 99 617 283 bytes, far too many to read for a picture. The writer kept [[summaries]] beside them, a
coarse view built while the run was recorded, and the viewer draws the hour from those. This chapter shows how. After
it you can draw an hour of a [[track]] from its summaries, zoom in to the samples, and write summaries alongside the
data.

## A tree over item numbers

A [[summary node]] covers a run of a track's [[items]] and holds what a viewer needs to draw them. A node at level 0
covers a power of two of items, 1 024 samples for the drive's current. Each level up, a node covers 16 nodes of the
level below: at level 1, 16 384 samples. The nodes run over [[item numbers]], not time. A sparse track therefore has no
empty nodes, and the nodes of two pieces of one track line up.

A track has one of two kinds of tree:

| Tree | Level 0 | A node holds |
|---|---|---|
| events | 2 048 items | the first and last item's start, the OR of the items' flags, and for spans the latest end |
| series | a power of two of samples, which the track picks | per channel, the statistics the track declares |

A series declares its statistics in its definition, a [`SeriesSummarySpec`]. The drive's `phase_a` keeps the minimum,
the maximum, the sum and the sum of squares. From them a viewer draws the envelope and the root mean square:

```rs
--8<-- "ww_tape_example/src/defs.rs:spec"
```

A node holds no ticks, since the chunks' [[grids]] give every sample's tick. A [[summary block|summary]] holds the
nodes of one level over a stretch of the track. The index lists it like any block, with its level.

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/summary_tree.svg"
<figcaption>Summary nodes of <code>phase_a</code> over its item numbers: each node at level 1 merges 16 nodes of level
0, and each bar spans its node's minimum and maximum. The viewer's pixel columns take the nodes of the coarsest level
that still has a node per column. Drawn by <code>ww_tape_figures</code> from the summary blocks of the drive's
recording at 1/1000 of its data.</figcaption>
</figure>

## Drawing the hour

The viewer picks the coarsest level whose nodes still outnumber its 600 columns. Every column then gets several
nodes, and no column falls between two:

```rs
--8<-- "ww_tape_example/src/fault.rs:level"
```

For the hour of `phase_a` that is level 1, nodes of 16 384 samples. The viewer reads the summary blocks of that level
in each piece, and merges each node into the column its first sample falls in:

```rs
--8<-- "ww_tape_example/src/fault.rs:nodes"
```

The overview reads 172 summary blocks with 5 494 nodes: 193 352 bytes in 172 reads, and no sample. Finding those
blocks costs more than reading them. `phase_a`'s index entries in the four pieces take 788 282 bytes in 20 reads,
since the track has a chunk a second. A viewer that opens the recording once keeps the entries in memory.

All 600 columns get nodes. The column with the highest peak is column 519, from 51:54.000, at 31 873 mA. It holds the
seconds just before the current stops at 52:00.

## Zooming in

Next the viewer zooms into that column. It reads the level-0 nodes of the column alone, from the summary blocks whose
items overlap it, with the same loop. The node with the highest peak covers the 1 024 samples from item 77 999 104, at
51:59.964. The zoom reads 6 756 bytes in 6 reads.

That node lies in the chunk from 51:59.000, which [Series](series.md#grids-where-samples-are-in-time) reads, sample by
sample. The engineer thus went from the hour to the 500 samples before the trip through summaries and one chunk.
[Events](events.md) gave the frames after it, and [Reading a tape](reading.md#state-at-a-place) the mode on either side.

## How nodes merge

Every statistic merges: a parent is its 16 children merged in item order, with [`ChannelStats::merge`] for a series
and [`EventNode::merge`] for events. A reader that builds coarser bins of its own, such as the viewer's columns, gets
exactly what the writer would have stored. A minimum of minimums is the minimum, and sums add.

Nodes of two scales merge at their common scale, as [Changing scale mid-run](series.md#changing-scale-mid-run)
shows. A summary block states its scale, so it reads without the chunks.

## Writing summaries

The crate `ww_tape_summary` builds the trees as the items arrive: an [`EventTree`] for an event track, a
[`SeriesSummaries`] for a series. The drive's recorder starts one for `phase_a` with the track's first grid and
period:

```rs
--8<-- "ww_tape_example/src/record.rs:summaries"
```

Each second, the recorder pushes the second's samples. A level that fills a block gives that block, and the recorder
hands it to the [[writer]] with [`TapeWriter::defer_block`]. The writer places a [[deferred block]] where it fits, in
an aligned file into the gap before a chunk's payload:

```rs
--8<-- "ww_tape_example/src/record.rs:push"
```

At the end of the run, [`SeriesSummaries::finish`] gives the blocks of the partial nodes. The first piece of the
drive's recording holds 730 summary blocks of 821 980 bytes, beside 25 373 597 bytes of samples. A lost summary block
costs nothing that a reader cannot rebuild from the chunks.

The crate `ww_tape` itself reads only a summary block's head, with [`Block::summary_head`], which is enough to list
the block in the index. [`Summary::of_block`] in `ww_tape_summary` reads the nodes. The last chapter of this part
proves that another file holds the same frames as the bench host's recording.
