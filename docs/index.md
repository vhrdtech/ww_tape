---
level: basic
---

<!-- The logo is inlined (assets/logo, `just logo`) so the book needs no image files; one of the two shows per
scheme, by the rules of stylesheets/hero.css. -->
<div class="ww-tape-hero ww-tape-hero--light" markdown="0" role="img" aria-label="ww_tape">
--8<-- "assets/logo/ww_tape-light.svg"
</div>
<div class="ww-tape-hero ww-tape-hero--dark" markdown="0" role="img" aria-label="ww_tape">
--8<-- "assets/logo/ww_tape-dark.svg"
</div>

# Tapes

A motor controller board, the drive, runs for an hour on an endurance bench. A host computer beside it records the
board's log lines, its mode, its frames on the vehicle bus and its phase current. This book is about the form that recording takes, a [[tape]]. This chapter says what a tape is, which of its
three uses fits a job, and which crates to take.

## What a tape holds

A tape is a recording of typed values over time. Each value is an [[item]], such as one log line or one current
sample. The items of one kind form a [[track]], and every item has a time on a [[clock]] that the tape defines. The
drive's recording has five tracks:

| Track | What it holds | Rate |
|---|---|---|
| `log` | the firmware's log lines | 3 a second |
| `mode` | the controller's mode: idle, run or fault | at each change |
| `can0` | frames on the Controller Area Network (CAN) bus of the vehicle | 1 000 a second |
| `phase_a` | the phase current in milliamperes | 25 000 a second |
| `temp` | the board's temperature | once a second |

The tracks differ by a factor of ten thousand in rate, and a tape stores each in the form that suits it. Slow tracks
share [[records blocks]], in which every [[record]] carries its own track, time and length. A fast track gets
[[chunks]] of its own, in which the times and values lie in columns. A [[block]] of either kind checks itself, so a
reader can trust it, or skip it, without reading anything else.

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/tape_map.svg"
<figcaption>The drive's hour as a tape. Above, its five tracks; below, the blocks that hold them on the same time
line. Each records block carries the log lines and temperatures of its stretch, and each chunk the frames or samples
of one track. Drawn by <code>ww_tape_figures</code> from the drive's recording at 1/100 of its data, read back from
its pieces. At full rate a block covers a second, not a minute.</figcaption>
</figure>

The `mode` track differs from the other four. Its items are not a stream of events but a [[state]]: the mode in force
until the next one. A reader that opens the recording at minute 40 needs the mode in force there, and the format
makes sure it finds it close by. The section on [state tracks](#state-tracks-and-near-use) below says how.

## One format, three uses

The same blocks serve three uses, and a producer picks the one that fits where the data goes. The format does not
change between them, so a tool that reads one reads all three.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/tape_uses.svg"
<figcaption>The drive's data becomes blocks of one format. The blocks go to a viewer as they are made, into a file on
a PC, or onto a card without a file system.</figcaption>
</figure>

- A [[live feed]] hands each block to a consumer as soon as it exists. A viewer on the bench draws the drive's
  current while the run goes on.
- A file on a personal computer (PC) holds the recording for later. The bench host keeps the drive's hour this way,
  and the next chapter writes such a file.
- A [[raw medium]], such as the Secure Digital (SD) card of a data logger, holds the blocks without a file system. A
  logger in the field writes there, and the reader finds its data again after a power cut.

## What the format is for

Four goals shape the format, and most of its choices follow from them:

- **Typed data.** The tape carries the definitions of its tracks, including the WireWeaver type of their items. A
  reader on another PC, years later, knows what the bytes mean without the device.
- **Exact time.** A time is a whole number of [[ticks]] of a named clock with an exact rate. The format rounds
  nothing to whole nanoseconds, and one tape holds tracks on many clocks.
- **Reads in place.** A reader queries the columns of a chunk where they lie in the file. It decodes only the items
  it needs, never a whole track into a list.
- **Damage stays local.** A crash, a torn write or a flipped bit costs the bytes it touched. A reader reports what it
  lost and reads the rest.

Nothing in the format knows a particular [[producer]]. WireWeaver's link tap records a link into tapes, and a device
logger, a simulator or an analysis program writes the same blocks. [WireWeaver and logring](related_books.md) gives
the background on WireWeaver, the framework for typed APIs that tapes belong to.

## State tracks and near use

A [[state track]] holds items that stay in force, such as the drive's mode. A reader can start anywhere: at minute
40, past a damaged stretch, or on a live feed it joined late. It needs the item in force there, not the whole history
before it.

The writer therefore keeps each state item close behind every place that uses it. The producer gives the writer the
mode at each use, which the book calls a [[put]]. The writer writes the item only when no record of it lies within 32
MiB behind. It also adds one copy, so that one damaged block cannot lose the item.

The definitions of tracks and clocks are state too, kept on track 0 by the same rule. A reader that starts anywhere
thus finds every definition and every state item it needs within about 32 MiB. [Definitions and
state](definitions.md) explains puts, copies and how a reader looks state up.

## Pieces and chains

A long recording does not go into one ever-growing file. The writer cuts it into [[pieces]]: complete files that
each read alone, with an index, a [[close record]] and a [[tail]] at the end. The bench host cuts the drive's hour
every 15 minutes, which gives four pieces of 26 to 28 MiB.

The pieces of one recording form a [[chain]]. Each piece names the piece before it by its content hash, and the next
one by its id. A reader can therefore tell when a piece is missing. Deleting the oldest piece frees space and leaves a chain that
reads as cut at its head, not as damaged.

A closed piece ends on a multiple of 4 KiB. A file system that shares extents, such as btrfs, can therefore
clone pieces back to back into one file without copying a byte. The result is again a valid tape. [Pieces and
chains](pieces.md) explains cuts, stores of pieces and how a chain links.

## The crates

Four crates implement the format. An application that writes files takes `ww_tape` and `ww_tape_fs`, and one that
draws summaries adds `ww_tape_summary`.

| Crate | What it holds |
|---|---|
| `ww_tape` | the format: blocks, definitions, chunks, the index, pieces, the writer [`TapeWriter`] and the reader [`TapeReader`]; it builds without the standard library |
| `ww_tape_codecs` | the column codecs, which know nothing of blocks: bit packing, checksums, varints, lz4, and the codecs of samples, times and lengths; `ww_tape` re-exports them |
| `ww_tape_fs` | everything that names a path: the file sink [`FileSink`], the reader's [`FileSource`], and writers and chains opened by path |
| `ww_tape_summary` | the trees of summaries a viewer draws a long track from |

## How to read this book

The book has six parts in the order a reader needs them. Each part lists its basic chapters first; a badge above each
title gives its [reading level](levels.md). Read the basic chapters first and stop when you have what you need.

The running example is the drive's recording, and the crate `ww_tape_example` writes it. Every figure that shows
bytes draws from it, and every number in the story comes from running it. The format reference at the end lists every
block byte by byte, for a reader in another language.

The next chapter, [A first tape](first_tape.md), writes the drive's first minute to a file and reads it back.
