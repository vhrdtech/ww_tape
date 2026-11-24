---
level: basic
---

# When the power fails

The bench host has recorded the drive's endurance run for 37 minutes when its power fails. It was in the middle of
writing a [[block]] of frames from the CAN bus. This chapter answers three questions:
what the crash cost, how a [[reader]] reads the file left behind, and how the recorder goes on. The
[[closed]] recording should be the same as if the power had never failed.

In short, the crash costs about the last second of data. A reader reads everything before the damaged block. A
[[resume]] cuts off the damaged end and lets the recorder write it again. The rest of the chapter shows why each answer
holds. It assumes [Tape concepts](concepts.md), and it uses the drive's recording, a
motor controller on a test bench, as its example throughout.

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
system to put its bytes on the disk at least once a second. The drive's recorder used the default, so it loses at most
about the last second of the run.

| [`SyncPolicy`] | The writer syncs | A crash loses at most |
|---|---|---|
| [`SyncPolicy::Interval`], 1 s by default | when blocks wait and the last sync is that long ago | about that long of data |
| [`SyncPolicy::EveryBlocks`] | after every n blocks | n blocks |
| [`SyncPolicy::None`] | only at the close | everything the system has not written yet |

The bytes on the disk end in a [[torn block]], the [[block]] the writer was in the middle of. It ends early or fails
its [[CRC-32C]]. Everything synced before it is whole, because the writer only appends and never rewrites a byte.
A sync policy that writes more often loses less, and each sync costs a wait for the disk. A recorder of a slow, precious
stream syncs after every block; one of a fast stream keeps the default.

## How a reader finds the end

A [[closed]] [[piece]] ends with a [[tail]], and a [[reader]] starts there. The tail leads it to the [[final index]], which
lists every [[block]]. An unclosed piece has neither, so the reader needs another way to its blocks. The [[writer]]
provides one while it writes. Every 4 MiB it places a [[checkpoint]]: an index block that lists the blocks since the
previous checkpoint and points back at it.

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/crash.svg"
<figcaption>A piece that a crash cut off. The reader finds the newest checkpoint near the end, follows the
checkpoints back to list every block before it, and scans the few blocks after it one by one until the torn block
fails its CRC. Drawn by <code>ww_tape_figures</code> from the drive's recording at 1/1000 of its data, cut at minute
37. Its checkpoints come every 3 KiB instead of every 4 MiB.</figcaption>
</figure>

The reader looks backwards from the end of the bytes for the newest checkpoint, which lies at most about 4 MiB back.
It then follows the chain of checkpoints back to the start, and so lists every block before the newest checkpoint
without reading them. Only the blocks after the newest checkpoint need a scan, one by one, and the scan stops at the
[[torn block]]. The scan therefore covers at most about 4 MiB, however long the recording is.

The drive's file reads like this after the crash. [`PieceInfo::cut`] says where the good blocks end:

```rs
--8<-- "ww_tape_tests/tests/it/crashes.rs:read"
```

Everything before that offset reads as it would in a closed file: the [[definitions]], the log lines, the [[CAN]] frames,
the [[summaries]] written so far. The piece has no [[content hash]] to check, because the [[close record]] that would
hold it was never written. Its missing tail is how a reader tells that the file was not closed.

## Resuming the file

When the recorder starts again, it calls [`writer::resume`] with the path and the options it wrote the file with.
The [[resume]] reads the file the way a [[reader]] does. It keeps every byte up to the end of the last good [[block]]
and cuts off the rest, the [[torn end]]. It then replays the kept blocks, so that the [[writer]] knows again what it
wrote. That covers its index, its hashes, and which [[definitions]] and [[state items]] lie close behind.

```rs
--8<-- "ww_tape_tests/tests/it/crashes.rs:resume"
```

The recorder gives its calls again from the first block the file lacks, which [`TapeWriter::next_seq`] names. The
drive's recorder keeps every block it sends, so it sends the lost ones again. A recorder that relays a device's
stream can instead ask the device for the blocks from that point on.

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/resume.svg"
<figcaption>The end of the piece three times: as the writer would have written it, as the crash left it, and resumed.
The resume cut off the torn block and the puts that went with it. The calls given again wrote the same blocks, so
the resumed piece is the same, byte for byte. Drawn by <code>ww_tape_figures</code> from the same scaled recording, and
it checks that the bytes are equal.</figcaption>
</figure>

