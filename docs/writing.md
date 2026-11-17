---
level: advanced
---

# Writing a tape

This page follows a producer from an empty file to a finished piece, and through a crash and back. It explains what
`TapeWriter` does for the producer and what the producer must do itself. It assumes the words of [Tape
concepts](concepts.md).

## The writer and its sink

`TapeWriter` appends the producer's blocks and adds the blocks a piece needs around them: the definitions and state each
block needs, checkpoints, the final index, the finish and the tail. The producer makes the data; the writer places it.

The writer puts its bytes into a **sink**. Two sinks come with `ww_tape`:

- `FileSink`, the default, writes a file. `TapeWriter::create(path, sync)` creates it as `NAME.partial` and renames it
  to `NAME` at the finish. `TapeWriter::append(path, sync)` appends to a file that is already there, in a new session.
- `MemSink` keeps the bytes in memory, where a `TapeReader` reads them while they are written. Tests use it to cut a
  piece at any byte and check what a reader makes of it.

A chain of pieces takes a **sink factory** instead, which makes the sink of each piece: `Files` makes files in a
directory, and `MemTapes` keeps every piece in memory under its name. Any other place, such as a raw SD card or a
network store, needs its own `TapeSink`: a sink appends, syncs, reports its size, and for a resume reads back at an
offset and truncates.

The writer's settings are its `WriterOptions`: the sync policy, the index policy, the layout (how blocks are placed),
the producer's name and the near policy. The writer puts the layout and the producer at the start of every piece. They
are the writer's own, and a caller can't write them.

## What the application does with its definitions

The writer keeps no table of definitions. The application keeps the definitions of what it works on in a `Definitions`
container and passes it, by reference, with every call that writes data. It does three things:

1. It holds each clock, clock map, bundle and track it uses with `Definitions::define`, or several at once with
   `define_all`. The container checks each definition against those it holds: a track must name a clock held, and a
   clock epoch must come with its link to the epoch before. Nothing is written yet.
2. It passes the container with every write of data (`write_block`, `write_chunk`, `defer_block`) and every put. Data on
   a track the container doesn't hold is refused.
3. It drops a track it stops working on with `Definitions::close`. The clocks, maps and bundles no held track names go
   with it. Nothing is written for that either.

The writer does the rest. Before every block of data it puts the definitions the block's tracks need: the clocks, maps
and bundles a track names, then the track itself. Each put writes only where the same definition doesn't lie near
behind, so a block usually costs no definitions at all.

```rust
let mut defs = Definitions::new();
defs.define_all(&[clock, temperature_track, mode_track])?;
writer.put(&defs, MODE, ticks, &mode_bytes)?; // a state item, at its use
writer.write_block(&defs, &readings)?; // the definitions it needs go first
```

How many definitions to hold is the application's choice; `Definitions::len` and `Definitions::bytes` say how much is
held. For example, a recorder of 10 000 sensors holds 10 000 track definitions and passes them with each block. A block
of 3 sensors' readings gets those 3 definitions and their clock written before it, and only once per 32 MiB. A track
added an hour in costs one record at its first block. A typed item that isn't a track's, such as a run record, is put
with `TapeWriter::put_item`.

A caller's own block of definitions is refused. Definitions reach the file only as puts, so every definition in a file
lies near the data that needs it.

## Writing data

Data goes into the file in four forms:

- **Record blocks.** Build one with `BlockWriter`: start it with the block's kind, its id (`TapeWriter::next_id`) and
  the clocks its records use, `push` records, then `finish` it and hand the bytes to `TapeWriter::write_block`. Use
  record blocks for tracks that produce a few items at a time.
- **Chunks.** Hand a chunk head and its columns to `TapeWriter::write_chunk`, which encodes the chunk block. Use chunks
  for tracks with many items.
- **Deferred blocks.** Hand small blocks that may wait, such as summary blocks, to `TapeWriter::defer_block`. The writer
  puts their definitions at once and places the block in the next gap, at most `DEFER_WINDOW` (1 MiB) of writes later.
- **State items.** Put a state track's item with `TapeWriter::put` each time the producer uses it.

