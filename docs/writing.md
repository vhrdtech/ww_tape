# Writing a tape

This page follows a producer from an empty file to a finished piece, then through a crash and back. A **producer** is
the program that makes the data, such as a recorder or a logger. The page explains what `TapeWriter` does for the
producer and what the producer does itself. It assumes the terms of [Tape concepts](concepts.md).

## The writer and its sink

`TapeWriter` appends the producer's blocks to a piece and adds the blocks that the piece needs around them. Before a
block of data it puts the definitions and the state that the block needs. Between blocks it places checkpoints, and at
the end it writes the final index, the finish and the tail. The producer makes the data; the writer places it.

The writer puts its bytes into a **sink**, which is any type that implements `TapeSink`. A sink appends bytes, syncs
them, reports its size and completes the piece at the finish; a file completes by taking its final name. To resume a
piece, a sink also reads bytes back at an offset and truncates them. The crates provide two sinks:

- `MemSink`, in `ww_tape`, keeps the bytes in memory, where a `TapeReader` reads them while they are written. Tests use
  it to cut a piece at any byte and check what a reader makes of it.
- `FileSink`, in the crate `ww_tape_fs`, writes a file.

A chain of pieces needs one sink per piece, so it takes a **store** instead: any type that implements `Tapes`. A store
makes the sink of each piece under a key. `MemTapes` keeps every piece in memory under its key. `ww_tape_fs::Files`
keeps the pieces in files: `Files::new(dir)` makes one file per piece in a directory (key 0 is `00000000.wwt`, key 1
is `00000001.wwt`, and so on), and `Files::file(path)` keeps every piece in one file. Any other medium, such as a raw
SD card or a network store, needs its own `TapeSink` and `Tapes`.

Each sink and store has its own error type: `MemError` for memory and `ww_tape_fs::FsError` for files. A `WriteError`
carries that error in `WriteError::Sink`, together with the step that failed. A store refuses to make a tape that
exists already or that another writer holds. `MemError<K>` names the refused tape by its key, a value of the store's
key type `K`.

The writer's settings are its `WriterOptions`:

