# Damage and recovery

Recordings fail in ordinary ways. Power goes out in the middle of a write, a cable is pulled, or a sector goes bad on
an old disk.

A tape is built so that each of these failures costs the bytes it touched and nothing else. A reader then says what
it could not read instead of guessing.

This page explains how the format finds damage and how a writer continues after a crash. It assumes the
[overview](index.md) and refers to chunks from [Events and series](events_and_series.md).

Two more pages continue the topic:

- [Reading a damaged file](damaged_files.md): how a reader reads past damage, what a damaged file still gives, and the
  limits of the format.
- [Sessions and raw media](raw_media.md): stale data on a reused card, gaps and duplicate blocks.

## Checksums at every level

Damage has to be found before it can be contained. A tape checks its bytes with a cyclic redundancy check (CRC) and a
content hash, at four levels:

| Check | Covers | Damage costs |
|---|---|---|
| block CRC-32C | each whole block | that block |
| head CRC-32C | the head of a chunk or index block | the head alone; the block's place stays known |
| segment CRC-32C | each 64 KiB segment of a chunk's payload | one segment of one chunk |
| content hash, BLAKE3 | every byte of a closed piece before its close record | nothing; it proves the piece is unchanged |

A chunk's **head** gives the chunk's length, its track, its item numbers and ticks. It also holds one CRC per
**segment**, a fixed stretch of the payload.

The head CRC matters most. When the payload is damaged but the head is good, a reader still trusts the length and
therefore finds the next block.

The reader also knows exactly which items were hit. For example, a flipped bit in a 4 MiB series chunk costs one 64 KiB
segment of samples.

Writers use 64 KiB segments. The head states the segment size, and a reader takes any size from 4 KiB to 16 MiB.

Every CRC is CRC-32C. Writers and readers compute it with the CPU's CRC instructions where they exist: SSE4.2 on x86-64
and the CRC extension on aarch64.

Elsewhere, such as in a browser or on a microcontroller, a table computes it. The result is the same, so the choice
never shows in the file.

The checks detect damage; they do not correct it. Correction is the medium's job: the error correction of a disk or a
flash controller, or a file system.

The block kind `BlockKind::Parity` is reserved for parity blocks, but no writer writes them.

## Writing so a crash costs little

`TapeWriter` writes a file the way databases write their logs. The file functions of the crate `ww_tape_fs`
(`writer::create`, `chain::create`) do this through a `FileSink`:

1. The sink creates the file as `NAME.partial`. It syncs the file and its directory, so the name can't vanish in a
   crash.
2. The writer syncs the data by a `SyncPolicy`: never, every N blocks, or at most an interval apart (1 s by default).
   The policy bounds what a crash can lose.
3. At the close the writer writes the final index, the close record and the tail. It pads the close record so the
   piece ends on 4 KiB.
4. The sink then renames the file to `NAME` and syncs the directory again.

A failed write or sync ends the file. The writer reports it and refuses every later write (`WriteError::Failed`).

The writer never retries. After a failed `fsync`, Linux may drop the unwritten pages, and a second `fsync` then
succeeds without them.

A file that still ends in `.partial` was never closed.

A file has one writer at a time. A file sink holds an exclusive lock on the lock file `NAME.lock` beside it.

A second writer of the same file gets `FsError::Locked`. The lock belongs to the writer's process, so a crash releases
it.

The crash leaves the lock file behind, but the file holds no lock. The next writer takes it.

### Resuming after a crash

A **resume** reopens an unclosed piece and continues it as if the crash never happened. These calls resume:

- `ww_tape_fs::writer::resume` takes the path `NAME` and the writer options, and reopens `NAME.partial`.
- `TapeWriter::resume_on` does the same on any other sink, such as a `MemSink`.

Calling `writer::create` again instead fails. It refuses a `.partial` file that holds bytes (`FsError::Exists`).

The resume reads the piece the way `TapeReader` does. It starts from the newest checkpoint and scans the blocks after
it up to the first bad block.

The resume keeps every good block up to the end of the piece's data. It cuts off the rest:

- a torn block, the last one the crash interrupted;
- the end of a piece (final index, close record) when the crash came inside it;
- trailing puts marked `FLAG_WITH_NEXT`, which belong with a block that didn't survive.

The resume then replays every block it keeps, so the writer knows again what it put and where.

A damaged block that the index still lists stays in the piece. It counts as a newer record of each slot it held,
with the item unknown.

The next put of such a slot therefore writes the item again.

#### What the producer does after a resume

The producer gives the calls again, from the block that `TapeWriter::next_id` names on. It passes the same definitions
and the same writer options it wrote the piece with; `TapeWriter::options` returns them.

The resumed piece then ends with the same bytes, and the same content hash, as one written without the crash. The
writer returns `Resumed` beside it, which says how many bytes the resume kept and dropped and where the reader stopped.

The writer holds no definitions of its own, so a resume needs none from the file. The application passes its
definitions with each call, as before the crash.

The writer puts what each block needs again wherever the same item doesn't lie close behind.

Three cases at the end of a piece differ:

- A piece whose last close and tail read whole is closed: the crash, if any, came after the close. The resume cuts
  off junk after the tail and writes on after it, behind an append head.
- A piece that a cut closed names the next piece in its close and is not written on. The resume gives it its name and
  returns `ResumeError::Continued`.
- A piece cut inside its end is opened again. The resume cuts off its final index and close record, and the next
  close pads the piece to 4 KiB again.

