---
level: basic
---

# Writing a tape

An application that records data hands it to a [[writer]], which turns it into the [[blocks]] of a [[tape]]. What does
the application hand over, and what does the writer add on its own? Four calls carry the data, and the writer adds the
blocks that let a [[reader]] find and understand it. The examples count the calls of the drive's recorder over an hour.

The writer serves one thread at a time. A recorder with several sources writes through a [[mux]] instead, which owns
the writer on a thread of its own. [Writing from many threads](writing_thread.md) explains it, and the drive's recorder
writes so. Each [[handle]] of the mux sends units, and the mux turns each into one call of the writer.

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

A [[chain]] needs one sink per piece, so it takes a [[store]] instead: any type that implements [`PieceStore`]. A store
makes the sink of each piece under a key. [`MemPieces`] keeps every piece in memory, and [`FilePieces`] keeps them in a
directory or in one file. A [[raw medium]] or a network store needs a sink and a store of its own.

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

The drive's recorder hands the hour to the mux through four handles. Each second it [[puts]] the mode and sends one
[[records block]] of log lines and temperatures. It also sends a [[chunk]] of CAN frames and pushes a second of phase
current. The mux turns each into one call of the writer, and adds the [[summary]] blocks of the phase current.

`cargo run -p example_drive` counts the calls:

| Handle call | Writer call | Count |
|---|---|---:|
| [`StateHandle::put`], the mode | [`TapeWriter::put`] | 3600 (12 wrote) |
| [`RecordsHandle::send`], log lines and temperatures | [`TapeWriter::write_block`] | 3600 |
| [`ChunkSender::send`], `can0` | [`TapeWriter::write_chunk`] | 3600 |
| [`GridHandle::push`], `phase_a` | [`TapeWriter::write_chunk_hashed`] | 3600 |
| none: the mux's summaries of `phase_a` | [`TapeWriter::defer_block`] | 2948 |