The resumed file ends with the same bytes as a file written without the crash, and so with the same [[content hash]].
That holds because what the writer adds depends only on the bytes before it, the calls and the options. It needs no
[[clock]] and no memory of what happened before the crash. The test behind this chapter's code cuts the drive's file,
resumes it, and compares it with one written whole. The crate's own tests cut [[pieces]] at every block boundary.

### Puts that go with a lost block

Before a [[block]] of data, the [[writer]] [[puts]] the [[definitions]] and [[state items]] the block needs, when no copy lies close
behind. Such puts belong with the block after them, and the writer marks them with the flag `FLAG_WITH_NEXT` for
that reason. When the crash tears the block, the [[resume]] cuts off its puts too.

The recorder then gives the call for the block again, and the writer writes the same puts and the same block. A put
that finds its [[item]] close behind writes nothing, so the recorder never needs to know which puts reached the disk. It
simply puts again what it uses, as it did before the crash.

### A torn group of puts

Some puts only mean something together. An application that keeps its settings on [[state tracks]] changes a limit
and its unit in one step. A reader that saw the new limit with the old unit would read a wrong setting.
`TapeWriter::put_group` puts such items as one group. Its blocks but the last are marked `FLAG_WITH_NEXT`, as puts
before a block of data are, and the last block of puts ends the group.

A crash inside the group leaves marked blocks at the end of the piece and no last one. A reader of the piece before
the [[resume]] takes none of them, and a follower hands out none. The resume drops them and says so in
`Resumed::dropped_run`, so the application knows the group is lost and gives it again. Every reader then shows either
the settings before the group or all of the group, never a mix. The [format reference](format.md#groups-of-puts)
lists what a writer checks before it writes a group.

## When a resume refuses

A [[resume]] cuts off what a crash leaves and nothing more. A crash leaves a [[block]] that stops short, or zeros from a
sector boundary to the end. A disk writes a 512-byte sector whole or not at all, so nothing else is torn. Anything
else past the good blocks is damage: the resume refuses and leaves the bytes as they are. These cases refuse:

| Refusal | Why |
|---|---|
| [`ResumeError::Damaged`] | the bytes to cut hold damage, not a torn end: a whole block that fails its check |
| [`ResumeError::GoodBlockAfterCut`] | a good block lies past where the reader stopped, so cutting would lose it |
| [`ResumeError::LookSpent`] | the look for good blocks past that point ran out before the end of the bytes |
| [`InputError::Layout`], [`InputError::Producer`] | the options name another layout or producer than the file |

The second case is the one to know. Suppose a sector of the disk went bad in the drive's file a few blocks before the
end, and then the power failed. The [[reader]]'s scan stops at the bad sector, but good blocks follow it, so cutting there
would lose them. The resume names the first good block and refuses. The file still reads past the damage with
[`Recovery::Resync`], as [When bytes go bad](damage.md#reading-past-damage) explains.

A crash after the [[close]] needs no new data. A closed [[piece]] goes on after its close, and a close with nothing written after it adds no byte. A crash in the middle of
the close opens the piece again, and the next close writes the end anew. The [format
reference](format.md#resuming-after-a-crash) tells a [[torn end]] from damage byte by byte and lists the bounds of the look.

## Resuming a chain

The drive's run is long, so the recorder writes it as a [[chain]] of [[pieces]] in a directory, `00000000.wwt`,
`00000001.wwt` and so on. The power failed while it wrote piece 2. On restart the recorder calls
[`chain::resume`] with the directory, the [[owner id]], the chain number and its options.

The chain reads pieces 0 and 1 back from their [[tails]] and reopens `00000002.wwt` as a single file resumes.
The recorder then goes on from the first [[block]] the chain lacks, exactly as before. Two crashes at a piece boundary
resume as well. One comes between closing a piece and creating the next, the other inside the head of the next.

| How the last piece ended | A piece per file | All pieces in one file |
|---|---|---|
| unclosed | resumed | resumed |
| closed naming a next piece: the crash came during a cut | the next piece starts | the next piece starts |
| closed, naming no next piece | written on after its close | written on after its close |
| its head is damaged | refused | refused |

A chain therefore gains a piece only at a [[cut]]. A recorder that opens the chain again goes on in its last piece.

## Trying crashes in a test

A crash is easy to rehearse, because a [[resume]] works on any [[sink]]. A [`MemSink`] keeps a [[piece]] in memory,
and a test writes a piece there and copies a prefix of its bytes into a new sink. It resumes that sink with
[`TapeWriter::resume_on`] and gives the lost calls again. Repeat that for every offset of a small piece, and compare
each result with the piece written whole. The crate tests its resumes this way, and a
[[producer]] can test its own.
