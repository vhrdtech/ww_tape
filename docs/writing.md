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

A [`TapeWriter`] turns the application's calls into [[blocks]] of a [[piece]]. The application keeps its
[[definitions]] and hands over the data. The writer adds the blocks that the piece needs around that data. These are
[[puts]] of definitions and [[state]], [[checkpoints]], and at the end the [[final index]], [[close]] and [[tail]].

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/writer_flow.svg"
<figcaption>The application calls the writer with its definitions and its data. The writer adds puts, checkpoints and
the end of the piece, places deferred blocks, and hands its bytes to a sink.</figcaption>
</figure>

The writer puts its bytes into a [[sink]]: any type that implements [`TapeSink`]. A sink appends bytes, syncs them,
reports its size and completes the piece at the close. To resume a piece, it also reads bytes back and truncates them.
A [[chain]] needs one sink per piece, so it takes a [[store]] instead: any type that implements [`PieceStore`]. A store
makes the sink of each piece under a key.

Four of them come with the crates, two in `ww_tape` and two in `ww_tape_fs`:

| Type | Is a | Keeps |
|---|---|---|
| [`MemSink`] | sink | one piece in memory, where a [`TapeReader`] reads it while it is written |
| [`FileSink`] | sink | one piece in a file |
| [`MemPieces`] | store | every piece of a chain in memory |
| [`FilePieces`] | store | the pieces of a chain in a directory, or all in one file |

A [[raw medium]] or a network store needs a sink and a store of its own.

### Errors of sinks and stores

Each sink and store has its own error type: [`MemError`] for memory and [`FsError`] for files. A [`WriteError`]
carries that error in [`WriteError::Sink`], together with the step that failed. A store refuses to make a tape that
exists already or that another writer holds, and names the tape by its key.

### The writer's settings

The writer's settings are its [`WriterOptions`]. Each field has a default that suits a file on a disk:

| Field | What it sets | Explained in |
|---|---|---|
| `sync` | when the writer syncs | [Files and syncing](files.md#how-often-to-sync) |
| `index` | when the writer places checkpoints | [Anatomy of a piece](anatomy.md#checkpoints-while-the-piece-grows) |
| `layout` | how blocks are placed, such as payloads on 4 KiB | [Aligned files](aligned.md#aligned-files) |
| `producer` | the producer's name | [Definitions and state](definitions.md#the-definitions-an-application-holds) |
| `near` | how the writer remembers puts and spaces state copies | [Near, puts and copies](definitions.md#near-puts-and-copies) |
| `clock` | the clock that decides when an interval is up | [Files and syncing](files.md#the-writers-clock) |
| `integrity` | which checks the writer computes | [Fewer checks, sealed later](integrity.md) |

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

Only 12 of the 3600 puts wrote a record, 3 of them the puts that started pieces 1 to 3 with the mode carried. A put
writes its [[state item]] only when no record of the same item lies [[near]] behind, as [Definitions and
state](definitions.md#near-puts-and-copies) explains. The recorder therefore puts the mode before every block and
never keeps track of what it put.

### Where a block lands

The application builds a [[records block]] with a [`BlockWriter`], as [A first tape](first_tape.md#writing-the-minute)
shows, and hands the finished bytes to [`TapeWriter::write_block`]. For a [[chunk]] it hands a [[chunk head]] and its
encoded [[columns]] to [`TapeWriter::write_chunk`], which builds the chunk block around them. Each call tells where the
block landed:

| Call | Returns |
|---|---|
| [`TapeWriter::write_block`] | a [`ww_tape::Placed`]: the offset from the start of the piece and the length; `None` for a block of puts, which lands as no single block |
| [`TapeWriter::write_chunk`] | a [`ww_tape::Placed`] whose [`Placed::unit`] is the chunk's [[unit hash]] |
| [`PieceChain::write_chunk`] | a [`ChunkPlaced`]: the same fields, and the sequence number of the piece the chunk landed in |

The writer states a chunk's unit hash and does not verify it. A writer in the same process thus folds [[range
hashes]] without reading the piece back. A chain's call may [[cut]] to a new piece first, so its caller learns where the chunk
went. Through a mux, a [`RecordsHandle`] sends a block's bytes, and the mux's [`ProgressWatch`] tells the piece and
the bytes written.

The recorder encodes its chunks as the pages on codecs show:

- [Event times and their codecs](columns.md) encodes the frames of `can0`.
- [Samples and their codecs](sample_codecs.md) encodes the samples of `phase_a`.

A typed handle of a mux encodes a chunk on its producer's thread and hashes its payload there. The mux then calls
[`TapeWriter::write_chunk_hashed`], which takes the payload's hash as given instead of computing it. The bytes are
those of [`TapeWriter::write_chunk`].

## What each kind of track takes

Each kind of [[track]] takes only its own form of data, and the writer refuses any other:

| Track | Takes | The writer refuses |
|---|---|---|
| events | records blocks and [[events chunks]] | puts |
| [[state track]] | puts: [`TapeWriter::put`], or [`TapeWriter::put_key`] on a keyed one | records and events chunks |
| [[series]] | [[series chunks]], each with its [[grid]] and [[scale]] in the head | records, puts and events chunks |
| bytes | parts of a value, each an events chunk of one item that a [`BytesWriter`] makes | records, puts and other chunks |

A keyed state track holds a value per key, such as the entries of a settings store, and `delete_key` ends a key. A
bytes track holds values of any length, such as a firmware image, read whole with their hash checked. Through a mux,
a value goes in through a [`BytesHandle`], as [Writing from many threads](writing_thread.md)
shows.

The writer adds blocks of its own around this data. [What the writer adds](writer_adds.md) shows them, from the
definitions it puts to the close.