A state track takes its items only through puts. A caller's records on a state track in a block of data are refused, and
so is an events chunk on a state track. A series track is the one state track with chunks: its series segments are put,
and its samples go into series chunks as data.

### Puts, near and copies

A put writes its item unless the newest record of the item's slot is the same item, by its 128-bit BLAKE3 hash, less
than the track's `near` behind. The distance is measured to where the data lands, after the puts in front of it. A put
of an item that lies near still writes a **copy** while the piece holds fewer than `1 + copies` records of it and the
newest lies at least `NearPolicy::copy_spacing` (1 MiB) behind. Copies are written only at uses, never scheduled ahead.

Puts and copies go into blocks marked `FLAG_REPEAT`. Such a block carries the sequence number of the block before it and
isn't counted, so the producer's sequence numbers stay its own count of its blocks. Puts that belong with the block
after them are also marked `FLAG_WITH_NEXT`, which a resume uses (see below).

The writer remembers, per slot, where it last wrote what. That memory is capped (`NearPolicy::memory_max`) and forgets
the slot written longest ago first. A forgotten slot costs one more write at its next put, never a wrong read.
[Numbers](numbers.md#definitions-and-puts) gives what puts and copies cost in a piece.

### Aligned files

A file that holds series is usually **aligned**: a layout with an alignment of 4 KiB (`Layout::ALIGNED`, or
`TapeWriter::set_alignment(ww_tape::ALIGNMENT)` before the first block) puts every chunk's sample payload on a 4 KiB
boundary. A reader can then map it from the page cache or read it with direct I/O. Deferred blocks fill the space before
an aligned payload while they fit, and zeros in the chunk's own head fill the rest. The summaries and checkpoints a
series produces anyway fill most gaps, so alignment costs little. An event file isn't aligned; its blocks follow each
other with nothing between them.

## Syncing: what a crash can lose

Data the operating system hasn't written to the disk is lost in a power cut. The **sync policy** (`SyncPolicy`) bounds
how much that can be:

| `SyncPolicy` | Syncs | A crash loses at most |
|---|---|---|
| `Interval(t)`, the default with `t` = 1 s | when blocks wait and the last sync is `t` ago | about `t` of data |
| `EveryBlocks(n)` | after every `n` blocks | `n` blocks |
| `None` | only at the finish | everything the system hasn't written yet |

`Interval` acts when a block arrives. A producer whose input may stop for a while calls `TapeWriter::tick` now and then,
so the last blocks are synced even when no new block comes. `TapeWriter::sync` syncs at once.

A failed write or sync ends the file. The writer reports it and refuses every later write. It never retries, because
after a failed `fsync` Linux may drop the pages it couldn't write, and a second `fsync` then succeeds without them.

## Checkpoints

A reader of a file that was never finished has no tail to start from. So the writer places a **checkpoint** index block
after about every 4 MiB of blocks (`IndexPolicy`). Each checkpoint lists the blocks placed since the one before and
points back at it. A reader after a crash finds the newest checkpoint and scans only the stretch after it, at most about
4 MiB.

Checkpoints are placed by bytes, not by time, because a place chosen by the clock would make the file's bytes depend on
when it was written. A producer that wants checkpoints by time as well sets `IndexPolicy::interval`. In an aligned file
a checkpoint waits for the gap before the next chunk payload.

## Cuts: ending one piece and starting the next

A **cut** ends the current piece and starts the next piece of the chain. `TapeWriter::cut(next_path)` places every
deferred block, writes the final index, the finish and the tail, renames the file, and returns a writer on the next
piece. The next piece starts with a piece head and the layout and producer. Its definitions and state are put again at
their first uses, since near is measured inside a piece.

`PieceChain` writes a whole chain and cuts by itself by a `CutPolicy`: after a number of bytes, a number of chunks, or
an age. It checks the policy before each chunk, so it never cuts after the last chunk and never leaves a piece empty. It
also cuts when a piece has no room left for state entries in its index (`IndexPolicy::state_entries_max`).
`PieceChain::cut_now` cuts at once.

Nothing waits for a cut. Durability comes from the sync policy and the checkpoints, which work inside a piece; the cut
only bounds how large one file grows. A slow input, such as one log line a minute, is written in a record block as it
comes, synced within the sync interval, and readable by a reader of the unfinished piece at once. Cutting by age is
there for retention by file, and it makes the file's bytes depend on the clock.

## Finishing

`TapeWriter::finish` ends the piece as complete. It places the deferred blocks, then writes the final index and the
finish block, padded so the piece ends on a 4 KiB boundary, then the tail. It syncs, renames `NAME.partial` to `NAME`
and syncs the directory. It returns `Finished`: the piece's name, its content hash, its length and its ids. `finish_as`
ends the piece in another state, such as aborted. A file still named `NAME.partial` was never finished.

## Relays

A **relay** is a program that takes a live producer's blocks and writes them into a tape of its own. For example, a host
receives a device's tape blocks over a link and records them to disk, or a capture tool reads a live feed and keeps a
file of it. The producer's stream has its own puts and copies, made by the producer's near rule in the producer's
stream. They don't fit the relay's file: the relay's file may start later, be cut elsewhere, or have lost blocks.

`TapeWriter::relay_block(&mut defs, block)` passes such a stream on, one block at a time. A block of puts (marked
`FLAG_REPEAT`) has its definitions taken into the relay's `Definitions`, and its state records are put on the relay's
tape by the relay's own near rule (`TapeWriter::put_block`). Any other block is written as data under those definitions.
The relay therefore needs no knowledge of the producer's tracks, and its file has every definition near its use like any
other.

On a raw medium a piece must be one session, as [Damage and recovery](recovery.md#sessions-and-stale-data) explains. A
writer for raw media passes a producer's blocks with `TapeWriter::adopt_block`, which renumbers each block into the
writer's own session. A jump in the producer's sequence numbers stays a jump in the writer's, so a reader still sees
blocks that were lost on the way.

## Resuming after a crash

`TapeWriter::resume(path, options)` reopens an unfinished `NAME.partial` and goes on writing the same piece. It works in
three steps:

1. It finds the piece as a reader would: from the newest checkpoint, then by a scan that stops at the first bad block.
2. It keeps every byte up to the end of the last good block. It cuts off the rest: a torn block, an end of piece, and
   blocks marked `FLAG_WITH_NEXT` at the end of what it keeps, since the block they belong with didn't survive.
3. It replays the kept blocks to rebuild the writer as it was: the content hash, the index entries, the track ranges,
   the record of what lies near, the session and the next sequence number.

The producer then gives again every call from the block `TapeWriter::next_seq` reports, with the same definitions. Since
what the writer adds depends only on the bytes before it, the calls and the options, the resumed piece ends with the
same bytes, and the same content hash, as a piece written without the crash. A put is idempotent, so the producer simply
puts again what it uses and needs no record of which puts reached the disk. The writer holds no definitions, so a resume
needs none from the file: the application passes its own, as before the crash.

A resume checks the options against the piece before it cuts or writes anything. Pass the options the piece was written
with; `TapeWriter::options` gives them. A layout or producer other than the newest the piece kept is refused
(`WriteError::Layout`, `WriteError::Producer`), because the writer would place blocks the way the file says it doesn't.
A piece found with its finish (a crash after the finish, before the rename) is opened again: its finish and tail are cut
off, and the next finish pads the piece to 4 KiB again.

`PieceChain::resume` does the same for a chain. It reads the finished pieces back from their tails and resumes the last,
or starts the next piece when the crash came between a cut's finish and the next piece's start. A chain whose last piece
already ended complete or aborted is refused with `WriteError::Ended`. [Damage and
recovery](recovery.md#resuming-after-a-crash) follows a recorder through a power cut.

## Same input, same bytes

A writer given the same input writes the same bytes. Blocks hold no wall time, puts and copies follow from the bytes
before them, and checkpoints and cuts are placed by bytes. A producer that seals blocks on a timer, adds checkpoints by
time or cuts pieces by age gives up this property for what it places that way. [Tapes](index.md#same-input-same-bytes)
says what the property is good for.
