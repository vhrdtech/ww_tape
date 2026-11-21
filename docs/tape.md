---
level: basic
---

# What a tape holds

A [[tape]] is a recording of typed values over time. Each value is an [[item]], such as one log line or one current
[[sample]]. The items of one kind form a [[track]], and every item has a time on a [[clock]] that the tape defines.

The book uses one recording as its example throughout. A motor controller board, the drive, runs for an hour on an
endurance bench, and a host computer beside it records the board. The recording has five tracks:

| Track | What it holds | Rate | Kept in |
|---|---|---|---|
| `log` | the firmware's log lines | 3 a second | records blocks |
| `mode` | the controller's mode: idle, run or fault | at each change | a state track |
| `can0` | frames on the CAN bus of the vehicle | 1 000 a second | events chunks |
| `phase_a` | the phase current, in milliamperes as 16-bit fixed point | 25 000 a second | series chunks |
| `temp` | the board's temperature | once a second | records blocks |

The tracks differ by a factor of ten thousand in rate, and a tape keeps each in the form that suits it. Slow tracks
share [[records blocks]], in which every [[record]] carries its own track, time and length. A fast track gets
[[chunks]] of its own, in which the times and values lie in [[columns]]. A [[block]] of either kind checks itself, so
a [[reader]] can trust it, or skip it, without reading anything else.

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/tape_map.svg"
<figcaption>The drive's hour as a tape. Above, its five tracks; below, the blocks that hold them on the same time
line. Each records block carries the log lines and temperatures of its stretch, and each chunk the frames or samples
of one track. Drawn by <code>ww_tape_figures</code> from the drive's recording at 1/100 of its data, read back from
its pieces. At full rate a block covers a second, not a minute.</figcaption>
</figure>

The `mode` track differs from the other four. Its items are not a stream of [[events]] but a [[state]]: the mode in
force until the next one. A reader that opens the recording at minute 40 needs the mode in force there, and the
format makes sure it finds that mode nearby. The section [below](#state-tracks-and-near-use) says how.

## One format, three uses

The same [[blocks]] serve three uses, and a [[producer]] picks the one that fits where the data goes. The format does
not change between them, so a tool that reads one reads all three.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/tape_uses.svg"
<figcaption>The drive's data becomes blocks of one format. The blocks go to a viewer as they are made, into a file on
a PC, or onto a card without a file system.</figcaption>
</figure>

- A [[live feed]] hands each block to a consumer as soon as it exists. A viewer on the bench draws the drive's
  current while the run goes on.
- A file on a PC holds the recording for later. The bench host keeps the drive's hour this way,
  and the next chapter writes such a file.
- A [[raw medium]], such as an [[SD]] card, holds the blocks without a file system. A [[reader]]
  finds the data again after a power failure. The recorder that writes there has a host-class processor and RAM. A
  recorder that is itself a small device takes [logring](related_books.md#logring-a-log-store-on-the-device) instead.

## What the format is for

Four goals shape the format, and most of its choices follow from them:

- **Typed data.** The [[tape]] carries the [[definitions]] of its [[tracks]], including the WireWeaver type of their
  [[items]]. A [[reader]] on another PC, years later, knows what the bytes mean without the device.
- **Exact time.** A time is a whole number of [[ticks]] of a named [[clock]] with an exact rate. The format rounds
  nothing to whole nanoseconds, and one tape holds tracks on many clocks.
- **Reads in place.** A reader queries the [[columns]] of a [[chunk]] where they lie in the file. It decodes only the
  items it needs, never a whole track into a list.
- **Damage stays local.** A crash, a torn write or a flipped bit costs the bytes it touched. A reader reports what it
  lost and reads the rest.

Nothing in the format knows a particular [[producer]]. WireWeaver's link tap writes the [[records]] of a link into
tapes, and a device logger, a simulator or an analysis program writes the same [[blocks]]. [WireWeaver and
logring](related_books.md) gives the background on WireWeaver, the framework for typed APIs that tapes belong to.

## State tracks and near use

A [[state track]] holds [[items]] that stay in force, such as the drive's mode. A [[reader]] can start in three
places where the history before it is out of reach:

- at minute 40 of a [[closed|close]] recording, without reading the 40 minutes before;
- past a damaged stretch, which it cannot read;
- on a [[live feed]] it joined late, whose start it never saw.

The reader needs the item in force there, not the whole history before it. The [[writer]] therefore keeps each
[[state item|state]] close behind every place that uses it. The [[producer]] gives the writer the mode at each use,
which the book calls a [[put]]. The writer writes the item only when no [[record]] of it lies within 32 MiB behind.
It also adds one [[copy|state copy]], so that one damaged [[block]] cannot lose the item.

The [[definitions]] of [[tracks]] and [[clocks]] are state too, kept on track 0 by the same rule. A reader that starts
anywhere thus finds every definition and every state item it needs within about 32 MiB. [Definitions and
state](definitions.md) explains puts, copies and how a reader looks state up.

## Pieces and chains

A long recording does not go into one ever-growing file. At each [[cut]], the [[writer]] ends one [[piece]] and
starts the next. A piece is a complete file that reads alone, with an index, a [[close record]] and a [[tail]] at the
end. The bench host cuts the drive's hour every 15 minutes, which gives four pieces of about 27 MB to 30 MB.

The pieces of one recording form a [[chain]]. Each piece names the piece before it by its [[content hash]], and the
next one by its id. A [[reader]] can therefore tell when a piece is missing. Deleting the oldest piece frees space,
and a reader tells a chain that lost its first pieces this way apart from a damaged one.

A closed piece ends on a multiple of 4 KiB. A file system that shares extents, such as btrfs, can therefore clone
pieces back to back into one file without writing any byte again. The result is a valid [[tape]] too. [Pieces and
chains](pieces.md) explains cuts, the [[stores]] that hold pieces and how a chain links.

## The crates

Four crates implement the format. An application that writes files takes `ww_tape` and `ww_tape_fs`, and one that
draws [[summaries]] adds `ww_tape_summary`.

| Crate | What it holds |
|---|---|
| `ww_tape` | the format: blocks, definitions, chunks, the index, pieces, the writer [`TapeWriter`] and the reader [`TapeReader`]; it builds without the standard library |
| `ww_tape_codecs` | the column codecs, which know nothing of blocks: bit packing, checksums, varints, lz4, and the codecs of samples, times and lengths; `ww_tape` re-exports them |
| `ww_tape_fs` | everything that names a path: the file sink [`FileSink`], the reader's [`FileSource`], and writers and chains opened by path |
| `ww_tape_summary` | the trees of summaries a viewer draws a long track from |

ww_tape runs on a host. Its [[writer]] needs an allocator and keeps the index of the open [[piece]] in memory. A piece
of 256 MiB in [[records blocks]] of 4 KiB takes its heap to a peak of about 44 MB, as `just bench-memory` measures.
ww_tape spends that memory on many [[tracks]], an index, summaries, column codecs and chains of pieces.

Firmware on a small device keeps its log in [logring](https://lr.vhrd.tech/) instead. That store writes a ring on an SD
card or a flash chip with no heap and little RAM. A power cut costs it at most the last second.
[Tapes and logring](https://lr.vhrd.tech/tapes_and_logring/) compares the two.
[WireWeaver and logring](related_books.md) introduces logring.
