---
level: basic
---

# What the writer adds

A piece holds more than the application's data. A [[reader]] may open it later without the program that wrote it.
It then needs the [[definitions]] of the tracks, an index to find blocks and a way to tell that the piece is complete. Which blocks does the [[writer]] add for that, what does it check, and why does it never go back to change
them?

## Definitions with every call

The writer keeps no table of definitions. Whoever drives it holds them in a [`struct@Definitions`] container and
passes it by reference with every write of data and every put. A mux holds the container itself, so its handles never
name a definition. Before each block, the writer [[puts]] the definitions that the block's tracks need: the clocks,
maps and [[bundles]] a track names, then the track itself.

```rust
--8<-- "ww_tape_tests/tests/it/book.rs:definitions"
```

A put writes a definition only where the same one does not lie [[near]] behind, so most blocks cost no definitions.
The writer refuses data on a track that the container does not hold. It also refuses a chunk whose head gives the
track's number with another id than the definition ([`DescribeError::TrackId`]).

### How many definitions to hold

How many definitions to hold is the application's choice. A recorder of 10 000 sensors may hold 10 000 track
definitions and pass them all with each block. A block of 3 sensors' readings gets only those 3 definitions and their
clock written before it. The writer writes each of them once per 32 MiB, plus one [[state copy]].

Three calls put what belongs to no block of data:

- [`TapeWriter::put_definitions`] puts a track that gets no data yet: its definition and what it needs, by the same
  rule.
- [`TapeWriter::put_item`] puts an item that belongs to no track, such as a description of the run.
- A track added an hour into the recording needs no call of its own: it costs one record at its first block.

The writer refuses a block of definitions that the caller wrote, so every definition in a file lies near the data
that needs it.

## The writer's own blocks

The writer's own blocks hold what a [[reader]] needs to find and understand the data. For the drive's hour they are
the [[piece heads]], the puts, the [[checkpoints]] and the end of each piece:

| Blocks | Made by | Count | Bytes |
|---|---|---:|---:|
| piece head | the writer | 4 | 2064 |
| records blocks of puts and copies | the writer | 32 | 5660 |
| records blocks | the recorder | 3600 | 811071 |
| events chunks | the recorder | 3600 | 8400924 |
| series chunks | the recorder | 3600 | 99617283 |
| summary blocks | the mux | 2948 | 3301304 |
| checkpoints | the writer | 24 | 1584512 |
| final index | the writer | 4 | 1772160 |
| close | the writer | 4 | 11566 |
| tail | the writer | 4 | 656 |

The writer's own blocks take 3.4 MB of the hour's 116 MB, 2.9 %. Most of that is index: the checkpoints while the
pieces grow, and each piece's final index. The 32 blocks of puts hold every definition and mode change of four pieces
in 5.7 kB.

## What the writer checks

The writer checks what it is handed in proportion to the cost of the check:

| Handed over | The writer checks | Passes over the bytes |
|---|---|---|
| a chunk's head and columns ([`TapeWriter::write_chunk`]) | the head, the column table, the track against the definitions, a series chunk's [[ticks]] against its grid | the [[content hash]] only: it copies the columns as they are |
| a whole block ([`TapeWriter::write_block`], [`TapeWriter::adopt_block`], [`TapeWriter::defer_block`]) | every [[CRC]] in it, a [[CRC-32C]] | one to check, then the content hash |

[Samples and their codecs](sample_codecs.md#what-a-chunk-costs-the-writer) measures what these passes cost per chunk.
The writer computes each CRC with the processor's CRC instructions where it has them, and with a table elsewhere, with
the same result.

The writer does not compare a series chunk's grid with the chunk before it. A reader's [`SeriesTimeline`] does that
([Series](series.md#grids-where-samples-are-in-time)).

## Nothing is filled in later

A writer never goes back to change bytes it already wrote. Every block is final once placed, and the file only grows.
This rule gives the format its shape:

- The piece's length and the place of its index are in the tail at the end, not in a header at the start.
- Checkpoints point back at the checkpoint before them, never forward at one not yet written.
- A tail cannot name the next piece's hash, which does not exist yet. It names the next piece's id instead.
- A new definition is a new record. The old one stays, and only readers after the new one see it replaced.
- A state copy is written at a use, never scheduled ahead, so nothing is left pending when the writer stops.
- A crash costs only the bytes not yet synced. No header exists that would be wrong after it.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/append_only.svg"
<figcaption>Every pointer in a piece points back. Each checkpoint names the one before it, the tail names the close,
and the close names the final index. The next piece appears only by its id. Nothing written earlier waits for a later byte.</figcaption>
</figure>

Append-only writing suits every medium a tape targets. A NAND flash page takes one program, and an SD card is fastest
in long sequential writes. A tool can also read a file whole while the file grows, since it is only appended.

## Closing

[`TapeWriter::close`] places the [[deferred blocks]] still waiting and writes the [[final index]], the [[close]] and
the [[tail]]. [Files and syncing](files.md#the-close) follows the syncs that make a file's close durable. The call
returns a [`writer::Closed`], which holds what a next piece or a check needs:

- the piece's [[content hash]], which the next piece's head names;
- the piece's length, a multiple of 4 KiB;
- each track's next [[item number]], where the next piece goes on.

A close ends a write session, not the piece. A writer that opens a closed piece again writes on after its close. The
next close names the one before it ([Closing more than once](format.md#closing-more-than-once)). A piece closed again
with nothing appended stays as it was, to the byte.

[`TapeWriter::close_with_summary`] closes with a summary block of the application's own, up to 1 MiB of any bytes. A
reader fetches it from the tail without a scan ([The close summary](format.md#the-close-summary)).
