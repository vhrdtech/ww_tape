# Syncing, cuts and relays

A long-running writer bounds two things: what a crash can lose, and how far a reader of an unclosed piece must scan.
Syncs bound the first.

**Checkpoints**, index blocks that the writer places between the data, bound the second. Both work inside a piece.

This page also covers the **cut**, which ends one piece of a chain and starts the next, and the close of a piece. It
ends with programs that pass on another producer's blocks, and it assumes [Writing a tape](writing.md).

## Syncing: what a crash can lose

Data that the operating system has not yet written to the disk is lost when the power fails. The **sync policy**
(`SyncPolicy`) bounds how much that can be:

| `SyncPolicy` | Syncs | A crash loses at most |
|---|---|---|
| `Interval(t)`, the default with `t` = 1 s | when blocks wait and the last sync is `t` ago | about `t` of data |
| `EveryBlocks(n)` | after every `n` blocks | `n` blocks |
| `None` | only at the close (twice, see [Closing](#closing)) | everything the system has not written yet |

`Interval` acts only when a block arrives. A producer whose input may stop for a while therefore calls
`TapeWriter::tick` now and then.

`tick` syncs the last blocks even when no new block comes. With `IndexPolicy::interval` set, `tick` also places a
checkpoint when its interval is up.

In an unaligned file, the checkpoint goes in at once. In an [aligned file](writing.md#aligned-files), `tick` only marks
the checkpoint due, and it goes into the gap before the next chunk.

`TapeWriter::sync` syncs at once.

### The writer's clock

The writer reads time only through its clock, `WriterOptions::clock` (or `TapeWriter::set_clock`). With `std`, the
default is the system's monotonic clock, `ClockRef::SYSTEM` (a `SystemClock`), so two default options compare equal.

Without `std`, the default is `ClockRef::NONE`: no interval is ever due, and only the block and byte limits apply.
Firmware passes its own `Clock`, and a test passes a clock it moves by hand.

Time decides only when the writer syncs, places a checkpoint or cuts. No reading of the clock is written into a block.

### A failed write ends the file

A failed write or sync ends the file. The writer reports the failure and refuses every later write.

The writer never retries. After a failed `fsync`, Linux may drop the pages it could not write, and a second `fsync` then
succeeds without them.

## Checkpoints

A reader of a file that was never closed has no tail to start from. The writer therefore places a checkpoint, an index
block, after about every 4 MiB of blocks (`IndexPolicy::bytes`).

Each checkpoint lists the blocks placed since the one before and points back at it. After a crash, a reader finds the
newest checkpoint and scans only the stretch after it, at most about 4 MiB.

The writer places checkpoints by bytes, not by time. A place chosen by the clock would make the file's bytes depend on
when the file was written.

With `IndexPolicy::interval`, a producer gets checkpoints by time as well. In an aligned file, such a checkpoint waits
for the gap before the next chunk payload.

## Cuts: ending one piece and starting the next

A cut ends the current piece and starts the next piece of the chain. Only a piece of a chain can be cut.

A writer starts such a piece with `TapeWriter::begin_piece`, which writes the piece head as the first block of the file.

`TapeWriter::cut_with(defs, tapes, key)` makes a cut in five steps:

1. It places every deferred block.
2. It writes the final index, the close and the tail.
3. It closes the sink; a file takes its final name.
4. It starts the next piece in a new sink, which the store `tapes` makes under `key`.
5. It puts the definitions of every track the new head carries, from the application's `defs`.

The call returns the closed piece (`Closed`) and a writer on the next piece. `TapeWriter::cut_on(defs, next)` does the
same on a sink `next` that the caller made.

`ww_tape_fs::writer::cut(writer, defs, next_path)` does the same with the file `next_path`.

The next piece starts with a piece head, the layout and the producer. The writer puts other definitions and state again
at their first uses, because it measures near inside a piece.

### The definitions a new piece carries

A piece head carries a row for every track that gave data in the chain so far. The writer puts the definition of each of
these tracks right after the head, the layout and the producer. The clocks, maps and bundles it needs come with it. A
quiet track, one with no data in the new piece, so keeps its kind, its clock and its label there.

Older pieces may be dropped or removed. A reader of the chain then finds, in the oldest piece left, the definition of
each track that `defs` held at the cut:

- a track that gave data before that piece: right after its head;
- a track that gives data first in a later piece: beside its first data, as before.

Some tracks have no definition there:

- A track that never gave data has no row and no definition in any piece.
- A track closed in `defs` (`Definitions::close`) keeps its row but no definition: dropping the pieces before loses it.
- A row whose number `defs` holds under another id or owner is passed over, since that definition is another track's.

The carried definitions come in blocks of at most `CARRIED_BLOCK_LEN` (4 KiB):

- First the bundles, each once. A bundle longer than a block has a block of its own.
- Then the tracks, grouped by the clocks and maps they count through. Each block holds its group's clocks and maps
  again, then as many tracks as fit.

One damaged block so costs only its own tracks, never another block's clock. A lost bundle block costs its tracks their
bundle.

#### When they don't fit

The definitions are state records, so each one also takes an index entry. A write that brings in a track past what
`IndexPolicy::state_entries_max` leaves for the next head's definitions is refused with `InputError::CarriedEntries`,
nothing written. Data on a track counted in already is never refused.

A cut is never refused for the definitions it carries. They can still outgrow the room in three ways:

- `defs` gains a clock map after the tracks were counted in;
- a definition grows, such as a longer label;
- the index policy is lowered.

The cut then fits them in this order, while room lasts:

1. Each definition once: the blocks no longer repeat their clocks and maps.
2. The clocks and maps the tracks count through.
3. The tracks with data in the piece, newest data first, each with its bundles; then the quiet tracks.

A track that doesn't fit is left out. The next head marks its row as not carried, and `Closed::tracks` of the cut marks
it too (`Closed::not_carried` counts them). A reader of the piece without the pieces before names each such track in
`DefinitionsAt::not_carried`, rather than missing its definition without a word.

#### What they cost

The cost per piece comes from `just bench-carried`. Records blocks of 4000 bytes go on one track, the others stay quiet,
each track is labeled `channel NNNNNNN`, and the piece is cut by bytes. A piece holds less data, and its final index
grows past the cut's bound, which counts no final index:

| tracks | cut at | records blocks | without carried | data less | piece (bytes) | without carried | piece more |
|---|---|---|---|---|---|---|---|
| 100 | 1 MiB | 255 | 257 | 0.8 % | 1093632 | 1081344 | 1.1 % |
| 100 | 8 MiB | 2037 | 2042 | 0.2 % | 8769536 | 8761344 | 0.1 % |
| 1000 | 1 MiB | 214 | 236 | 9.3 % | 1200128 | 1081344 | 11.0 % |
| 1000 | 8 MiB | 1970 | 2021 | 2.5 % | 8871936 | 8634368 | 2.8 % |

Each definition costs its record and its index entries, in the checkpoint that lists it and in the final index. At the
track caps, about 335 bytes per track, a piece of one records block grows:

- by 155783168 bytes for `TRACKS_PER_PIECE_MAX` (466430) tracks;
- by 234717184 bytes for `HEAD_TRACKS_MAX` (699006) tracks.

A piece cut by bytes below what its carried definitions take holds them and one chunk: each cut comes due right after
the head.

A resume that keeps only the piece head (a crash right after the cut) leaves the definitions out. The next call given
`defs` puts them first, so the piece gets the bytes of a cut that never stopped. A piece closed before any such call
carries none: `close` takes no `defs`, so the piece before it keeps the only copy.

### Cutting by policy

`PieceChain` writes a whole chain and cuts by itself, by a `CutPolicy` with three limits:

- `bytes`: the size of the piece;
- `chunks`: the number of chunk blocks in the piece;
- `interval`: the age of the piece, counted from its start or its resume.

A limit of 0 or `None` is off, and the first limit reached cuts. The chain checks the policy before each chunk.

A cut by policy therefore never comes after the last chunk and never leaves a piece without data. `PieceChain::cut_now`
cuts at once.

The chain also cuts before a write that would need more state entries than the piece's index may hold
(`IndexPolicy::state_entries_max`).

### Limits that a cut does not lift

Two limits on tracks hold whatever the cuts, so that each close and each head fits one block:

- A piece takes data of at most `TRACKS_PER_PIECE_MAX` tracks.
- A piece head carries at most `HEAD_TRACKS_MAX` tracks from the pieces before it.

The writer refuses a write past either limit (`InputError::TooManyTracks`, `InputError::TooManyCarried`).

### One tape per piece or one for all

A chain writes each piece into a tape of its own. With `ChainOptions::tapes` set to `PieceTapes::One`, it writes every
piece into one tape.

`TapeWriter::cut_in_place` is the cut of a writer into one file. The next piece starts right behind the closed one in
the same sink, and the file keeps its name.

A sink that holds one piece only, such as a `NAME.partial` file, refuses this cut. [Pieces and
files](pieces_and_files.md#writing-a-chain-into-one-file) explains when to choose which.

### Nothing waits for a cut

Durability comes from the sync policy and the checkpoints, which work inside a piece. A cut only bounds how large one
file grows.

For example, a slow input of one log line a minute goes into a records block as it comes. The writer syncs it within the
sync interval, and a reader of the unclosed piece reads it at once.

Cutting by age serves retention by file, and it makes the file's bytes depend on the clock.

## Closing

`TapeWriter::close` ends the piece as complete, in five steps:

1. It places the deferred blocks.
2. It writes the final index and syncs the file.
3. It writes the close record, padded so that the piece ends on a 4 KiB boundary, then the tail.
4. It syncs the file again.
5. It renames `NAME.partial` to `NAME` and syncs the directory.

The two syncs hold whatever the sync policy, and a cut makes them too.

A disk may write pages out of order, so a crash could leave a whole close and tail behind blocks that never reached the
disk. The sync in step 2 rules that out: a close record found whole names only bytes that were on the disk before it.

It returns a `Closed`, which holds:

- the piece's content hash and its length;
- its ids, and where it starts in its file;
- the hash and next item number of each track.

`close_as` ends the piece in another state, such as aborted. A file still named `NAME.partial` was never closed.

## Relays

A **relay** is a program that takes a live producer's blocks and writes them into a tape of its own. For example, a host
receives a device's tape blocks over a link and records them to disk.

A capture tool that reads a live feed and keeps a file of it is a relay too.

The producer's stream carries its own puts and copies, placed by the producer's near rule in the producer's stream. They
do not fit the relay's file, which may start later, be cut elsewhere, or have lost blocks.

`TapeWriter::relay_block(&mut defs, block)` passes such a stream on, one block at a time. A block of puts is marked
`FLAG_REPEAT`.

The relay takes the definitions of such a block into its own `Definitions` container. It puts the block's state records
on its own tape by its own near rule (`TapeWriter::put_block`).

The relay writes any other block as data under those definitions. It therefore needs no knowledge of the producer's
tracks, and its file has every definition near its use, like any other file.

### Adopting blocks on raw media

On a raw medium a piece must be one session, as [Where the data ends on a raw
card](raw_media.md#sessions-and-stale-data) explains.
A writer for raw media passes a producer's blocks with `TapeWriter::adopt_block`.

The call renumbers each block into the writer's own session. It returns where the block landed (`Placed`), or `None` for
a block of puts.

A jump in the producer's sequence numbers stays a jump in the writer's. A reader therefore still sees that blocks were
lost on the way.

## Same input, same bytes

A writer given the same input writes the same bytes. Blocks hold no wall time, puts and copies follow from the bytes
before them, and checkpoints and cuts are placed by bytes.

A producer may seal blocks on a timer, add checkpoints by time or cut pieces by age. It then gives up this property for
what it places that way.

[Same input, same bytes](anatomy.md#same-input-same-bytes) says what the property is good for.
