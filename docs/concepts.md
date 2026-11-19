# Tape concepts

A **tape** is a recording of typed values over time. This page names every part of a tape, and shows where each part
sits in a file.

Read it first: the other pages of this book use these words without explaining them again. The page assumes no
knowledge of the format and none of the `ww_tape` code.

A program that makes a tape is a **producer**, such as a link tap, a device logger, a simulator or an analysis tool. A
program that reads a tape is a **reader**.

A tape can travel as a live feed, sit in a file on a personal computer (PC), or lie on a raw medium. A Secure Digital
(SD) card is one such raw medium.

The `ww_tape` crate writes and reads all three.

## Items, tracks and clocks

An **item** is one value: a received frame, a temperature reading, a log line. Every item is a WireWeaver value, stored
in its shrink_wrap bytes.

[WireWeaver and logring](related_books.md) explains WireWeaver types and shrink_wrap.

A **track** is a sequence of items of one type, such as "frames received on link 2" or "phase current A". Inside a
file, a track has a small **track number**.

Across files, a track has a stable **track key**: a 64-bit hash of where the track comes from in the producer's API.
Two files with data of the same sensor give it the same key, even when they give it different numbers.

A track may also carry **dimensions**, name and value pairs such as `channel = 3`, which tell apart tracks of one key.
The **track id** is a 64-bit hash of the key and the dimensions.

A track without dimensions has its key as its id.

### Clocks and records

A **clock** is anything that counts time: the host's nanosecond clock, a device's microsecond timer since boot, an
audio codec's sample clock. Every time in a tape is a whole number of **ticks** of one clock, and every track names
the clock its times count.

A clock's rate is exact. A 48 kHz audio clock and a 60000/1001 Hz video clock therefore both keep exact times.

A **record** is one item on one track at one tick. [Records, tracks and clocks](records_and_clocks.md) explains
clocks, their origins and the maps between them.

## Records blocks and chunks

A tape is a sequence of **blocks**, written one after another. Every block has a 32-byte header, a body, and a cyclic
redundancy check (CRC-32C) at its end.

The header gives the block's kind and length. A reader can therefore skip a block it doesn't understand.

Two kinds of block hold items, and they differ in how many tracks they mix:

| | Records block | Chunk |
|---|---|---|
| Tracks | any number, mixed | exactly one |
| Items | records back to back, each with its own time and length | a run of consecutive items, stored column by column |
| Suits | a few items at a time: a log line, a state change | many items: a 1 kHz event stream, a 25 kHz current |
| Finding one item | walk the block from its start | jump to it by its item number and read it in place |

A **records block** is cheap to write, and is complete as soon as it holds one record. A slow track therefore never
waits to fill anything.

A **chunk** pays off when a track has many items. Its columns pack times, flags and values tightly, and a reader
reaches item `i` without decoding the items before it.

A chunk names its track by its track id. There are two kinds of chunk.

An **events chunk** holds items that have times of their own. A **series chunk** holds samples at a fixed rate, which
need no time each.

[Events and series](events_and_series.md) explains both kinds of chunk.

### Item numbers and examples

Each item in a chunk also has an **item number**, which counts the track's items from 0 over the whole recording,
across files.

Item 1 000 000 of a track is the same item in every copy of the recording. A series numbers its samples the same way.

A firmware's log lines, for example, arrive a few per second. They go into records blocks together with other slow
tracks.

A 25 kHz phase current fills series chunks one after another. A viewer reads any range of its samples without
touching the rest.

Other kinds of block hold no items:

- A **summary** block holds a coarse view of a long track, such as the minimum and maximum of every 1024 samples. A
  viewer draws an hour of data from summaries, without reading the samples.
- An **index** block lists where other blocks are. Index entries name their track by its track id.
- A **piece head**, a **finish** and a **tail** mark where a piece begins and ends. [Sessions, pieces and
  chains](sessions_and_pieces.md) explains them.

The crate `ww_tape_summary` builds and reads summaries.

## Definitions on track 0

Before a track's items mean anything, a reader needs to know what the track is. It needs the clock the track counts,
the type of its items, and the API that type comes from.

These facts are **definitions**. A definition describes one of these:

- the producer;
- the layout of the file;
- an API bundle;
- a clock or a clock map;
- a track;
- a typed item, such as a run record.

Definitions are records on **track 0**, which every tape has and which holds only definitions. Each definition fills a
**slot** that follows from the definition itself.

A track's slot is its track number, a clock's slot its clock number, and a bundle's slot its hash. A newer definition
in the same slot replaces the older one for every reader after it.

