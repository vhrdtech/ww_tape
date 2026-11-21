---
level: basic
---

# Writing a tape

An application that records data hands it to a [[writer]], which turns it into the [[blocks]] of a [[tape]]. What does
the application hand over, and what does the writer add on its own? Four calls carry the data, and the writer adds the
blocks that let a [[reader]] find and understand it. The examples count the calls of the drive's recorder over an hour.

## The writer and its sink

A [`TapeWriter`] turns the application's calls into [[blocks]] of a [[piece]]. The application keeps its [[definitions]]
and hands over the data. The writer adds the blocks that the piece needs around that data. These are [[puts]] of
definitions and [[state]], [[checkpoints]], and at the end the [[final index]], [[close]] and [[tail]].

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/writer_flow.svg"
<figcaption>The application calls the writer with its definitions and its data. The writer adds puts, checkpoints and
the end of the piece, places deferred blocks, and hands its bytes to a sink.</figcaption>
</figure>

The writer puts its bytes into a [[sink]]: any type that implements [`TapeSink`]. A sink appends bytes, syncs them,
reports its size and completes the piece at the close. To resume a piece, it also reads bytes back and truncates them.

Two sinks come with the crates. A [`MemSink`] keeps the bytes in memory, where a [`TapeReader`] reads them while they
are written. A [`FileSink`], in the crate `ww_tape_fs`, writes a file.

A [[chain]] needs one sink per piece, so it takes a [[store]] instead: any type that implements [`Tapes`]. A store makes
the sink of each piece under a key. [`MemTapes`] keeps every piece in memory, and [`Files`] keeps them in a directory or
in one file. A [[raw medium]] or a network store needs a sink and a store of its own.

### Errors of sinks and stores

Each sink and store has its own error type: [`MemError`] for memory and [`FsError`] for files. A [`WriteError`] carries
that error in [`WriteError::Sink`], together with the step that failed. A store refuses to make a tape that exists
already or that another writer holds, and names the tape by its key.

### The writer's settings

The writer's settings are its [`WriterOptions`]. Each field has a default that suits a file on a disk:

