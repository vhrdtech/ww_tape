<!-- The logo is inlined (assets/logo, `just logo`) so the book needs no image files; one of the two shows per
scheme. -->
<style>
.md-typeset .ww-tape-hero { margin: 0.5rem 0 1.5rem; text-align: center; }
.md-typeset .ww-tape-hero svg { width: min(420px, 90%); height: auto; }
[data-md-color-scheme="default"] .md-typeset .ww-tape-hero--dark,
[data-md-color-scheme="slate"] .md-typeset .ww-tape-hero--light { display: none; }
</style>
<div class="ww-tape-hero ww-tape-hero--light" markdown="0" role="img" aria-label="ww_tape">
--8<-- "assets/logo/ww_tape-light.svg"
</div>
<div class="ww-tape-hero ww-tape-hero--dark" markdown="0" role="img" aria-label="ww_tape">
--8<-- "assets/logo/ww_tape-dark.svg"
</div>

# Tapes

A **tape** is a recording of typed data over time: what a device streamed, what crossed a link, what a program
computed from either. The same format serves three uses. A live feed hands blocks to a consumer as they are made, a
file on a PC holds a recording, and a raw medium such as the SD card of a data logger holds one without a file system.
This book explains the format to readers who have not seen its code: what a file holds, how time is stored, and what
survives when something goes wrong.

Tapes belong to WireWeaver, a framework for typed APIs between devices and hosts. Every item on a tape is a value of a
WireWeaver type. [WireWeaver and logring](related_books.md) introduces WireWeaver and logring, a log store for
devices, for readers who need that background.

## What the format is for

The format has four goals, and most of its choices follow from them:

- **Typed data.** Every item is a WireWeaver type, and the file carries the API bundle that defines it. A reader on
  another PC, years later, knows what the bytes mean without the device.
- **Exact time.** Times are integer ticks of a named clock with an exact rate. Nothing is rounded to whole nanoseconds,
  and one file holds tracks on many clocks.
- **Reads in place.** Large tracks are stored in columns that a reader queries as they lie in the file, without decoding
  them into a list first.
- **Damage stays local.** A crash, a torn write or a flipped bit costs the bytes it touched, and a reader says what was
  lost.

Nothing in the format knows any particular producer. WireWeaver's link tap records a link into tapes, and a device
log, a simulator or an analysis program writes the same blocks.

## The crates

Four crates implement the format. An application that writes files takes `ww_tape` and `ww_tape_fs`; one that reads
or draws summaries adds `ww_tape_summary`.

| Crate | What it holds |
|---|---|
| `ww_tape` | the format: blocks, definitions, chunks, the index, pieces, the writer (`TapeWriter`, `PieceChain`) and the reader (`TapeReader`); it builds without the standard library |
| `ww_tape_codecs` | the column codecs, which know nothing of blocks: bit packing, CRC-32C, varints, lz4, sample, time and length codecs; `ww_tape` re-exports them |
| `ww_tape_fs` | everything that names a path: the file sink `FileSink`, the store of piece files `Files`, the file reader `FileSource`, and writers and chains opened by path |
| `ww_tape_summary` | summary trees: `EventTree` and `SeriesSummaries` build summary blocks, and `Summary::of_block` reads their nodes |

`ww_tape` itself checks a summary block and reads its head, so its index lists the block without decoding any node.

## How to read this book

A reader new to the format starts with [Tape concepts](concepts.md), which names every part of a tape and shows one
file from start to end. This page then gives an overview of the whole format: records, clocks, blocks, definitions and
the anatomy of a file. The other pages go deeper, in the order a reader needs them:

- [Tape concepts](concepts.md): the words, and one picture of a file.
- [Writing a tape](writing.md): the writer, definitions and puts, syncing, cuts, relays and resuming after a crash.
- [Pieces and files](pieces_and_files.md): chains, many pieces in one file, copies, and addressing one item.
- [Reading a tape](reading.md): opening a file, the index, state at a place, damage and salvage.
- [Events and series](events_and_series.md): event chunks, the time codec, series and summaries.
- [Numbers](numbers.md): sizes and speeds, and why fixed point comes first.
- [Damage and recovery](recovery.md): checksums, crashes, raw media and the limits of the format.
- [Format reference](format.md): every part of the format, block by block.

