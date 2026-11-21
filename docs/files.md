---
level: basic
---

# Files and syncing

A file on a disk is only as safe as the bytes the operating system has put there. A recorder must know what a power
failure can take, and that no second program writes the same file. How does a [[writer]] make a file durable, and how
much may a crash cost? The examples use the drive's recording, whose host loses power at minute 37 ([When the power
fails](crashes.md)).

## Opening a file

Everything that names a path lives in the crate `ww_tape_fs`, so `ww_tape` itself builds without a file system. Its
module `writer` opens a [`TapeWriter`] on a path in one of four ways:

| Call | What it opens |
|---|---|
| [`writer::create`] | a new file, under the name `NAME.partial` until the close |
| [`writer::append`] | an existing file, with a new [[piece]] after the last one, or a new file |
| [`writer::resume`] | the unclosed piece of a file, after a crash ([Resuming the file](crashes.md#resuming-the-file)) |
| [`writer::cut`] | the next piece of a [[chain]] in a new file ([Pieces and chains](pieces.md)) |

A recorder starts with [`writer::create`] and its [[sync policy]]. A host program usually also gives the writer its
clock, which decides when a time interval is up:

```rs
--8<-- "ww_tape_tests/tests/it/durable.rs:create"
```

[`writer::create`] never empties a file. It refuses a `NAME` that exists, and a `NAME.partial` that a crash left with
bytes in it ([`FsError::Exists`]). Reopen such a piece with [`writer::resume`], or remove it. The sink syncs the new
file and its directory at once, so the name cannot vanish in a crash.

An error states the step, the file and the kind of failure in its first line. For example: `creating a writer:
/data/x.wwt: locked by another writer`. A log line of that text alone says what went wrong. An error of the operating
system, such as a full disk, is the source of that first error and is not repeated in its text.

## One writer per file

Two writers appending to one file would interleave their [[blocks]] into a file that reads as damaged. A file sink
therefore holds a [[lock]] on the file `NAME.lock` beside `NAME`. A second writer of the same file is refused with
[`FsError::Locked`], whether it runs in the same process or another:

```rs
--8<-- "ww_tape_tests/tests/it/durable.rs:locked"
```

The lock sits on a file of its own, so readers never touch it. A [[reader]] opens and closes `NAME` as it likes and
never releases the lock by doing so. The lock file holds no byte that a reader needs. A piece moved without its lock
file loses nothing.

The lock belongs to the writer's process, so a crash releases it. The lock file stays behind and holds no lock, and the
next writer takes it as it is. [Locks](format.md#locks) gives the rules for links, long names and devices.

A file on a read-only mount takes no writer: each step that meets the refusal fails with [`FsError::ReadOnly`] and the
path. Read such a file with [`FileSource`], which opens it for reading only and takes no lock.

## How often to sync

An operating system keeps written bytes in memory for a while before it puts them on the disk. A power failure loses
whatever it has not put there yet. The writer asks for the bytes to go to the disk, a sync, by its [[sync policy]]:

| [`SyncPolicy`] | The writer syncs |
|---|---|
| [`SyncPolicy::Interval`], 1 s by default | when blocks wait and the last sync is that long ago |
| [`SyncPolicy::EveryBlocks`] | after every n blocks |
| [`SyncPolicy::None`] | only at the close |

The drive's crash shows what each policy costs. The example writes the hour four times, once per policy, and counts what
was not synced when the power failed at minute 37:

| Policy | Syncs | Bytes not synced | Whole blocks not synced | Of the run |
|---|---:|---:|---:|---:|
| `Interval(1 s)` | 2223 | 30866 | 3 | 1.000 s |
| `EveryBlocks(1)` | 8516 | 1263 | 0 | 0.333 s |
| `EveryBlocks(256)` | 36 | 622577 | 75 | 20.000 s |
| `None` | 4 | 13412380 | 1611 | 420.000 s |

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/sync_loss.svg"
<figcaption>Three seconds of the drive's file and where each sync policy synced. A crash in the third second loses the
bytes after the last sync. That is part of one block with a sync after every block and about a second by interval.
Without syncs, it is everything since the last close. Drawn by <code>ww_tape_figures</code> from the drive's first two
minutes at full rate.</figcaption>
</figure>

A sync after every block loses only the [[torn block]], the 1263 bytes a resume cuts off. It pays with about 8520 waits
for the disk. The default loses the last second, three whole blocks, with 2223 syncs. Without syncs, the crash may lose
all of piece 2: 420 s of the run. A recorder of a slow, precious stream syncs after every block; one of a fast stream
keeps the default.

### A quiet input

[`SyncPolicy::Interval`] acts only when a block arrives. A log line written just before the input goes quiet would wait
unsynced until the next block. A producer whose input may stop for a while therefore calls [`TapeWriter::tick`] now and
then:

```rs
--8<-- "ww_tape_tests/tests/it/durable.rs:tick"
```

`tick` syncs the waiting blocks once the interval is up, even when no block comes. With [`IndexPolicy::interval`] set,
it also places a [[checkpoint]] when that interval is up. In an [aligned file](writing.md#aligned-files), that
checkpoint waits for the gap before the next chunk's payload. [`TapeWriter::sync`] syncs at once.

### The writer's clock

The writer reads time only through its clock, set by [`WriterOptions::clock`] or [`TapeWriter::set_clock`]. With the
standard library, the default is the system's monotonic clock, [`ClockRef::SYSTEM`]. Without it, the default is
[`ClockRef::NONE`]: no interval is ever due, and only the block and byte limits apply. Firmware passes its own
[`clock::Clock`], and a test passes one it moves by hand.

Time decides only when the writer syncs, places a checkpoint or cuts. No reading of the clock goes into a block. The
drive's recorder sets [`ClockRef::NONE`], so its bytes follow from its calls alone.

### A failed write ends the file

A failed write or sync ends the file. The writer reports the failure and refuses every later write
([`WriteError::Failed`]). It never retries: after a failed `fsync`, Linux may drop the pages it could not write, and a
second `fsync` then succeeds without them.

## Writing so a crash costs little

A file sink writes the way databases write their logs. The bytes before the last sync are whole, because the writer only
appends and never rewrites a byte. The name tells whether the file was closed. And the close is written so that a whole
close never names bytes that missed the disk.

### The close

[`TapeWriter::close`] ends the piece in five steps:

1. It places the [[deferred blocks]] still waiting.
2. It writes the [[final index]] and syncs the file.
3. It writes the [[close]], padded so that the piece ends on a 4 KiB boundary, then the [[tail]].
4. It syncs the file again.
5. It renames `NAME.partial` to `NAME` and syncs the directory.

```rs
--8<-- "ww_tape_tests/tests/it/durable.rs:close"
```

The two syncs hold whatever the sync policy. A disk may write pages out of order, so a crash could leave a whole close
behind blocks that never reached the disk. The sync in step 2 rules that out: a close found whole names only bytes that
were on the disk before it. A file still named `NAME.partial` was never closed.

### Appending to a closed file

[`writer::append`] opens a closed file again and adds a new piece after the last one, in a new [[session]]. The pieces
before stay as they were, to the byte:

```rs
--8<-- "ww_tape_tests/tests/it/durable.rs:append"
```

The file then holds two pieces, each read alone. A [[resume]] differs: it writes on in the last piece, after its close,
and the next close names the one before ([Closing more than once](format.md#closing-more-than-once)).