Only 12 of the 3600 puts wrote a record, 3 of them the puts that started pieces 1 to 3 with the mode carried. A put writes its [[state item]] only when no record of the same item lies
[[near]] behind, as [Definitions and state](definitions.md#near-puts-and-copies) explains. The recorder therefore puts
the mode before every block and never keeps track of what it put.

### Records blocks

The application builds a [[records block]] with a [`BlockWriter`], as [A first tape](first_tape.md#writing-the-minute)
shows. It hands the finished bytes to [`TapeWriter::write_block`], which returns where the block landed as a
[`ww_tape::Placed`]: its offset from the start of the piece and its length. A block of puts lands as no single block,
and for such a block the call returns `None`. Through a mux, a [`RecordsHandle`] sends the bytes, and the mux's
[`ProgressWatch`] tells the piece and the bytes written.

### Chunks

The application hands a [[chunk head]] and its encoded [[columns]] to [`TapeWriter::write_chunk`]. The writer builds the
chunk block around them and returns its [`ww_tape::Placed`]. For a chunk, [`Placed::unit`] is the chunk's [[unit hash]].
The writer states that hash and does not verify it, so a writer in the same process folds [[range hashes]] without
reading the piece back. [Event times and their codecs](columns.md) shows how the recorder encodes its frames, and
[Samples and their codecs](sample_codecs.md) its samples.

A typed handle of a mux encodes the chunk on its producer's thread and hashes its payload there. The mux then calls
[`TapeWriter::write_chunk_hashed`], which takes the payload's hash as given instead of computing it. The bytes are
those of [`TapeWriter::write_chunk`].

A [`PieceChain`] writes a chunk with [`PieceChain::write_chunk`], which returns a [`ChunkPlaced`]. It holds the same
fields and the sequence number of the piece that the chunk landed in. The call may [[cut]] to a new piece first, so the
caller learns where the chunk went.

### What each kind of track takes

Each kind of [[track]] takes only its own form of data. A [[state track]] takes its items only through puts. The writer
therefore refuses a caller's record or [[events chunk]] on a state track. A [[series]] has no records: its [[samples]]
go into [[series chunks]], each with its [[grid]] and [[scale]] in the head. The writer refuses a record, a put or an
events chunk on a series track.

## Definitions with every call

The writer keeps no table of definitions. Whoever drives it holds them in a [`struct@Definitions`] container and passes it
by reference with every write of data and every put. A mux holds the container itself, so its handles never name a
definition. A program that drives the writer directly passes it. Before each block, the writer puts the [[definitions]] that the
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
a description of the run, goes in with [`TapeWriter::put_item`]. The writer refuses a block of definitions that the caller wrote, so
every definition in a file lies near the data that needs it. A track that gets no data yet goes in with
[`TapeWriter::put_definitions`]: its definition and what it needs, by the same rule.

## What the writer adds

The writer's own blocks hold what a [[reader]] needs to find and understand the data. For the drive's hour they are the
[[piece heads]], the puts, the checkpoints and the end of each piece:

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

The writer's own blocks take 3.4 MB of the hour's 116 MB, 2.9 %. Most of that is index: the checkpoints while the pieces
grow, and each piece's final index. The 32 blocks of puts hold every definition and mode change of four pieces in 5.5
kB.

### What the writer checks

The writer checks what it is handed in proportion to the cost. For a chunk that it builds from columns, it checks the
head and the column table. It checks the chunk's track against the definitions, and a series chunk's [[ticks]] against
its grid. It copies the caller's columns as they are and does not read them back.

A caller's whole block, handed to [`TapeWriter::write_block`], [`TapeWriter::adopt_block`] or
[`TapeWriter::defer_block`], is read once. The writer checks every [[CRC]] in it, a [[CRC-32C]]. Beyond that, each byte
placed costs one more pass: the [[content hash]]. [Samples and their codecs](sample_codecs.md#what-a-chunk-costs-the-writer)
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
`ww_tape_summary` fills one as data comes. The mux, or an application that drives the writer itself, hands such a
block to [`TapeWriter::defer_block`], and the writer chooses where it lands. That makes it a [[deferred block]]. The writer puts the block's definitions at once, and
where it places the block depends on the file's layout.

### Aligned files

Disks, the page cache and direct input and output all work in blocks of 4 KiB, or in sectors that divide that size.
A 4 KiB boundary is an offset in the file that 4096 divides. An **aligned** file starts every chunk's payload, the
column bytes after its head, on such a boundary. Its samples then begin at the start of a page of the
page cache and of a sector on the disk.

A reader gains three things from that:

- **Memory maps.** A reader maps the file and uses a chunk's samples in place, as a slice that starts on a page. A
  vector kernel or a copy engine takes it without moving a byte.
- **Direct input and output.** Reads that bypass the page cache, such as `O_DIRECT` on Linux, need offsets and lengths
  in whole blocks. A payload on a boundary reads that way with no bytes before it.
- **Raw media.** On a card or a disk without a file system, a payload on a boundary covers whole sectors. Reading one
  chunk's samples then reads no sector of the block before it.

Choose it with the layout [`Layout::ALIGNED`], or call [`TapeWriter::set_alignment`] before the first block. In a file
that is not aligned, blocks follow each other with nothing between them, and a deferred block lands at once.

In an aligned file, the space before a payload holds zeros in the chunk's own head. The writer fills that gap with
deferred blocks while they fit. A deferred block waits at most 1 MiB of writes for a gap that fits it.

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/deferred.svg"
<figcaption>One second of samples in three files, each row from the end of one series chunk to the end of the next.
Not aligned, the payload starts wherever the head ends. Aligned, zeros in the chunk's head move the payload to the
next 4 KiB boundary. In a file of <code>phase_a</code> alone, the waiting summary fills part of that gap. In the
drive's file, the next second's put of the mode places the summary first, so the gap holds zeros only. Drawn by
<code>ww_tape_figures</code> from the drive's first minute at full rate.</figcaption>
</figure>

Alignment costs the drive more than a series alone. The hour takes 129 MB aligned against 116 MB, 12 % more. In the
aligned drive file, 0 of the 2948 summary blocks lie in a gap. Each second starts with a put of the mode and a records
block, before the next series chunk. Every put and every block handed over places the waiting deferred blocks first,
even a put that writes nothing.

A file of `phase_a` alone takes 112 MB aligned against 105 MB, 7.3 % more. There 2724 of the 2933 summary blocks fill
gaps. Alignment therefore suits a file that holds mostly series and is read by memory maps or direct input and
output. Alignment is optional: where space matters more than such reads, leave the file unaligned. The drive's
recorder keeps its mixed file unaligned.

## Closing

[`TapeWriter::close`] places the deferred blocks still waiting and writes the final index, the close and the tail.
[Files and syncing](files.md#the-close) follows the syncs that make a file's close durable. The call
returns a [`writer::Closed`], which holds the piece's content hash, its length and each track's next [[item number]].

A close ends a write session, not the piece. A writer that opens a closed piece again writes on after its close. The
next close names the one before it ([Closing more than once](format.md#closing-more-than-once)). A piece closed again
with nothing appended stays as it was, to the byte.

[`TapeWriter::close_with_summary`] closes with a summary block of the application's own, up to 1 MiB of any bytes. A
reader fetches it from the tail without a scan ([The close summary](format.md#the-close-summary)).