The logring book compares a tape with a device's log store in [Tapes and
logring](https://vhrdtech.github.io/logring/tapes_and_logring/). The exact byte layouts are in the module docs of the
`ww_tape` crate, linked from each section. This book explains those layouts and does not repeat them.

## Records, tracks and clocks

A **record** is one item on one track at one time. A **track** is a named sequence of items of one type: the frames a
link received, the temperature readings of a sensor, the log lines of a firmware. A track's definition (`TrackDef`)
gives the track a small number used inside the file and a stable key that names the same track across files. It also
names the WireWeaver resource path the items come from and the clock that their times count.

A **clock** is anything that counts time: the host's nanosecond clock, a device's microsecond timer since boot, an
audio codec's sample clock. A clock's definition (`ClockDef`) gives an exact rate in ticks per second, an exact
resolution, and an **origin**: what tick 0 is. The origin is an identity, never a number. It is a device's boot, the
Unix epoch, the start of a media file, the start of the producer's session, or nothing known (a free-running counter).

A **tick** is one step of a clock, and every record time is a signed 64-bit count of ticks. Rates are exact, so a clock
that does not run at a whole number of hertz stays exact. For example, a video at 60000/1001 frames per second lives on
its stream's 60 000 Hz time base. Each frame lasts 1001 ticks, and every frame time is exact.

### Clocks that don't share a time base

Two clocks are related only by a **clock map**. A map is a list of map segments, and each segment says
`b = b0 + (a - a0) · slope` over a range of clock `a`, with an error bound in ticks of clock `b`. The slope is a reduced
fraction, so a fitted drift such as 1000000007/999999931 stays exact.

A map must increase strictly across all its segments, or the order of items would flip between the clocks. A forward
jump of the wall clock, such as a step from a time server, starts a new map segment. A backward step is a gap or a new
clock.

Wall time is one more clock, with the Unix epoch as its origin, and no block carries a wall time of its own. A producer
that knows the wall time at its start writes a map from its clock to the wall clock. A producer that estimates drift
over time writes the map as a track of its own, whose items are map segments.

A device restart is a new clock: the same counter, with the boot count as part of its key. Only an estimated map
relates the two boots, because nothing says how long the device was off.

### Epochs: long captures at fine ticks

Signed 64-bit ticks cover about 292 years at 1 ns, but only about 106 days at 1 ps. A capture that needs more is cut
into **clock epochs**. Each epoch is a new clock of the same counter, and an exact map with slope 1 and error 0 says
that tick 0 of the new epoch is tick `b0` of the previous one. A reader joins the epochs into one long time line, while
the stored ticks stay 64-bit.

### Example: 48 kHz audio on a 1 ns clock

A recorder stamps everything on the host's nanosecond clock and records a 48 kHz audio stream. One sample lasts
1/48000 s, which is 20833⅓ ns and not a whole number of ticks. The track stores its period as the exact fraction
62500/3 ticks per sample, and sample `i` sits at tick `start + ⌊i · 62500/3⌋`:

| Sample | Exact tick | Stored tick |
|---:|---:|---:|
| 0 | 0 | 0 |
| 1 | 20833.33… | 20833 |
| 2 | 41666.66… | 41666 |
| 3 | 62500 | 62500 |
| 172 800 000 (one hour) | 3 600 000 000 000 | 3 600 000 000 000 |

Each sample's tick is rounded down on its own, so the rounding never adds up: after an hour the samples sit exactly one
hour after the first. A period rounded to 20833 ns would drift by 57.6 ms per hour.

The recorder could instead put the audio on a 48 kHz clock of its own and relate that clock to the host clock by a
map. That is the right choice when the codec's crystal drifts against the host.

## Blocks

A tape is a sequence of **blocks**. Every block has a 32-byte header, a body, and a CRC-32C checksum over everything
before it. The header starts with the magic `WWTP`. It then gives the block's version, its kind, its flags, its total
length, a count (records, items or entries, by kind), a sequence number and a session tag.

A file is blocks appended back to back, with no padding between them, and a live feed sends one block per message. The
header and the trailing CRC are the same in every version of the format. A reader can therefore check any block and
skip it by its length, even a block of a kind or version it doesn't know. A later version changes only the bodies, so
an old reader skips a block it doesn't understand instead of taking it for damage.

The **sequence number** counts a writer's blocks from 0, so a gap in it says how many blocks were lost. The **session
tag** names one writer's unbroken append, such as a producer's start or a device's boot. A reader uses the session tag
to tell current data from stale data that an earlier recording left on a medium. [Damage and
recovery](recovery.md#sessions-and-stale-data) explains how.

The block kinds fall into four groups:

| Group | Kinds | Holds |
|---|---|---|
| Description | records on track 0 | what the tape is: the producer, clocks, maps, tracks, API bundles |
| Data | records, events chunk, series chunk | the items |
| Overview | summary | coarse views of long tracks, rebuilt from the data when lost |
| Structure | piece head, index, finish, tail | where things are, how the file ends, how files chain |

The byte layout of the header and of records blocks is in the [`ww_tape` crate
docs](https://github.com/vhrdtech/ww_tape/blob/main/ww_tape/src/lib.rs).

### Definitions on track 0

A reader needs some facts before the data means anything, and **definitions** state them. They describe the producer,
how blocks are placed (the **layout**), the API bundles, every clock, clock map and track, and typed items such as a
run record. Each definition is a record on track 0, which is reserved for definitions. A block that holds definitions
holds nothing else.

Each definition fills a **slot** that follows from the definition itself. A track's slot is its number, a clock's slot
its number, a map's slot the two clocks it links, and a bundle's slot its hash. A newer definition in the same slot
replaces the older one.

The application keeps the definitions it works with in a `Definitions` container. `Definitions::define` holds a
definition in its slot and checks it against those already held: a track or a map must name clocks that are held, and
an epoch clock must come with its link in the same `define_all` call. Defining writes nothing to the tape.
`Definitions::close` drops a track, and the clocks, maps and bundles that no held track names go with it. The container
reports how many definitions it holds (`len`) and their size in bytes (`bytes`), and the application decides how many
to keep.

The writer puts each definition before the first data that needs it. Adding a track mid-piece therefore costs one
record, however many tracks the piece already has.

The producer and the layout belong to the writer, which takes them from `WriterOptions::producer` and
`WriterOptions::layout`. The writer puts them after every piece head, before the first block of a file, and when a
block starts another session. A block of definitions that the caller wrote is refused. A relay passes a live
producer's blocks on with `TapeWriter::relay_block`, which holds the definitions those blocks carry and puts their
records by the writer's own rule.

### What an application does

An application that writes a tape does three things:

1. It keeps the definitions of what it works on in a `Definitions`, and closes a track it stops using.
2. It passes those definitions with every write of data (`write_block`, `write_chunk`, `defer_block`) and every put.
3. It puts a state track's item each time it uses the item (`TapeWriter::put`).

The writer does the rest. It puts the definitions a block needs before the block, writes a state item only where the
same item doesn't lie close behind, and adds copies at later uses. For example, a recorder of 10 000 sensors keeps
10 000 track definitions and passes all of them with each block. A block with the readings of 3 sensors gets those 3
definitions and their clock written before it, and they are written again only every 32 MiB, plus one copy.

```rust
let mut defs = Definitions::new();
defs.define_all(&[clock, temperature_track, mode_track])?;
writer.put(&defs, MODE, ticks, &mode_bytes)?; // a state item, at its use
writer.write_block(&defs, &readings)?; // the definitions it needs go first
```

### State tracks and near use

Some tracks hold a **state** rather than a stream of events: a link's session, the run a recorder is in. A reader that
starts in the middle of a tape needs the item in force at that place, not the history. A track declares state with
`TrackRepeat::State`, which carries a `StatePolicy` of two numbers. `near` says how close behind a place the item must
lie, 32 MiB by default. `copies` says how many extra records the item gets, one by default. Track 0 is a state track
too, and its policy is in the layout (`Layout::definitions`).

The producer **puts** a state item with `TapeWriter::put` each time it uses the item. The writer compares items by a
128-bit BLAKE3 hash. It writes the item only when the newest record of its slot is a different item, or lies more than
`near` bytes behind. The distance is measured to where the data lands, after the puts.

Before every block of data, the writer puts the definitions that the block's tracks need in the same way: first the
clocks, maps and bundles they name, then the track itself. Every block therefore has the definitions it needs close
behind it, and a piece that never uses a track never repeats its definition.

Copies are written at uses, never ahead of them. While the piece holds fewer than `1 + copies` records of an item, a
use at least `copy_spacing` after the newest record writes the item again. `copy_spacing` is a field of `NearPolicy`,
1 MiB by default, so that one damaged block doesn't lose the item. For example, a track added an hour into a piece
costs one record, and its next use 1 MiB later writes one copy.

Puts and copies go into blocks marked with the header flag `FLAG_REPEAT`. Such a block carries the sequence number of
the block before it and is not counted, so the producer's sequence numbers stay its own count. A reader that already
holds the same item in that slot takes the record as nothing new.

Puts that belong with the block after them also carry the flag `FLAG_WITH_NEXT`. When a resume after a crash finds
such puts at the end of what it keeps, it drops them. The application then gives the same call again, and the writer
writes the puts again with the same bytes.

The writer remembers what it wrote for at most `NearPolicy::memory_max` slots and forgets the slot written longest ago
first. A forgotten slot costs one more write, never a wrong read.

### Looking up state

A reader looks state up through the index instead of reading everything. Each state record has its own index entry
with its slot and hash. A piece lists at most `IndexPolicy::state_entries_max` state entries, about a million by
default. The writer refuses a write past that limit, and a `PieceChain` then cuts to a new piece.

`TapeReader` answers three kinds of lookup:

- `state_at` gives the record of a slot in force at a place.
- `item_by_hash` gives an item by its hash.
- `definitions_at` gives every definition in force at a place.

The reader keeps the state entries it has sorted in one cache over all pieces. The cache holds at most
`OpenOptions::state_cache_max` entries, about two pieces' worth, and drops the piece used longest ago first.

A lookup reports what it could not see. A block that doesn't read is listed as lost, and the lookup takes the next
older record of the slot. A stretch that the index doesn't cover is listed as `uncovered`, since a newer record may lie
there: a gap a scan read past, or the bytes past a cut. For `state_at` that is a stretch between the record and the
place, for `definitions_at` one before the place, and for `item_by_hash` one after the record.

A lookup reads at most `OpenOptions::state_read_max` bytes, 1 GiB by default, and charges a coded body's decoded
length before it decodes the body. When that budget runs out, the lookup stops and sets `spent`. The answer is then
incomplete, but nothing in the file is damaged.

### Deferred blocks

A block of summaries or other small data can wait for a gap with `TapeWriter::defer_block`. The writer puts the
definitions the block needs at once, when the block is deferred, and places the block later with no puts of its own.

In an aligned file, the block waits for the gap before a chunk's payload, for at most `DEFER_WINDOW` (1 MiB) of
writes. It therefore lies at most its tracks' `near` plus that window after their definitions. In a file that isn't
aligned, the writer places the block at once. A cut or a finish places every deferred block first, so a deferred
block never leaves its piece.

The definitions are in the [describe module docs](https://github.com/vhrdtech/ww_tape/blob/main/ww_tape/src/describe.rs)
and the slots in the [state module docs](https://github.com/vhrdtech/ww_tape/blob/main/ww_tape/src/state.rs).

### Records blocks

A **records block** holds records of any tracks back to back. Each record has a head, a time delta, the item's length
and the item's bytes. The head names the track and which of the block's clocks the record counts. The head, the delta
and the length are variable-length integers, and records on track 0 carry no time.

The delta is against the previous record on the same clock in the same block, and the first one is absolute, so every
block reads alone. A writer may compress the whole body of a records block with lz4, and the header flag
`FLAG_BODY_CODED` says so.

Records blocks suit tracks that produce a few items at a time, where waiting to fill a chunk would delay a live reader.
Large tracks go into chunks, which [Events and series](events_and_series.md) describes.

## Anatomy of a file

A complete file is a **piece**. It starts with a piece head, then the layout and the producer. The data follows, with
the definitions it needs put close to it and checkpoint index blocks between it. At the end come a final index, a
finish block and a tail. [Tape concepts](concepts.md#one-file-start-to-end) shows one piece in a diagram.

A reader of a finished file starts at the end. It reads the last 164 bytes, the **tail**, which give the piece's
length and where its finish block is. The **finish** block holds the piece's content hash, per-track totals and the
offsets of the final index and the newest checkpoint. The content hash is a BLAKE3 hash over every byte before the
finish, with each chunk's payload taken in by its own hash. The per-track totals are each track's items, ticks and
running hash.

The **final index** lists every block of the piece, sorted by track, with a directory of tracks in its head. A reader
of one track out of thousands reads that directory and that track's list.

While the file is written, it has no tail. The writer places a **checkpoint** index block about every 4 MiB. Each
checkpoint lists every block placed since the previous checkpoint and points back at it. A reader of an unfinished file
finds the newest checkpoint from the end, follows the chain back, and scans only the stretch after the newest one.

All offsets count from the start of the piece, so an index stays valid wherever the piece sits. The layouts are in the
module docs of [index](https://github.com/vhrdtech/ww_tape/blob/main/ww_tape/src/index/mod.rs) and
[piece](https://github.com/vhrdtech/ww_tape/blob/main/ww_tape/src/piece/mod.rs).

A file that a writer started without a piece head, a plain stream of blocks, is still a valid tape. It is one piece
with no ids.

### Pieces and chains

A long recording can be cut into several pieces, and each piece is a complete file that reads alone. A `PieceChain`
cuts by its `CutPolicy`: by size, by chunk count or by age, always before a chunk. Bounded files make retention simple,
since deleting the oldest piece frees space, and a crash costs only the piece being written.

A `PieceChain` keeps its pieces in a store of tapes. `ww_tape_fs::chain::create` opens a chain on files: each piece is
a file of a directory, named `00000000.wwt`, `00000001.wwt` and so on. With `PieceTapes::One` in the chain options,
every piece goes into one file instead.

The pieces of one recording form a **chain**, which is checked in both directions. Each piece head names the previous
piece by the content hash in its finish block. The tail cannot name the next piece's hash, since that piece isn't
written yet. It names the next piece's id instead, which is derived from the piece's own id and hash at the cut, and
the next head repeats that id. Each piece also names the recording it belongs to (the **owner id**, opaque to
`ww_tape`), its sequence number in the chain, and for each track where its items continue.

Pieces written back to back are one valid tape: `cat 00000000.wwt 00000001.wwt` gives a file any reader takes. A
reader finds the last piece's tail at the end, the piece's start from that tail, and the previous piece's tail just
before that start. It walks back through concatenated pieces with one small read each. An unfinished piece after them,
one still being written or cut off by a crash, is found from its checkpoints or by a scan, and the finished pieces are
still found by their tails.

A finished piece is a multiple of 4 KiB long. The writer pads the finish block with zeros so that the tail, still the
last bytes of the piece, ends on the boundary. The padding is part of the finish block and its CRC, and the content
hash covers the bytes before the finish as before.

The alignment lets file systems clone pieces back to back. On btrfs and XFS, `cp --reflink` and `copy_file_range`
(which `std::io::copy` uses between files on Linux) build one file of many pieces without copying a byte and with
nothing between them. Only the end is aligned: blocks inside a piece stay where the writer put them. A piece without a
finish ends where it ends, until a resume finishes it on a boundary.

A chain whose first pieces were deleted reads as "head cut", not as damage.

### Reading by position

`TapeReader` reads a tape through any `ByteSource`, a source of bytes that reads at an offset. A slice in memory is
one, `ww_tape_fs::FileSource` opens a file as one, and an HTTP resource fetched by range requests can be one. The
reader needs no file system and no threads, so it runs in a browser. It opens a finished piece with a few small reads
(the tail, the finish and the final index's directory) and then reads only the blocks a query needs.

## Same input, same bytes

A writer given the same input writes the same bytes. Blocks hold no wall time, and definitions go in the order they
are made. Puts and copies follow from the bytes before them, and blocks are cut by size rather than by time. Every
codec is integer-only or uses arithmetic that every platform rounds the same way.

A producer gives up this property for the blocks it seals on a timer, and for the pieces it cuts by age. One more
exception is the lz4 codec: an lz4-coded payload column is byte-identical only for one version of the lz4 library, on
targets of the same pointer width and byte order. Decoding is unaffected.

Deterministic output makes files comparable by hash. A test can pin a file's exact bytes, and two machines that
recorded the same feed can confirm that they hold the same data.

## See also

- [Format reference](format.md): every part of the format, block by block.
- [The link tap](https://ww.vhrd.tech/transport/tap/), in the WireWeaver book: the producer that records a WireWeaver
  link into tapes.
