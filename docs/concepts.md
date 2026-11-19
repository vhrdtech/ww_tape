# Tape concepts

This page names every part of a tape and shows where each part sits in a file. Read it first: the other pages of this
book use these words without explaining them again. The page assumes no knowledge of the format and none of the
`ww_tape` code.

A **tape** is a recording of typed items over time. A program that makes a tape is a **producer**, such as a link
tap, a device logger, a simulator or an analysis tool. A program that reads a tape is a **reader**. The same tape can
travel as a live feed, sit in a file on a PC, or lie on a raw medium such as an SD card. The `ww_tape` crate writes
and reads all three.

## Items, tracks and clocks

An **item** is one value: a received frame, a temperature reading, a log line. Every item is a WireWeaver value,
stored in its shrink_wrap bytes. [WireWeaver and logring](related_books.md) explains WireWeaver types and shrink_wrap.

A **track** is a sequence of items of one type, such as "frames received on link 2" or "phase current A". Inside a file
a track has a small **track number**. Across files it has a stable **track key**, a 64-bit hash of where the track
comes from in the producer's API. Two files that record the same sensor give it the same key, even when they give it
different numbers.

A **clock** is anything that counts time: the host's nanosecond clock, a device's microsecond timer since boot, an
audio codec's sample clock. Every time in a tape is a whole number of **ticks** of one clock, and every track names the
clock its times count. A clock's rate is exact, so a 48 kHz audio clock and a 60000/1001 Hz video clock both keep
exact times. [Tapes](index.md#records-tracks-and-clocks) explains clocks, their origins and the maps between them.

A **record** is one item on one track at one tick. The items of a track stored in chunks (see below) also have an
**item number**, which counts the track's items from 0 over the whole recording, across files. Item 1 000 000 of a
track is the same item in every copy of the recording. A series numbers its samples the same way.

## Record blocks and chunks

A tape is a sequence of **blocks**, written one after another. Every block has a 32-byte header, a body and a CRC-32C
checksum at its end. The header gives the block's kind and length, so a reader can skip a block it doesn't understand.

Two kinds of block hold items, and they differ in how many tracks they mix:

| | Record block | Chunk |
|---|---|---|
| Tracks | any number, mixed | exactly one |
| Items | records back to back, each with its own time and length | a run of consecutive items, stored column by column |
| Suits | a few items at a time: a log line, a state change | many items: a 1 kHz event stream, a 25 kHz current |
| Finding one item | walk the block from its start | jump to it by its item number and read it in place |

A **record block** is cheap to write and is complete as soon as it holds one record, so a slow track never waits to
fill anything. A **chunk** pays off when a track has many items. Its columns pack times, flags and values tightly, and
a reader reaches item `i` without decoding the items before it.

There are two kinds of chunk. An **events chunk** holds items that have times of their own. A **series chunk** holds
samples at a fixed rate, which need no time each. [Events and series](events_and_series.md) explains both.

For example, a firmware's log lines arrive a few per second, so they go into record blocks together with other slow
tracks. A 25 kHz phase current fills series chunks one after another, and a viewer reads any range of its samples
without touching the rest.

Other kinds of block hold no items:

- A **summary** block holds a coarse view of a long track, such as the minimum and maximum of every 1024 samples. A
  viewer draws an hour of data from summaries without reading the samples. The crate `ww_tape_summary` builds and
  reads summaries.
- An **index** block lists where other blocks are.
- A **piece head**, a **finish** and a **tail** mark where a piece begins and ends (see below).

## Definitions on track 0

A reader needs to know what a track is before its items mean anything: which clock it counts, what type its items
have, which API that type comes from. These facts are **definitions**. A definition describes the producer, the layout
of the file, an API bundle, a clock, a clock map, a track, or a typed item such as a run record.

Definitions are records on **track 0**, a track that every tape has and that is reserved for them. Each definition
fills a **slot** that follows from the definition itself. A track's slot is its track number, a clock's slot its clock
number, and a bundle's slot its hash. A newer definition in the same slot replaces the older one for every reader after
it. A track added an hour into a recording therefore costs one record, however many tracks the file already has.

The definitions belong to the application, not to the file. The application keeps the definitions of what it works on
in a `Definitions` container and passes the container with every write. The writer puts each definition into the file
just before the first data that needs it. [Writing a tape](writing.md#what-the-application-does-with-its-definitions)
shows how.

## State, near and copies

Most tracks are streams of events: a frame received at one tick says nothing about the next tick. Some tracks hold
**state** instead: an item that stays in force until the next one replaces it. A link's session parameters and the run
a recorder is in are state. A reader needs the item in force at a place to understand the data after it. Definitions
are state too, on track 0.

A reader that reads a file from its start always knows the state, because it has seen every record. A reader that
starts in the middle can look backwards for the newest record of the track. That works only when the record lies in
bytes the reader has and can read, and a record written once at the start of a week-long file lies a week of bytes
away. Three readers fail even with unlimited time:

1. A reader of a file whose start is damaged: the only record of the state may be in the damaged bytes.
2. A reader of one piece of a long recording, without the pieces before it: the record is in another file.
3. A reader that joins a live feed after it started: there is nothing behind it.

The format therefore keeps every state item **near its use**. A track with the state policy (`TrackRepeat::State`) has
two numbers. **Near** is how far behind a use the item must lie in the same piece, 32 MiB by default. **Copies** is how
many extra records an item gets at later uses, as a guard against damage around one place. The default is one copy, at
least 1 MiB after the first record.

The producer **puts** a state item each time it uses the item (`TapeWriter::put`). The writer compares the item by its
hash with the newest record of its slot. When the same item lies less than `near` behind, the put writes nothing.
Otherwise the put writes the item again, so a reader at this place finds it close behind. The writer puts definitions
the same way before every block of data. A put is idempotent, so the producer needs no record of what it put before.

For example, a recorder puts a link's session before each block of that link's frames. The first put writes the
session. The puts after it write nothing, except for one copy at a use 1 MiB or more after the first record. A put more
than 32 MiB after the newest record writes the session again. A reader that starts anywhere finds the session within
32 MiB behind it, and one damaged block never takes the only record of it.

The drawing shows the rule at work. Its writer runs with `near` shrunk to 8 KiB and the copy spacing to 2 KiB, so that
a whole cycle fits on one line. With the defaults of 32 MiB and 1 MiB, the near span is 4096 times longer and the copy
spacing 512 times longer.

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/puts.svg"
<figcaption>A state track put before each of 26 blocks of data. Only four puts write: the first, one copy 2 KiB on,
and the item again each time the newest record lies more than <code>near</code> behind. Track 0's definitions are put
the same way, by the layout's own policy. Drawn by <code>ww_tape_figures</code> from the blocks
<code>TapeWriter</code> wrote.</figcaption>
</figure>

A record written by a put or as a copy goes into a block marked with the header flag `FLAG_REPEAT`. A reader that
already holds the same item in that slot takes the record as nothing new. A state item is never written ahead of a
use, so a track whose data has stopped costs nothing until its data continues.

A series track shows where the line between state and data runs: it holds no state at all. Each series chunk carries
its own **grid**, which gives the tick of one sample and the scale of the stored numbers. The chunk places its samples
in time alone, and nothing has to be kept near it. Series chunks are data: each is written once and never repeated.

## Sessions, pieces, chains and owners

Four words name four different groupings of blocks. They are easy to confuse, so each gets an example.

A **session** is one writer's unbroken run of appends, from a producer's start to its stop or from a device's boot to
its power-off. Every block of a session carries the same 32-bit **session tag** in its header. It also carries a
**sequence number** that counts the session's blocks from 0, so a gap in the sequence numbers says how many blocks were
lost. The session tag tells a reader which blocks belong to the current recording and which are stale leftovers of an
older one on the same SD card.

A **piece** is a complete file in the tape sense. It holds a piece head, the data with its definitions and state near
it, and at the end a final index, a finish block and a tail. A piece reads alone, because near is measured inside the
piece: every definition that a piece's data needs lies in that piece. A long recording is cut into several pieces so
that each piece stays a bounded size.

A **chain** is the pieces of one recording, in order. Each piece head names the piece before it by its content hash,
so a reader can check that no piece is missing or swapped.

An **owner id** is a 32-byte id in every piece head, finish and tail. It names what the pieces belong to. `ww_tape`
doesn't interpret it; the application gives it a meaning. One owner may have several chains, such as one for events
and one for a high-rate stream.

An application may add groupings of its own on top. For example, a recorder application can keep a **project**:
something the user creates, opens and closes, which holds many runs of many producers. The recorder can write each
stretch from open to close as a new session in a new piece, and give every piece the project's id as its owner id. The
project and its runs are the application's words; `ww_tape` sees only sessions, pieces, chains and an opaque owner id.

| Word | Who decides it | Example |
|---|---|---|
| session | the writer, at each start | one boot of a data logger |
| piece | the writer, by size or by hand | `00000003.wwt`, about 256 MiB of one recording |
| chain | the pieces of one recording | pieces 0 to 7 of one capture |
| owner id | the application | a project's id |

## Cuts

A **cut** ends the current piece and starts the next piece of the chain. The writer writes the piece's final index,
finish and tail, then a new piece head. In the new piece, the writer puts the layout and the producer first. It puts
every definition and state item again at its first use, since nothing in the old piece counts as near.

A chain cuts by its policy (size, chunk count or age) only before a chunk. It also cuts before a write for which the
piece's index has no room left, and wherever the application asks.

A slow input never waits for a cut. Records reach the file in record blocks as they come, and a reader of the
unfinished piece reads them at once. A **sync policy** bounds what a crash can lose. By default the writer syncs
waiting blocks once a second has passed since the last sync, at the next block or at a call of `TapeWriter::tick`. A
cut only bounds how large one file grows. For example, a producer that writes one log line a minute and calls `tick`
regularly has each line on disk about a second later, whether the piece is cut after an hour or after a week.

## Index, finish and tail

While a piece is written, the writer places a **checkpoint**, an index block, about every 4 MiB. Each checkpoint lists
the blocks placed since the one before and points back at it. A reader of a file that was never closed finds the
newest checkpoint near the end, and so knows every block before it without reading them.

At the end of a piece the writer places a **final index**, which lists every block sorted by track, then the
**finish** block. The finish holds a BLAKE3 hash of every byte before it, per-track totals, and where the final index
is. Last comes the **tail**, 164 bytes that give the piece's length and where its finish is. A finished piece is a
multiple of 4 KiB long, and the finish block is padded with zeros to make it so.

## One file, start to end

The diagram shows one finished piece, with its start at the top.

```
 piece start ─► ┌─────────────────────────────┐
                │ piece head                  │ ids, the previous piece's hash
                ├─────────────────────────────┤
                │ puts           (track 0)    │ layout and producer
                ├─────────────────────────────┤
                │ puts           (track 0)    │ clocks, maps, tracks 1-3
                │ events chunk   (track 3)    │ first data block
                │ records        (tracks 1,2) │ small tracks, many per block
                │ puts           (track 0)    │ track 5's definition
                │ series chunk   (track 5)    │
                │ puts           (track 0, 6) │ track 6's definition and state
                │ summary        (track 3)    │
                │ ...                         │
                │ copies         (track 0, 6) │ at a use 1 MiB after the first
                │ index (checkpoint) ◄──┐     │ every 4 MiB: the blocks placed
                │ ...                   │     │ since the previous checkpoint,
                │ index (checkpoint) ───┘     │ and a pointer back to it
                │ ...                         │
                │ puts           (track 0)    │ a definition 32 MiB on, still
                │ records        (track 1)    │ in use
                │ ...                         │
                ├─────────────────────────────┤
                │ final index    ◄──────┐     │ every block again, by track
                │ finish         ◄──┐   │     │ BLAKE3 hash of all bytes before it,
                │                   │   │     │ zero padding to the 4 KiB end
                │ tail (164 B) ─────┴───┘     │ piece length, where the finish is
   end of file ─► └─────────────────────────────┘ a multiple of 4 KiB from the start
```

A reader of this file starts at the bottom. It reads the tail, then the finish, then the part of the final index for
the tracks it wants, and then only the blocks it needs. [Reading a tape](reading.md) follows that path.

The figure below draws a small piece of 12 KiB to scale and shows how its end falls on the 4 KiB grid. The final
index, the finish and the tail take the bytes after the data, and the finish's zero padding fills the rest up to the
boundary.

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/piece.svg"
<figcaption>A finished piece of a chain, each block to scale on rows of 1 KiB: four records blocks and four events
chunks with the puts they need, two checkpoints, the final index, the finish with its padding, and the 164-byte tail
ending on 12 KiB. Drawn by <code>ww_tape_figures</code> from a piece <code>PieceChain</code> wrote.</figcaption>
</figure>

## Addressing one item

An item is addressed by its track key and its item number, never by a byte offset. The track key names the track in
every file of the recording, whatever number the track has there. The item number counts the track's items over the
whole recording, and each piece head says where each track's numbers continue.

Offsets change when pieces are joined into one file, or when a file is copied with only some of its tracks; the address
doesn't. [Pieces and files](pieces_and_files.md#addressing-one-item) explains how a reader turns the address into
bytes.

## Nothing is filled in later

A writer never goes back to change bytes it already wrote. Every block is final once placed, and the file only grows.
This rule gives the format its shape:

- The piece's length and the place of its index are in the tail at the end, not in a header field at the start.
- Checkpoints point back at the checkpoint before them, never forward at one not yet written.
- A tail can't name the next piece's hash, which doesn't exist yet. It names the next piece's id instead, derived from
  its own id and hash, and the next piece head repeats that id.
- A new definition is a new record. The old one stays in the file and is replaced only for readers after the new one.
- A copy of a state item is written at a use, never scheduled ahead, so nothing is left pending when the writer stops.
- A crash costs only the bytes not yet synced. No header exists that would be wrong after it.

Append-only writing suits every medium the format targets. A NAND flash page takes one program, an SD card is fastest
in long sequential writes, and a file that is only appended can be copied while it grows.

## Where to go next

- [Writing a tape](writing.md): the writer, sinks, definitions and puts, syncing, cuts, relays and resuming after a
  crash.
- [Pieces and files](pieces_and_files.md): chains, many pieces in one file, copies, and addressing one item.
- [Reading a tape](reading.md): opening a file, finding blocks, state in force at a place, damage.
- [Events and series](events_and_series.md): how chunks store time and values.
- [Numbers](numbers.md): sizes and speeds, and which codec to choose.
- [Damage and recovery](recovery.md): what survives a crash, a torn write or a bad sector.
- [Format reference](format.md): every part of the format, block by block.
- [Tapes and logring](https://vhrdtech.github.io/logring/tapes_and_logring/), in the logring book: when a device's own
  log store is the better choice.
