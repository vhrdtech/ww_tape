# Writing a tape

A **producer** is the program that makes the data, such as a recorder or a logger. This page follows a producer from an
empty file to its first blocks of data.

It shows what `TapeWriter` does for the producer and what the producer does itself. It assumes the terms of [Tape
concepts](concepts.md), and [Where to go next](#where-to-go-next) lists the pages that take the story further.

## The writer and its sink

`TapeWriter` appends the producer's blocks to a piece and adds the blocks that the piece needs around them. Before a
block of data, it puts the definitions and the state that the block needs.

Between blocks, the writer places **checkpoints**: index blocks that let a reader find the blocks of an unclosed
piece. At the end, it writes the final index, the close and the tail.

A close ends a write session, not the piece. A writer that resumes a closed piece writes on after its close, and the
next close names the one before it ([Closing more than once](format.md#closing-more-than-once)).

The producer makes the data; the writer places it.

### Sinks and stores

The writer puts its bytes into a **sink**: any type that implements `TapeSink`. A sink appends bytes and syncs them.

A sink also reports its size and completes the piece at the close, and a file sink completes a piece by taking its
final name. To resume a piece, a sink also reads bytes back at an offset and truncates them.

The crates provide two sinks:

- `MemSink`, in `ww_tape`, keeps the bytes in memory, where a `TapeReader` reads them while they are written. Tests use
  it to cut a piece at any byte and check what a reader makes of it.
- `FileSink`, in the crate `ww_tape_fs`, writes a file.

A chain of pieces needs one sink per piece. It therefore takes a **store** instead: any type that implements `Tapes`,
which makes the sink of each piece under a key.

The crates provide two stores, and `ww_tape_fs::Files` lays out its files in two ways:

- `MemTapes` keeps every piece in memory under its key.
- `ww_tape_fs::Files::new(dir)` makes one file per piece in a directory: key 0 is `00000000.wwt`, key 1 is
  `00000001.wwt`, and so on.
- `ww_tape_fs::Files::file(path)` keeps every piece in one file.

Any other medium, such as a raw Secure Digital (SD) card or a network store, needs its own `TapeSink` and `Tapes`.

### Errors of sinks and stores

Each sink and store has its own error type: `MemError` for memory and `ww_tape_fs::FsError` for files. A `WriteError`
carries that error in `WriteError::Sink`, together with the step that failed.

A store refuses to make a tape that exists already or that another writer holds. `MemError<K>` names the refused tape by
its key, a value of the store's key type `K`.

The writer's settings are its `WriterOptions`:

| Field | What it sets |
|---|---|
| `sync` | the sync policy: when the writer syncs ([Syncing](syncing_and_cuts.md#syncing-what-a-crash-can-lose)) |
| `index` | the index policy: when the writer places checkpoints ([Checkpoints](syncing_and_cuts.md#checkpoints)) |
| `layout` | how blocks are placed, such as the alignment of chunk payloads ([Aligned files](#aligned-files)) |
| `producer` | the producer's name |
| `near` | the near policy: how the writer remembers puts and spaces copies ([Puts](#puts-near-and-copies)) |
| `clock` | the clock that decides when an interval is up |

The writer puts the layout and the producer at the start of every piece. They belong to the writer, and a caller's block
cannot write them.

## What the application does with its definitions

The writer keeps no table of definitions. The application keeps the definitions of what it works on in a `Definitions`
container.

The application passes the container by reference with every call that writes data. It does three things with the
container:

1. It holds each clock, clock map, bundle and track it uses, with `Definitions::define`, or with `define_all` for
   several at once. Nothing is written yet.
2. It passes the container with every write of data (`write_block`, `write_chunk`, `defer_block`) and with every put.
3. It drops a track it stops working on (`Definitions::close`), and the clocks, maps and bundles that no remaining track
   names go with it. Nothing is written for that either.

The container checks each definition against those it holds. A track or a clock map must name clocks that are held, and
a clock epoch must come with its link to the epoch before.

The writer refuses data on a track that the container does not hold. It also refuses a chunk or summary whose head gives
the track's number with another id than the track's definition (`DescribeError::TrackId`).

The writer does the rest: before every block of data, it puts the definitions that the block's tracks need. These are
the clocks, maps and bundles that a track names, then the track itself.

A put writes a definition only where the same definition does not lie near behind. Most blocks therefore cost no
definitions at all.

```rust
let mut defs = Definitions::new();
defs.define_all(&[clock, temperature_track, mode_track])?;
writer.put(&defs, MODE, ticks, &mode_bytes)?; // a state item, at its use
writer.write_block(&defs, &readings)?; // the definitions it needs go first
```

Here `clock` is a `ClockDef` and the two tracks are `TrackDef`s, all passed as `Describe` values. `readings` is a
closed records block from a `BlockWriter` ([Writing data](#writing-data)).

The repository runs these lines as a test (`ww_tape_tests/tests/it/book.rs`).

### Puts, near and copies

Each state item and each definition fills a **slot**: a state track, or the track, clock, map or bundle that a
definition defines. A put writes its item unless the newest record of the slot is the same item and lies less than the
track's `near` behind.

The writer compares items by a 128-bit hash of their bytes, made with BLAKE3 (a cryptographic hash function). It
measures the distance to where the data lands, after the puts in front of it.

A put of an item that lies near still writes a **copy** while two conditions hold:

- the piece holds fewer than `1 + copies` records of the item;
- the newest one lies at least `NearPolicy::copy_spacing` (1 MiB) behind.

The writer writes copies only at uses and never schedules them ahead.

Puts and copies go into blocks marked `FLAG_REPEAT`. Such a block carries the sequence number of the block before it and
is not counted.

The producer's sequence numbers therefore stay its own count of its blocks. Puts that belong with the block after them
are also marked `FLAG_WITH_NEXT`, which a resume uses ([Resuming after a crash](resuming.md#resuming-after-a-crash)).

The writer remembers, per slot, where it last wrote what. `NearPolicy::memory_max` caps that memory, and the writer
forgets the slot written longest ago first.

A forgotten slot costs one more write at its next put, never a wrong read. [Numbers](numbers.md#definitions-and-puts)
gives what puts and copies cost in a piece.

### How many definitions to hold

How many definitions to hold is the application's choice. `Definitions::len` says how many are held, and
`Definitions::bytes` how large they are.

For example, a recorder of 10 000 sensors holds 10 000 track definitions and passes them all with each block. A block of
3 sensors' readings gets only those 3 definitions and their clock written before it.

The writer writes those definitions once per 32 MiB, plus one [copy](#puts-near-and-copies).

A track added an hour into the recording costs one record at its first block. The application puts a typed item that
belongs to no track, such as a run record, with `TapeWriter::put_item`.

The writer refuses a caller's own block of definitions. Definitions reach the file only as puts; every definition in a
file therefore lies near the data that needs it.

## Writing data

Data goes into the file in four forms:

| Form | Call | Use it for |
|---|---|---|
| records block | `TapeWriter::write_block` | tracks that produce a few items at a time |
| chunk | `TapeWriter::write_chunk` | tracks with many items |
| deferred block | `TapeWriter::defer_block` | small blocks that may wait, such as summary blocks |
| state item | `TapeWriter::put` | the items of a state track, at each use |

### Records blocks

Build a records block with `BlockWriter`. Start it with the block's kind, its id (`TapeWriter::next_id`) and the clocks
its records use, then `push` the records and `finish` the block.

Hand the bytes to `TapeWriter::write_block`. It returns where the block landed as a `Placed`: its `offset` from the
start of the piece and its `len`.

A block of puts lands as no single block. For such a block, `write_block` returns `None`.

### Chunks

Hand a chunk head and its columns to `TapeWriter::write_chunk`. It encodes the chunk block and returns its `Placed`.

For a chunk, `Placed::unit` is the chunk's unit hash as the writer states it in the index entry. For a records block, it
is `UnitHash::NONE`.

The writer states the unit hash and does not verify it. A writer in the same process can therefore fold range hashes
without reading the piece back.

`PieceChain::write_chunk` returns a `ChunkPlaced`: the same fields plus the `seq` of the piece that the chunk landed in.
The `seq` is there because the call may cut to a new piece first.

`PieceChain::write_block` returns nothing.

### Deferred blocks

`TapeWriter::defer_block` takes small blocks that may wait. The usual case is a summary block, which the crate
`ww_tape_summary` builds: `EventTree` for an event track, `SeriesSummaries` for a series track.

The writer puts the block's definitions at once. Where it places the block depends on whether the file is **aligned**,
with every chunk's sample payload on a 4 KiB boundary ([Aligned files](#aligned-files)):

- In an aligned file, the writer places the block in the gap before a chunk's payload, at most `DEFER_WINDOW` (1 MiB) of
  writes later.
- In a file that is not aligned, the writer places the block at once.

### State items

Put a state track's item with `TapeWriter::put` each time the producer uses it. The call returns whether it wrote the
item.

### What each kind of track takes

Each kind of track takes only its own forms. A state track takes its items only through puts.

The writer therefore refuses a caller's records on a state track in a block of data, and an events chunk on a state
track.

A series track has no records. Its samples go into series chunks, and each chunk's head carries its grid and scale.

The writer refuses a record, a put or an events chunk on a series track.

### What the writer checks

The writer checks what it is handed in proportion to the cost:

- For a chunk that it encodes from columns, it checks the head and the column table.
- It checks the chunk's track against the definitions, and a series chunk's ticks against its grid.
- It copies the caller's columns as the payload and does not read them back.
- It reads a caller's whole block (`write_block`, `adopt_block`, `defer_block`) once and checks every cyclic redundancy
  check (CRC) in it.
- Beyond that, each byte placed costs one more pass: the content hash.

The writer does not compare a series chunk's grid with the chunk before it. A reader's `SeriesTimeline` does
([Series](series.md#grids-where-samples-are-in-time)).

Every CRC is a CRC-32C. The writer computes it with the CPU's CRC instructions where the CPU has them (SSE4.2 on x86-64,
the CRC extension of ARMv8).

Elsewhere the writer uses a table, with the same result.

### What a chunk costs the writer

`just bench-chunks` measures what the writer spends on a series chunk of 1 MiB, which holds 524 288 `i16` samples. The
table shows the best of 5 rounds of 256 chunks.

The bench ran on a virtual machine with 14 vCPUs on an Advanced Micro Devices (AMD) processor, an EPYC (Genoa
generation). The load average was about 5.

| What | µs per chunk | GB/s |
|---|---:|---:|
| `chunk::write_chunk`: copy and CRCs | 53 | 19.82 |
| `Block::read`: the CRCs checked | 33 | 31.32 |
| `ItemHash::of`: BLAKE3 | 213 | 4.92 |
| `TapeWriter::write_chunk` from columns | 254 | 4.12 |
| `chunk::write_chunk`, then `TapeWriter::write_block` | 281 | 3.73 |

The content hash is most of the cost. Besides the hash, a writer that hands over columns spends about 41 µs per MiB.

### Aligned files

A file that holds series is usually aligned. Choose this with the layout `Layout::ALIGNED`, or call
`TapeWriter::set_alignment(ww_tape::ALIGNMENT)` before the first block.

A reader can then map the payloads from the page cache or read them with direct I/O.

Deferred blocks fill the space before an aligned payload while they fit, and zeros in the chunk's own head fill the
rest. The summaries and checkpoints that a series produces anyway fill most gaps, so alignment costs little.

An event file is not aligned; its blocks follow each other with nothing between them.

## Where to go next

Three pages take the writer further:

- [Sinks, files and locks](sinks_and_locks.md): where the bytes go, and how one writer holds a file.
- [Syncing, cuts and relays](syncing_and_cuts.md): what a crash can lose, how a piece ends, and how a relay passes
  blocks on.
- [Resuming a writer](resuming.md): how a writer reopens a piece after a crash.
