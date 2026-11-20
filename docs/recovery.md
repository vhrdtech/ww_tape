# Damage and recovery

Recordings fail in ordinary ways. Power goes out in the middle of a write, a cable is pulled, or a sector goes bad on
an old disk.

A tape is built so that each of these failures costs the bytes it touched and nothing else. A reader then says what
it could not read instead of guessing.

This page explains how the format finds damage and how a writer limits what a crash costs. It assumes the
[overview](index.md) and refers to chunks from [Events and series](events_and_series.md).

Two chapters continue the topic:

- [When bytes go bad](damage.md): how a reader reads past damage, what a damaged piece still gives, and the limits of
  the format.
- [Where the data ends on a raw card](raw_media.md): stale data on a reused card, gaps and duplicate blocks.

## Checksums at every level

[When bytes go bad](damage.md#checks-at-three-levels) explains the checks of a tape and what damage under each costs.

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

### After a crash { #resuming-after-a-crash }

[When the power fails](crashes.md) follows a recording through a power failure. It shows what the crash costs and how
a reader finds the end of the file. It also shows how a resume lets the writer go on to the same bytes.

## See also

- [When bytes go bad](damage.md): the walk past damage, salvage, and the limits of the format.
- [Where the data ends on a raw card](raw_media.md): sessions, stale data, gaps and duplicates.
- [Format reference](format.md#sessions-media-and-damage): sessions, media, gaps and state records as a reference.
- [Writing a tape](writing.md): the writer, its sinks and stores of tapes.
- [When the power fails](crashes.md): a crash, the file it leaves, and the resume.