A track added an hour into a recording therefore costs one record, however many tracks the file already has.

The definitions belong to the application, not to the file. The application keeps the definitions of what it works on
in a `Definitions` container, and passes the container with every write.

The writer places each definition in the file just before the first data that needs it. [Writing a
tape](writing.md#what-the-application-does-with-its-definitions) shows how.

## State, near and copies

Most tracks are streams of events: a frame received at one tick says nothing about the next tick. Some tracks hold
**state** instead: an item that stays in force until the next one replaces it.

A link's session parameters and the run a recorder is in are state. Definitions are state too, on track 0.

### Why a reader needs state close behind

A reader needs the item in force at a place to understand the data after it. A reader that reads a file from its start
always knows the state, because it has seen every record.

A reader that starts in the middle can look backwards for the newest record of the track. That works only when the
record lies in bytes the reader has and can read.

A record written once at the start of a week-long file lies a week of bytes away. Three readers fail even with
unlimited time:

1. A reader of a file whose start is damaged: the only record of the state may be in the damaged bytes.
2. A reader of one piece of a long recording, without the pieces before it: the record is in another file.
3. A reader that joins a live feed after it started: there is nothing behind it.

### Near and copies

The format therefore keeps every state item close to its use. A track with the state policy (`TrackRepeat::State`) has
two numbers:

- **Near** is how far behind a use the item must lie in the same piece, 32 MiB by default.
- `copies` is how many extra records an item gets at later uses, as a guard against damage around one place. The
  default is one copy, at least 1 MiB after the first record.

Each time the producer uses a state item, it **puts** the item: it calls `TapeWriter::put` with it. The writer
compares the item by its hash with the newest record of its slot.

When the same item lies less than `near` behind, the put writes nothing. Otherwise the put writes the item again, and a
reader at this place finds it close behind.

The writer puts definitions the same way before every block of data. A put is idempotent: the producer needs no record
of what it put before.

For example, a recorder puts a link's session before each block of that link's frames. The first put writes the
session.

The puts after it write nothing, except for one copy at a use 1 MiB or more after the first record. A put more than
32 MiB after the newest record writes the session again.

A reader that starts anywhere finds the session within 32 MiB behind it. One damaged block never takes the only record
of it.

### The rule at work

The drawing shows the rule at work, and `ww_tape_figures` draws it from the blocks `TapeWriter` wrote. Its writer runs
with `near` shrunk to 8 KiB and the copy spacing to 2 KiB, so that a whole cycle fits on one line.

With the defaults of 32 MiB and 1 MiB, the near span is 4096 times longer and the copy spacing 512 times longer. The
writer puts track 0's definitions the same way, by the layout's own policy.

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/puts.svg"
<figcaption>A state track put before each of 26 blocks of data, of which only four puts write. They are the first, one
copy 2 KiB on, and the item again each time the newest record lies more than <code>near</code> behind.</figcaption>
</figure>

A record written by a put or as a copy goes into a block that carries the flag `FLAG_REPEAT` in its header.
A reader that already holds the same item in that slot takes the record as nothing new.

The writer never writes a state item ahead of a use. A track whose data has stopped therefore costs nothing until its
data continues.

A series track shows where the line between state and data runs: it holds no state at all. Each series chunk carries
its own **grid**, which gives the tick of one sample and the scale of the stored numbers.

The chunk places its samples in time alone, and nothing needs to stay near it. Series chunks are data: the writer
writes each one once, and never repeats it.

## Where to go next

- [Sessions, pieces and chains](sessions_and_pieces.md): how blocks group into sessions, pieces and chains, cuts, and
  one file from start to end.
- [Writing a tape](writing.md): the writer, sinks, definitions and puts.
- [Syncing, cuts and relays](syncing_and_cuts.md): syncing, checkpoints, cuts and relays.
- [Resuming a writer](resuming.md): resuming after a crash.
- [Pieces and files](pieces_and_files.md): chains, many pieces in one file, copies, and addressing one item.
- [Reading a tape](reading.md): opening a file, finding blocks, state in force at a place, damage.
- [Events and series](events_and_series.md): how chunks store time and values.
- [Numbers](numbers.md): sizes and speeds, and which codec to choose.
- [Damage and recovery](recovery.md): what survives a crash, a torn write or a bad sector.
- [Format reference](format.md): every part of the format, block by block.
- [Tapes and logring](https://vhrdtech.github.io/logring/tapes_and_logring/), in the logring book: when a device's own
  log store is the better choice.