[Closing more than once](format.md#closing-more-than-once) shows the bytes of a piece written on after its close.

### When a resume refuses

A resume cuts off a torn end and nothing more. Where cutting would lose good data, or where it can't tell, it refuses
and leaves the bytes as they are:

| Error | Why |
|---|---|
| `ResumeError::NoPiece` | there is no unclosed piece; `ww_tape_fs` reports `FsError::NoPiece` with the path |
| `ResumeError::GoodBlockAfterCut { offset }` | a good block of the piece lies past where the reader stopped |
| `ResumeError::LookSpent { offset }` | the look for good blocks ran out before the end of the bytes |
| `ResumeError::HashWalkSpent { end }` | the content-hash walk of the bytes kept spent its budget; new blocks would not verify |
| `InputError::Layout`, `InputError::Producer` | the options name another layout or producer than the piece |

For example, a sector goes bad in the middle of the stretch after the newest checkpoint, and the power then fails. The
reader's scan stops at the bad sector, but good blocks follow it.

Cutting there would lose those blocks. The resume therefore refuses with `GoodBlockAfterCut`, naming the first good
block.

A walk in the `Resync` mode still reads every good block of the file ([Reading a damaged
file](damaged_files.md#resync-reading-on-after-damage)).

The look past where the reader stopped has two bounds:

- It reads at most 128 MiB (`RESUME_LOOK_MAX`), twice the longest block.
- It checks at most 4 places whole (`RESUME_PROBES_MAX`).

Crafted bytes therefore cost a bounded read. A resume that runs out refuses with `LookSpent` instead of cutting.

#### Checks of the options

The resume checks the options against the piece before it cuts or writes anything. The newest layout and producer the
piece names must be the ones the options give, and each error names both.

The chunk alignment matters because a reader trusts the [layout](raw_media.md#blank-space-and-gaps) when it looks for
the end of the data. Options with another chunk alignment would place blocks the way the file says it doesn't.

The layout's gap settings need no check, because the writer never leaves a gap.

A damaged block that the resume keeps leaves the piece checkable. The close record marks the hash of each track whose
data that block may hold as incomplete (`TrackRange::incomplete`), and the piece still verifies.

### Resuming a chain of pieces

`ww_tape_fs::chain::resume` resumes a chain of pieces in a directory, and `PieceChain::resume_on` does so on any store
of tapes. The chain reads its closed pieces back from their tails and resumes the unclosed last one.

For example, a recorder loses power while it writes piece 3 of a chain. On restart it calls `ww_tape_fs::chain::resume`
with the directory, the owner, the chain number and the chain options.

The chain reads pieces 0 to 2 back from their tails and reopens `00000003.wwt.partial`. The recorder goes on from the
first block the file lacks.

Two crashes at a piece boundary resume too:

- A crash between closing one piece and creating the next: the chain creates the next piece, as the cut would have.
- A crash inside the head of the next piece: the piece starts again.

A chain kept in memory from which `MemTapes::remove` took pieces is not resumed.

#### A chain in one file

A chain written into one file (`PieceTapes::One`) resumes the same way. The chain finds the pieces by their tails from
the end of the file.

It then resumes the last piece behind the closed ones, unclosed or closed: a closed last piece is written on after its
close. The pieces before it keep every byte, and so does the last piece up to its close.

The resume refuses, with `WriteError::Index`, a last piece whose head is damaged. Keeping its blocks would leave them
outside the chain, and dropping them would lose more than a torn end.

### Testing crashes in memory

A `MemSink` keeps a piece in memory, and its tape (`MemSink::tape`) reads back as a byte source. A test writes a
piece, cuts the bytes at any offset, and resumes or reads what is left.

The tests cut pieces at every block boundary and at bytes in between. They check that each piece resumes to the bytes
of an uninterrupted write.

[Writing a tape](writing.md) describes the sinks and stores of tapes.

### Example: a recorder that lost power

A recorder on a desktop computer writes a tape of several tracks, 50 MiB so far. It uses the default sync interval of
1 s.

The power fails. On disk is `capture.wwt.partial`: every block up to the last sync, and perhaps part of the block being
written.

A reader opens the file. The file has no tail, so the reader looks backwards from the end for the newest checkpoint
index.

That checkpoint is at most about 4 MiB back.

From that checkpoint the reader follows the chain of checkpoints back. It thereby lists every block before the newest
checkpoint without reading them.

The reader then scans the blocks after the newest checkpoint, one by one. It stops at the torn block, which fails its
CRC.

The reader reports the piece as unclosed, cut at that offset (`PieceInfo::cut`).

The recording loses the torn block and whatever the writer had not yet synced: at most about one second of data.
Everything else reads, including the definitions and the summary blocks written so far.

The reader can't check the content hash, because the writer never wrote the close record. That is how a reader knows
the file was not closed.

#### After the restart

When the recorder starts again, it calls `ww_tape_fs::writer::resume` with `capture.wwt` and its writer options. The
resume cuts off the torn block and returns the writer.

The recorder gives its calls again from the first block the file lacks. The closed `capture.wwt` then reads as if
the power had never failed.

## See also

- [Reading a damaged file](damaged_files.md): the walk past damage, salvage, and the limits of the format.
- [Sessions and raw media](raw_media.md): sessions, stale data, gaps and duplicates.
- [Format reference](format.md#sessions-media-and-damage): sessions, media, gaps and state records as a reference.
- [Writing a tape](writing.md): the writer, its sinks and stores of tapes.
- [Resuming a writer](resuming.md): the producer's side of a resume.
