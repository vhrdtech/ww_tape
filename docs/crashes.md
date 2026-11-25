---
level: basic
---

# When the power fails

The bench host has recorded the drive's endurance run for 37 minutes when its power fails. It was in the middle of
writing a [[block]] of frames from the CAN bus. What did the crash cost, can a [[reader]] still read the file, and how
does the recorder go on? The [[closed]] recording should end up the same as if the power had never failed.

The short answers come first, and the sections below show why each one holds:

- The crash costs about the last second of data.
- A reader reads everything before the damaged block.
- A [[resume]] cuts off the damaged end, and the recorder writes that part again.

## A file's bytes tell its state

A [[tape]] file is never [[closed]] by accident, and its bytes show whether it was. [`writer::create`] makes the file
`capture.wwt` under its final name and takes a [[lock]] on it. Only [`TapeWriter::close`] ends the [[piece]] with a
[[close]] and a [[tail]], and syncs the file.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/states.svg"
<figcaption>A file's states over its life, under one name. A closed file ends in a close and a tail. A crash leaves a
torn end and no lock, and a resume takes the file back to writing.</figcaption>
</figure>

A program that finds `capture.wwt` on disk reads its end to learn whether a [[writer]] closed it. The lock tells whether
a writer still works on it. The kernel holds the lock for the writer's process, so a crash releases it and leaves
nothing behind. While the lock is held, a second writer gets [`FsError::Locked`] and stops.

## What the crash costs

An operating system keeps written bytes in memory for a while before it puts them on the disk. A power failure loses
whatever it has not put there yet. The [[writer]] bounds that loss with its [[sync policy]]. By default it asks the
system to put its bytes on the disk at least once a second. The drive's recorder thus loses about the last second.

| [`SyncPolicy`] | The writer syncs | A crash loses at most |
|---|---|---|
| [`SyncPolicy::Interval`], 1 s by default | when blocks wait and the last sync is that long ago | about that long of data |
| [`SyncPolicy::EveryBlocks`] | after every n blocks | n blocks |
| [`SyncPolicy::None`] | only at the close | everything the system has not written yet |

The bytes on the disk end in a [[torn block]], the [[block]] the writer was in the middle of. It ends early or fails
its [[CRC-32C]]. Everything synced before it is whole, because the writer only appends and never rewrites a byte.
[Files and syncing](files.md#how-often-to-sync) measures what each policy loses and what its syncs cost.

## How a reader finds the end

A [[closed]] [[piece]] ends with a [[tail]], and a [[reader]] starts there. The tail leads it to the [[final index]],
which lists every [[block]]. An unclosed piece has neither, so the reader needs another way to its blocks. The
[[writer]] provides one while it writes: every 4 MiB it places a [[checkpoint]]. A checkpoint is an index block that
lists the blocks since the previous checkpoint and points back at it.

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/crash.svg"
<figcaption>A piece that a crash cut off. The reader finds the newest checkpoint near the end, follows the
checkpoints back to list every block before it, and scans the few blocks after it one by one until the torn block
fails its CRC. Drawn by <code>ww_tape_figures</code> from the drive's recording at 1/1000 of its data, cut at minute
37. Its checkpoints come every 3 KiB instead of every 4 MiB.</figcaption>
</figure>

The reader finds its way in three steps:

1. It looks backwards from the end of the bytes for the newest checkpoint, which lies at most about 4 MiB back.
2. It follows the checkpoints back to the start, and so lists every block before the newest one without reading them.
3. It scans the blocks after the newest checkpoint one by one and stops at the [[torn block]].

The scan therefore covers at most about 4 MiB, however long the recording is. The drive's file reads like this after
the crash, and [`PieceInfo::cut`] says where the good blocks end:

```rust
--8<-- "ww_tape_tests/tests/it/crashes.rs:read"
```

Everything before that offset reads as it would in a closed file. That covers the [[definitions]], the log lines, the
[[CAN]] frames and the [[summaries]] written so far. The piece has no [[content hash]] to check, because the [[close record]]
that would hold it was never written. Its missing tail is how a reader tells that the file was not closed.

## Resuming the file

When the recorder starts again, it calls [`writer::resume`] with the path and the options it wrote the file with. The
[[resume]] then works in three steps:

1. It reads the file the way a [[reader]] does.
2. It keeps every byte up to the end of the last good [[block]] and cuts off the rest, the [[torn end]].
3. It replays the kept blocks, so that the [[writer]] knows again its index, its hashes, and which [[definitions]] and
   [[state items]] lie close behind.

The recorder then gives its calls again from the first block the file lacks, which [`TapeWriter::next_seq`] names.
The drive's recorder keeps every block it sends, so it sends the lost ones again. A recorder that relays a device's
stream can instead ask the device for the blocks from that point on.

```rust
--8<-- "ww_tape_tests/tests/it/crashes.rs:resume"
```

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/resume.svg"
<figcaption>The end of the piece three times: as the writer would have written it, as the crash left it, and resumed.
The resume cut off the torn block and the puts that went with it. The calls given again wrote the same blocks, so
the resumed piece is the same, byte for byte. Drawn by <code>ww_tape_figures</code> from the same scaled recording, and
it checks that the bytes are equal.</figcaption>
</figure>

The resumed file ends with the same bytes as a file written without the crash, and so with the same [[content hash]].
That holds because what the writer adds depends only on the bytes before it, the calls and the options. It needs no
[[clock]] and no memory of what happened before the crash. The test behind this section cuts the file, resumes it and
compares it with one written whole.

### Puts that go with a lost block

Before a [[block]] of data, the [[writer]] [[puts]] the [[definitions]] and [[state items]] that the block needs. It
skips those of which a copy lies close behind. Such puts belong with the block after them, so the writer marks them with the flag
`FLAG_WITH_NEXT`. When the crash tears the block, the [[resume]] cuts off its puts too.

The recorder then gives the call for the block again, and the writer writes the same puts and the same block. A put
that finds its [[item]] close behind writes nothing, so the recorder never needs to know which puts reached the disk.
It puts again what it uses, as it did before the crash.

A crash can also tear a group of puts or fall between two pieces of a chain. Damage can look like a torn end. [Beyond a torn block](resume_cases.md) shows what a resume does in each case.