| Field | What it sets | Explained in |
|---|---|---|
| `sync` | when the writer syncs | [Files and syncing](files.md#how-often-to-sync) |
| `index` | when the writer places checkpoints | [Anatomy of a piece](anatomy.md#checkpoints-while-the-piece-grows) |
| `layout` | how blocks are placed, such as payloads on 4 KiB | [Aligned files](#aligned-files) |
| `producer` | the producer's name | [Definitions and state](definitions.md#the-definitions-an-application-holds) |
| `near` | how the writer remembers puts and spaces state copies | [Near, puts and copies](definitions.md#near-puts-and-copies) |
| `clock` | the clock that decides when an interval is up | [Files and syncing](files.md#the-writers-clock) |

## Four calls for data

The drive's recorder hands the hour to the writer in four calls. Each second it [[puts]] the mode and writes one
[[records block]] of log lines and temperatures. It also writes a [[chunk]] of CAN frames and one of phase current. It
hands over the [[summary]] blocks of the phase current as they fill. `cargo run -p ww_tape_example` counts the calls:

| Call | Count |
|---|---:|
| [`TapeWriter::put`], the mode | 3600 (12 wrote) |
| [`TapeWriter::write_block`], log lines and temperatures | 3600 |
| [`TapeWriter::write_chunk`], `can0` | 3600 |
| [`TapeWriter::write_chunk`], `phase_a` | 3600 |
| [`TapeWriter::defer_block`], summaries of `phase_a` | 2933 |

Only 12 of the 3600 puts wrote a record. A put writes its [[state item]] only when no record of the same item lies
[[near]] behind, as [Definitions and state](definitions.md#near-puts-and-copies) explains. The recorder therefore puts
the mode before every block and never keeps track of what it put.

### Records blocks

The application builds a [[records block]] with a [`BlockWriter`], as [A first tape](first_tape.md#writing-the-minute)
shows. It hands the finished bytes to [`TapeWriter::write_block`], which returns where the block landed as a
[`ww_tape::Placed`]: its offset from the start of the piece and its length. A block of puts lands as no single block,
and for such a block the call returns `None`.

### Chunks

The application hands a [[chunk head]] and its encoded [[columns]] to [`TapeWriter::write_chunk`]. The writer builds the
chunk block around them and returns its [`ww_tape::Placed`]. For a chunk, [`Placed::unit`] is the chunk's [[unit hash]].
The writer states that hash and does not verify it, so a writer in the same process folds [[range hashes]] without
reading the piece back. [Chunks from columns](columns.md) shows how the recorder encodes its frames and samples.

A [`PieceChain`] writes a chunk with [`PieceChain::write_chunk`], which returns a [`ChunkPlaced`]. It holds the same
fields and the sequence number of the piece that the chunk landed in. The call may [[cut]] to a new piece first, so the
caller learns where the chunk went.

### What each kind of track takes

Each kind of [[track]] takes only its own form of data. A [[state track]] takes its items only through puts. The writer
therefore refuses a caller's record or [[events chunk]] on a state track. A [[series]] has no records: its [[samples]]
go into [[series chunks]], each with its [[grid]] and [[scale]] in the head. The writer refuses a record, a put or an
events chunk on a series track.

## Definitions with every call

The writer keeps no table of definitions. The application holds them in a [`Definitions`] container and passes it by
reference with every write of data and every put. Before each block, the writer puts the [[definitions]] that the
block's tracks need: the clocks, maps and [[bundles]] a track names, then the track itself.

```rs
--8<-- "ww_tape_tests/tests/it/book.rs:definitions"
```

A put writes a definition only where the same one does not lie near behind, so most blocks cost no definitions. The
writer refuses data on a track that the container does not hold. It also refuses a chunk whose head gives the track's
number with another id than the definition ([`DescribeError::TrackId`]).

### How many definitions to hold

How many definitions to hold is the application's choice. A recorder of 10 000 sensors may hold 10 000 track definitions
and pass them all with each block. A block of 3 sensors' readings gets only those 3 definitions and their clock written
before it. The writer writes each of them once per 32 MiB, plus one [[state copy]].

A track added an hour into the recording costs one record at its first block. An item that belongs to no track, such as
a run record, goes in with [`TapeWriter::put_item`]. The writer refuses a block of definitions that the caller wrote, so
every definition in a file lies near the data that needs it.

## What the writer adds

The writer's own blocks hold what a [[reader]] needs to find and understand the data. For the drive's hour they are the
[[piece heads]], the puts, the checkpoints and the end of each piece:

| Blocks | Made by | Count | Bytes |
|---|---|---:|---:|
| piece head | the writer | 4 | 1776 |
| records blocks of puts and copies | the writer | 35 | 5584 |
| records blocks | the recorder | 3600 | 811071 |
| events chunks | the recorder | 3600 | 8400924 |
| series chunks | the recorder | 3600 | 99617283 |
| summary blocks | the recorder | 2933 | 3299294 |
| checkpoints | the writer | 24 | 1485336 |
| final index | the writer | 4 | 1658680 |
| close | the writer | 4 | 9508 |
| tail | the writer | 4 | 656 |

The writer's own blocks take 3.2 MB of the hour's 115 MB, 2.7 %. Most of that is index: the checkpoints while the pieces
grow, and each piece's final index. The 35 blocks of puts hold every definition and mode change of four pieces in 5.6
kB.

### What the writer checks

The writer checks what it is handed in proportion to the cost. For a chunk that it builds from columns, it checks the
head and the column table. It checks the chunk's track against the definitions, and a series chunk's [[ticks]] against
its grid. It copies the caller's columns as they are and does not read them back.

A caller's whole block, handed to [`TapeWriter::write_block`], [`TapeWriter::adopt_block`] or
[`TapeWriter::defer_block`], is read once. The writer checks every [[CRC]] in it, a [[CRC-32C]]. Beyond that, each byte
placed costs one more pass: the [[content hash]]. [Chunks from columns](columns.md#what-a-chunk-costs-the-writer)
measures what these passes cost per chunk.

The writer does not compare a series chunk's grid with the chunk before it. A reader's [`SeriesTimeline`] does
([Series](series.md#grids-where-samples-are-in-time)). The writer computes each CRC with the processor's CRC
instructions where it has them, and with a table elsewhere, with the same result.

### Nothing is filled in later

A writer never goes back to change bytes it already wrote. Every block is final once placed, and the file only grows.
This rule gives the format its shape:

- The piece's length and the place of its index are in the tail at the end, not in a header at the start.
- Checkpoints point back at the checkpoint before them, never forward at one not yet written.
- A tail cannot name the next piece's hash, which does not exist yet. It names the next piece's id instead.
- A new definition is a new record. The old one stays, and only readers after the new one see it replaced.
- A state copy is written at a use, never scheduled ahead, so nothing is left pending when the writer stops.
- A crash costs only the bytes not yet synced. No header exists that would be wrong after it.

Append-only writing suits every medium a tape targets. A NAND flash page takes one program, and a SD card is fastest in
long sequential writes. A tool can also read a file whole while the file grows, since it is only appended.

## Deferred blocks

Some blocks may wait. A [[summary]] block of a series is the usual case: a [`SeriesSummaries`] or an [`EventTree`] from
`ww_tape_summary` fills one as data comes. The application hands such a block to [`TapeWriter::defer_block`], and the
writer chooses where it lands. That makes it a [[deferred block]]. The writer puts the block's definitions at once, and
where it places the block depends on the file's layout.

### Aligned files

An **aligned** file starts every chunk's sample payload on a 4 KiB boundary. A reader can then map the payloads from the
page cache or read them with direct input and output. Choose it with the layout [`Layout::ALIGNED`], or call
[`TapeWriter::set_alignment`] before the first block. In a file that is not aligned, blocks follow each other with
nothing between them, and a deferred block lands at once.

In an aligned file, the space before a payload holds zeros in the chunk's own head. The writer fills that gap with
deferred blocks while they fit. A deferred block waits at most 1 MiB of writes for a gap that fits it.

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/deferred.svg"
<figcaption>Two stretches of aligned files. In the drive's file, the next second's put of the mode places the waiting
summary at once, so the gap before the payload holds zeros. In a file of <code>phase_a</code> alone, the summary fills
the gap. Drawn by <code>ww_tape_figures</code> from the drive's first minute at full rate.</figcaption>
</figure>

Alignment costs the drive more than a series alone. The hour takes 129 MB aligned against 115 MB, 12 % more. In the
aligned drive file, 0 of the 2933 summary blocks lie in a gap. Each second starts with a put of the mode and a records
block, before the next series chunk. Every put and every block handed over places the waiting deferred blocks first,
even a put that writes nothing.

A file of `phase_a` alone takes 112 MB aligned against 104 MB, 7.3 % more. There 2724 of the 2933 summary blocks fill
gaps. Alignment therefore suits a file that holds mostly series. The drive's recorder keeps its mixed file unaligned.

## Closing

[`TapeWriter::close`] places the deferred blocks still waiting and writes the final index, the close and the tail.
[Files and syncing](files.md#the-close) follows the syncs and the rename that make a file's close durable. The call
returns a [`writer::Closed`], which holds the piece's content hash, its length and each track's next [[item number]].

A close ends a write session, not the piece. A writer that opens a closed piece again writes on after its close. The
next close names the one before it ([Closing more than once](format.md#closing-more-than-once)). A piece closed again
with nothing appended stays as it was, to the byte.

[`TapeWriter::close_with_summary`] closes with a summary block of the application's own, up to 1 MiB of any bytes. A
reader fetches it from the tail without a scan ([The close summary](format.md#the-close-summary)).