| Field | What it sets |
|---|---|
| `sync` | the sync policy: when the writer syncs ([Syncing](#syncing-what-a-crash-can-lose)) |
| `index` | the index policy: when the writer places checkpoints ([Checkpoints](#checkpoints)) |
| `layout` | how blocks are placed, such as the alignment of chunk payloads ([Aligned files](#aligned-files)) |
| `producer` | the producer's name |
| `near` | the near policy: how the writer remembers puts and spaces copies ([Puts](#puts-near-and-copies)) |
| `clock` | the clock that decides when an interval is up |

The writer puts the layout and the producer at the start of every piece. They belong to the writer, and a caller's
block cannot write them.

### Writing to files

Everything that names a path lives in `ww_tape_fs`, so `ww_tape` itself builds without a file system. The module
`ww_tape_fs::writer` opens a writer on a path:

- `writer::create(path, sync)` creates the file as `NAME.partial` and renames it to `NAME` at the finish. It refuses a
  `NAME` that exists and a `NAME.partial` that a crash left with bytes in it (`FsError::Exists`). It never empties a
  file: reopen such a piece with `writer::resume`, or remove it. When `NAME.partial` is renamed or replaced between
  its open and its lock, `create` refuses too (`FsError::Replaced`).
- `writer::append(path, sync)` appends to a file in a new session, and creates the file when it does not exist.
- `writer::resume(path, options)` reopens the unfinished piece of `path` ([Resuming after a
  crash](#resuming-after-a-crash)). With neither `NAME` nor `NAME.partial` there, it fails with `FsError::NoPiece`.
- `writer::cut(writer, next_path)` ends the piece and starts the next one in the new file `next_path`
  ([Cuts](#cuts-ending-one-piece-and-starting-the-next)).

The module `ww_tape_fs::chain` opens a chain of pieces in a directory, or in one file, with `chain::create` and
`chain::resume`. Each of these functions only makes the sink or the store and hands it to a constructor of `ww_tape`:
`TapeWriter::create_on`, `append_on`, `resume_on` and `cut_with`, or `PieceChain::create_on` and `resume_on`. A sink
of any other kind uses the same constructors.

### One writer per file

Two writers appending to one file would interleave their blocks into a file that reads as damaged. A file sink
therefore holds a lock on `NAME.lock`, a file beside `NAME`, and a second sink of the same file is refused with
`FsError::Locked`. The refusal holds for a second writer in the same process and in another process. A name of 251
bytes or more leaves no room for the `.lock` suffix, so its lock file is `HASH.lock`, where `HASH` is the BLAKE3 hash
of the name in hex.

Only writers open the lock file. Readers open and close `NAME` as they like, and never release the lock by doing so.
On unix the lock is a POSIX record lock, which a child process forked meanwhile does not hold, so a resume right after
a writer is dropped is never refused because of a child. On unix the writer removes the lock file when it lets go. On
Windows the lock file stays, since removing it could race the next holder. A lock file left behind, by Windows or by a
crash, holds no lock, and the next writer takes it as it is.

One file has one lock, whatever name reaches it. The lock file is named after the file's canonical name, with links
resolved and `.partial` dropped. On unix, writers of one process are also kept apart by the file's device and inode,
so a second writer through a hard link is refused as well. Two processes that write one file through two hard-linked
names are not refused, and their blocks interleave. A device, such as a raw SD card, is locked itself. A FIFO, a
socket or a directory is refused (`FsError::NotAFile`).

An error states the step, the file and the kind of failure in its first line, for example `creating a writer:
/data/x.wwt: locked by another writer`. A log line of that text alone therefore says what went wrong. When the
operating system reported the error, such as a full disk or a missing directory, the system's error is the source of
that first error and is not repeated in its text.

### Writing to memory

A `MemSink` of a chain reserves the room of a whole piece when the piece starts: the chain's cut size
(`CutPolicy::bytes`) plus `CUT_SLACK` (1 MiB). A reader's `read_at` therefore never waits while the bytes are copied
into a bigger buffer.

`MemTapes::remove(key)` takes a finished piece out of the store, for example to keep a bounded window of a live
chain. A reader that already holds the piece's `MemTape` reads on unchanged. The piece still being written is not
removed (`MemError::Unfinished`). What remains of the chain reads as a head cut, and each remaining piece still reads
alone, since it holds the definitions and state it uses. A chain with removed pieces cannot be resumed, since a
resume starts from the first piece.

### Without the standard library

The writer, the chain and resume need only an allocator, so firmware or a browser can write tapes on a sink of its
own. Without the standard library, two defaults change:

- A new writer has no session, because it has no random source. It refuses to write until `TapeWriter::set_session`
  gives it one, for example `SessionTag::from_seed` over a device id or a boot counter, or until a piece head does
  (`begin_piece`, and every piece of a chain).
- A new writer has no clock, so no sync, checkpoint or cut falls due by time until `WriterOptions::clock` gives it
  one.

## What the application does with its definitions

The writer keeps no table of definitions. The application keeps the definitions of what it works on in a
`Definitions` container, and passes the container by reference with every call that writes data. The application
does three things with it:

1. It holds each clock, clock map, bundle and track it uses, with `Definitions::define`, or with `define_all` for
   several at once. The container checks each definition against those it holds: a track or a clock map must name
   clocks that are held, and a clock epoch must come with its link to the epoch before. Nothing is written yet.
2. It passes the container with every write of data (`write_block`, `write_chunk`, `defer_block`) and with every put.
   The writer refuses data on a track that the container does not hold. It also refuses a chunk or summary whose head
   gives the track's number with another id than the track's definition (`DescribeError::TrackId`).
3. It drops a track it stops working on with `Definitions::close`. The clocks, maps and bundles that no remaining
   track names go with it. Nothing is written for that either.

The writer does the rest. Before every block of data it puts the definitions that the block's tracks need: the clocks,
maps and bundles a track names, then the track itself. A put writes a definition only where the same definition does
not lie near behind, so most blocks cost no definitions at all.

```rust
let mut defs = Definitions::new();
defs.define_all(&[clock, temperature_track, mode_track])?;
writer.put(&defs, MODE, ticks, &mode_bytes)?; // a state item, at its use
writer.write_block(&defs, &readings)?; // the definitions it needs go first
```

Here `clock` is a `ClockDef` and the two tracks are `TrackDef`s, all passed as `Describe` values. `readings` is a
finished records block from a `BlockWriter` ([Writing data](#writing-data)). The repository runs these lines as a test
(`ww_tape_tests/tests/it/book.rs`).

How many definitions to hold is the application's choice. `Definitions::len` says how many are held, and
`Definitions::bytes` how large they are. For example, a recorder of 10 000 sensors holds 10 000 track definitions and
passes them all with each block. A block of 3 sensors' readings gets only those 3 definitions and their clock written
before it, once per 32 MiB plus one copy. A track added an hour into the recording costs one record at its first
block.

A typed item that belongs to no track, such as a run record, is put with `TapeWriter::put_item`. The writer refuses a
caller's own block of definitions. Definitions reach the file only as puts, so every definition in a file lies near
the data that needs it.

## Writing data

Data goes into the file in four forms:

| Form | Call | Use it for |
|---|---|---|
| records block | `TapeWriter::write_block` | tracks that produce a few items at a time |
| chunk | `TapeWriter::write_chunk` | tracks with many items |
| deferred block | `TapeWriter::defer_block` | small blocks that may wait, such as summary blocks |
| state item | `TapeWriter::put` | the items of a state track, at each use |

**Records blocks.** Build a records block with `BlockWriter`. Start it with the block's kind, its id
(`TapeWriter::next_id`) and the clocks its records use, `push` the records, then `finish` it. Hand the bytes to
`TapeWriter::write_block`, which returns where the block landed as a `Placed`: its `offset` from the start of the
piece and its `len`. A block of puts lands as no single block, so for such a block `write_block` returns `None`.

**Chunks.** Hand a chunk head and its columns to `TapeWriter::write_chunk`, which encodes the chunk block and returns
its `Placed`. For a chunk, `Placed::unit` is the chunk's unit hash as the writer states it in the index entry; for a
records block it is `UnitHash::NONE`. The writer states the unit hash and does not verify it, so a writer in the same
process can fold range hashes without reading the piece back. `PieceChain::write_chunk` returns a `ChunkPlaced`: the
same fields plus the `seq` of the piece that the chunk landed in, since the call may cut to a new piece first.
`PieceChain::write_block` returns nothing.

**Deferred blocks.** Hand small blocks that may wait to `TapeWriter::defer_block`. The usual case is a summary block,
which the crate `ww_tape_summary` builds: `EventTree` for an event track, `SeriesSummaries` for a series track. The
writer puts the block's definitions at once. In an aligned file it places the block in the gap before a chunk's
payload, at most `DEFER_WINDOW` (1 MiB) of writes later. In a file that is not aligned it places the block at once.

**State items.** Put a state track's item with `TapeWriter::put` each time the producer uses it. The call returns
whether it wrote the item.

Each kind of track takes only its own forms. A state track takes its items only through puts: the writer refuses a
caller's records on a state track in a block of data, and an events chunk on a state track. A series track has no
records. Its samples go into series chunks, each chunk's head carrying its grid and scale, and the writer refuses a
record, a put or an events chunk on a series track.

The writer checks what it is handed in proportion to the cost. For a chunk that it encodes from columns, it checks
the head and the column table, the track against the definitions, and a series chunk's ticks against its grid. The
payload is the caller's columns copied, and the writer does not read it back. A caller's whole block (`write_block`,
`adopt_block`, `defer_block`) is read once with every CRC checked. Beyond that, each byte placed costs one more pass:
the content hash. The writer does not compare a series chunk's grid with the chunk before it; a reader's
`SeriesTimeline` does ([Events and series](events_and_series.md#grids-where-samples-are-in-time)).

Every CRC is a CRC-32C. The writer computes it with the CPU's CRC instructions where the CPU has them (SSE4.2 on
x86-64, the CRC extension of ARMv8), and with a table elsewhere, with the same result.

`just bench-chunks` measures what the writer spends on a series chunk of 1 MiB, which holds 524 288 `i16` samples.
The table shows the best of 5 rounds of 256 chunks. It was measured on an AMD EPYC Genoa virtual machine with 14
vCPUs, at a load average of about 5.

| What | µs per chunk | GB/s |
|---|---:|---:|
| `chunk::write_chunk`: copy and CRCs | 53 | 19.82 |
| `Block::read`: the CRCs checked | 33 | 31.32 |
| `ItemHash::of`: BLAKE3 | 213 | 4.92 |
| `TapeWriter::write_chunk` from columns | 254 | 4.12 |
| `chunk::write_chunk`, then `TapeWriter::write_block` | 281 | 3.73 |

The content hash is most of the cost. Besides the hash, a writer that hands over columns spends about 41 µs per MiB.

### Puts, near and copies

Each state item and each definition fills a **slot**: a state track, or the track, clock, map or bundle that a
definition defines. A put writes its item unless the newest record of the slot is the same item and lies less than
the track's `near` behind. The writer compares items by their 128-bit BLAKE3 hash, and it measures the distance to
where the data lands, after the puts in front of it.

A put of an item that lies near still writes a **copy** while two conditions hold: the piece holds fewer than
`1 + copies` records of the item, and the newest one lies at least `NearPolicy::copy_spacing` (1 MiB) behind. The
writer writes copies only at uses and never schedules them ahead.

Puts and copies go into blocks marked `FLAG_REPEAT`. Such a block carries the sequence number of the block before it
and is not counted, so the producer's sequence numbers stay its own count of its blocks. Puts that belong with the
block after them are also marked `FLAG_WITH_NEXT`, which a resume uses ([Resuming after a
crash](#resuming-after-a-crash)).

The writer remembers, per slot, where it last wrote what. That memory is capped by `NearPolicy::memory_max`, and the
writer forgets the slot written longest ago first. A forgotten slot costs one more write at its next put, never a wrong
read. [Numbers](numbers.md#definitions-and-puts) gives what puts and copies cost in a piece.

### Aligned files

A file that holds series is usually **aligned**: every chunk's sample payload starts on a 4 KiB boundary. Choose this
with the layout `Layout::ALIGNED`, or call `TapeWriter::set_alignment(ww_tape::ALIGNMENT)` before the first block. A
reader can then map the payloads from the page cache or read them with direct I/O.

Deferred blocks fill the space before an aligned payload while they fit, and zeros in the chunk's own head fill the
rest. The summaries and checkpoints that a series produces anyway fill most gaps, so alignment costs little. An event
file is not aligned; its blocks follow each other with nothing between them.

## Syncing: what a crash can lose

Data that the operating system has not yet written to the disk is lost in a power cut. The **sync policy**
(`SyncPolicy`) bounds how much that can be:

| `SyncPolicy` | Syncs | A crash loses at most |
|---|---|---|
| `Interval(t)`, the default with `t` = 1 s | when blocks wait and the last sync is `t` ago | about `t` of data |
| `EveryBlocks(n)` | after every `n` blocks | `n` blocks |
| `None` | only at the finish | everything the system has not written yet |

`Interval` acts only when a block arrives. A producer whose input may stop for a while calls `TapeWriter::tick` now
and then, so the last blocks are synced even when no new block comes. With `IndexPolicy::interval` set, `tick` also
places a checkpoint when its interval is up. In an unaligned file the checkpoint goes in at once. In an aligned file
`tick` only marks the checkpoint due, and it goes into the gap before the next chunk. `TapeWriter::sync` syncs at
once.

The writer reads time only through its clock, `WriterOptions::clock` (or `TapeWriter::set_clock`). With `std` the
default is the system's monotonic clock, `ClockRef::SYSTEM` (a `SystemClock`), so two default options compare equal.
Without `std` the default is `ClockRef::NONE`: no interval is ever due, and only the block and byte limits apply.
Firmware passes its own `Clock`, and a test passes a clock it moves by hand. Time decides only when the writer syncs,
places a checkpoint or cuts; no reading of the clock is written into a block.

A failed write or sync ends the file. The writer reports the failure and refuses every later write. It never retries,
because after a failed `fsync` Linux may drop the pages it could not write, and a second `fsync` then succeeds without
them.

## Checkpoints

A reader of a file that was never finished has no tail to start from. The writer therefore places a **checkpoint**, an
index block, after about every 4 MiB of blocks (`IndexPolicy::bytes`). Each checkpoint lists the blocks placed since
the one before and points back at it. After a crash, a reader finds the newest checkpoint and scans only the stretch
after it, at most about 4 MiB.

The writer places checkpoints by bytes, not by time, because a place chosen by the clock would make the file's bytes
depend on when the file was written. A producer that wants checkpoints by time as well sets `IndexPolicy::interval`.
In an aligned file such a checkpoint waits for the gap before the next chunk payload.

## Cuts: ending one piece and starting the next

A **cut** ends the current piece and starts the next piece of the chain. Only a piece of a chain can be cut. A writer
starts such a piece with `TapeWriter::begin_piece`, which writes the piece head as the first block of the file.

`TapeWriter::cut_with(tapes, key)` makes a cut in four steps:

1. It places every deferred block.
2. It writes the final index, the finish and the tail.
3. It finishes the sink; a file takes its final name.
4. It starts the next piece in a new sink, which the store `tapes` makes under `key`.

The call returns the finished piece (`Finished`) and a writer on the next piece. `TapeWriter::cut_on(next)` does the
same on a sink `next` that the caller made, and `ww_tape_fs::writer::cut(writer, next_path)` does it with the file
`next_path`. The next piece starts with a piece head, the layout and the producer. Its definitions and state are put
again at their first uses, since near is measured inside a piece.

`PieceChain` writes a whole chain and cuts by itself, by a `CutPolicy` with three limits:

- `bytes`: the size of the piece;
- `chunks`: the number of chunk blocks in the piece;
- `interval`: the age of the piece, counted from its start or its resume.

A limit of 0 or `None` is off, and the first limit reached cuts. The chain checks the policy before each chunk, so a
cut by policy never comes after the last chunk and never leaves a piece without data. The chain also cuts before a
write that would need more state entries than the piece's index may hold (`IndexPolicy::state_entries_max`).
`PieceChain::cut_now` cuts at once.

Two limits on tracks are not met by a cut. A piece takes data of at most `TRACKS_PER_PIECE_MAX` tracks, and a piece
head carries at most `HEAD_TRACKS_MAX` tracks from the pieces before it, so that each finish and each head fits one
block. The writer refuses a write past either limit (`InputError::TooManyTracks`, `InputError::TooManyCarried`).

A chain writes each piece into a tape of its own, or, with `ChainOptions::tapes` set to `PieceTapes::One`, every piece
into one tape. `TapeWriter::cut_in_place` is the cut of a writer into one file: the next piece starts right behind the
finished one in the same sink, and the file keeps its name. A sink that holds one piece only, such as a
`NAME.partial` file, refuses this cut. [Pieces and files](pieces_and_files.md#writing-a-chain-into-one-file)
explains when to choose which.

Nothing waits for a cut. Durability comes from the sync policy and the checkpoints, which work inside a piece; a cut
only bounds how large one file grows. For example, a slow input of one log line a minute is written in a records block
as it comes, synced within the sync interval, and readable at once by a reader of the unfinished piece. Cutting by age
serves retention by file, and it makes the file's bytes depend on the clock.

## Finishing

`TapeWriter::finish` ends the piece as complete, in three steps:

1. It places the deferred blocks.
2. It writes the final index, then the finish block, padded so that the piece ends on a 4 KiB boundary, then the
   tail.
3. It syncs the file, renames `NAME.partial` to `NAME` and syncs the directory.

It returns a `Finished`: the piece's content hash, its length, its ids, where it starts in its file, and the hash and
next item number of each track. `finish_as` ends the piece in another state, such as aborted. A file still named
`NAME.partial` was never finished.

## Relays

A **relay** is a program that takes a live producer's blocks and writes them into a tape of its own. For example, a
host receives a device's tape blocks over a link and records them to disk, or a capture tool reads a live feed and
keeps a file of it. The producer's stream carries its own puts and copies, placed by the producer's near rule in the
producer's stream. They do not fit the relay's file, which may start later, be cut elsewhere, or have lost blocks.

`TapeWriter::relay_block(&mut defs, block)` passes such a stream on, one block at a time. A block of puts is marked
`FLAG_REPEAT`: the relay takes its definitions into its own `Definitions` and puts its state records on its own tape
by its own near rule (`TapeWriter::put_block`). The relay writes any other block as data under those definitions. It
therefore needs no knowledge of the producer's tracks, and its file has every definition near its use, like any other
file.

On a raw medium a piece must be one session, as [Damage and recovery](recovery.md#sessions-and-stale-data) explains.
A writer for raw media passes a producer's blocks with `TapeWriter::adopt_block`. The call renumbers each block into
the writer's own session and returns where it landed (`Placed`), or `None` for a block of puts. A jump in the
producer's sequence numbers stays a jump in the writer's, so a reader still sees that blocks were lost on the way.

## Resuming after a crash

`TapeWriter::resume_on(sink, options)` reopens the unfinished piece in a sink and goes on writing the same piece. For
a file, `ww_tape_fs::writer::resume(path, options)` opens the file `NAME.partial` that `writer::create` wrote and
resumes it. The call returns the writer and a `Resumed`, which says how many bytes it kept (`kept_bytes`) and dropped
(`dropped_bytes`) and where the reader stopped taking blocks (`cut`). A resume works in three steps:

1. It finds the piece as a reader would: by its tail when it has one, else from the newest checkpoint and a scan after
   it, else by a scan from the start. A scan stops at the first bad block.
2. It keeps every byte up to the end of the last good block and cuts off the rest. The rest is a torn block, an end of
   piece, and blocks marked `FLAG_WITH_NEXT` at the end of what it keeps, since the block they belong with did not
   survive.
3. It replays the kept bytes to rebuild the writer as it was: the content hash, the index entries, the track ranges,
   the record of what lies near, the session and the next sequence number. It walks the bytes as a reader's verify
   does. It takes a good block wherever the walk meets one. In bytes that no block explains, a block that the index
   lists is kept by its entry as a damaged block when it ends before the next good block; other bytes are junk, kept
   and never listed.

A damaged block kept by its entry stands for what the entry says. A damaged chunk goes into its track's hash by the
unit hash of its entry. The records of a damaged records block cannot, so the finish marks the hash of each track that
may hold records as incomplete.

Step 2 cuts off only a torn end. Where cutting could lose more, the resume refuses and leaves the bytes as they are:

| Refusal | When |
|---|---|
| `ResumeError::GoodBlockAfterCut` | a good block of the piece's sessions lies behind the bytes the reader did not take; with no block kept, a good block of any session counts |
| `ResumeError::LookSpent` | the look for good blocks ran out before the end of the bytes: it reaches at most `RESUME_LOOK_MAX` bytes far and reads at most `RESUME_PROBES_MAX` places whole, so it cannot tell that the rest holds no good block |
| `ResumeError::HashWalkSpent` | the kept bytes claim so many blocks that do not read that a reader's content-hash walk stops trying blocks before their end (`WALK_MISS_PER_BYTE_MAX`), so a block written after them would not verify |

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/resume.svg"
<figcaption>What a resume keeps. The puts at the end belong with the block of data that was torn, so they go with it;
the producer gives that call again, and it writes the same puts and the same block.</figcaption>
</figure>

The producer then gives again every call from the block that `TapeWriter::next_seq` reports, with the same
definitions. A call that placed several counted blocks, such as a chunk with deferred blocks in its gap, is given again
whole. What the writer adds depends only on the bytes before it, the calls and the options. The resumed piece
therefore ends with the same bytes, and the same content hash, as a piece written without the crash.

A put is idempotent, so the producer simply puts again what it uses and needs no record of which puts reached the
disk. The writer holds no definitions, so a resume needs none from the file: the application passes its own, as
before the crash.

A resume checks the options against the piece before it cuts or writes anything. Pass the options that the piece was
written with, which `TapeWriter::options` returns. The resume refuses a layout or a producer other than the newest one
the piece kept (`InputError::Layout`, `InputError::Producer`), because the writer would place blocks other than the
way the file says. Two cases at the end of a piece differ:

- A piece whose finish and tail read whole is complete; the crash came after the finish and before the rename. The
  resume cuts off the junk a crash left after the tail, renames the file, writes nothing and returns
  `WriteError::Ended`. A good block after the tail refuses the resume (`ResumeError::GoodBlockAfterCut`).
- A piece cut off inside its end is opened again. Its final index and finish are cut off, and the next finish pads the
  piece to 4 KiB again.

### Resuming a chain

`PieceChain::resume_on` (`ww_tape_fs::chain::resume` for files) does the same for a chain. It reads the finished
pieces back from their tails and then deals with the last piece. A finished piece that a crash left as `NAME.partial`
gets its name. What happens next depends on how the last piece ended:

| Last piece | Chain of a tape per piece | Chain in one tape (`PieceTapes::One`) |
|---|---|---|
| unfinished | resumed | resumed |
| finished as continued: the crash came during a cut | the next piece starts | the next piece starts |
| finished as complete or aborted | refused: `WriteError::Ended` | a new piece starts behind it |
| its piece head is damaged | refused | refused |

A chain in one tape therefore gains a piece each time a session opens the file. A last piece with a damaged piece
head is refused because the blocks after the head would form a piece with no link to the chain. [Damage and
recovery](recovery.md#resuming-after-a-crash) follows a recorder through a power cut.

A chain resume also restores the newest value of each monotone relation, the per-item values that a track declares
never to decrease. It takes the range relation's newest value from every finished piece's index entries, which it
reads anyway. The other relations' values are in chunks. For each track the resume reads one chunk: the track's last
chunk in the newest finished piece that holds one, at most `MAX_BLOCK_LEN` (64 MiB). A resume of a chain of N tracks
therefore reads up to N chunks besides each piece's head, finish, tail and index.

Where that chunk does not read, the writer refuses the track's next chunk (`DescribeError::RelationUnread`). The
writer cannot vouch for an order it cannot read, so only the caller can go on. The call
`TapeWriter::accept_relation_break(defs, track, relation, from)` checks the track's next chunk against `from` for that
relation and marks it as a [relation break](format.md#tape-blocks) (`FLAG_RELATION_BREAK`), which readers see. The
break waits for the track's next chunk, also across cuts. A crash before that chunk drops the break, and the resumed
writer refuses again. The writer marks a chunk that it encodes itself. A caller's block carries the flag
(`mark_relation_break`) exactly when a break waits, and is refused otherwise (`InputError::Flags`).

## Same input, same bytes

A writer given the same input writes the same bytes. Blocks hold no wall time, puts and copies follow from the bytes
before them, and checkpoints and cuts are placed by bytes. A producer that seals blocks on a timer, adds checkpoints
by time or cuts pieces by age gives up this property for what it places that way.
[Tapes](index.md#same-input-same-bytes) says what the property is good for.
