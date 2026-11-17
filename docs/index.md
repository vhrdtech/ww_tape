---
level: advanced
---

# Tapes

A **tape** is how WireWeaver records data over time: what a device streamed, what crossed a link, what a program
computed from either. The `ww_tape` crate writes and reads it. One format serves a live feed (blocks handed to a
consumer as they are made), a file on a PC, and a raw medium such as an SD card in a data logger. This chapter explains
the format for readers who have not seen its code: what a file holds, how time is stored, and what survives when
something goes wrong.

The format has four goals, and most of its choices follow from them:

- **Typed data.** Every item is a WireWeaver type, and the file carries the API bundle that defines it. A reader on
  another PC, years later, knows what the bytes mean without the device.
- **Exact time.** Times are integer ticks of a named clock with an exact rate. Nothing is rounded to whole nanoseconds,
  and a file holds tracks on many clocks.
- **Reads in place.** Large tracks are stored in columns that a reader queries as they lie in the file, without decoding
  them into a list first.
- **Damage stays local.** A crash, a torn write or a flipped bit costs the bytes it touched, and a reader says what was
  lost.

The link tap is one producer of tapes; a device log, a simulator or an analysis program writes
the same blocks. Nothing in the format knows any producer.

A reader new to the format starts with [Tape concepts](concepts.md), which names every part of a tape and shows one
file from start to end. This page gives an overview of the whole format: records, clocks, blocks, definitions and the
anatomy of a file. The other pages go deeper, in the order a reader needs them:

- [Tape concepts](concepts.md): the words, and one picture of a file.
- [Writing a tape](writing.md): the writer, definitions and puts, syncing, cuts, relays and resuming after a crash.
- [Pieces and files](pieces_and_files.md): chains, many pieces in one file, copies, and addressing one item.
- [Reading a tape](reading.md): opening a file, the index, state at a place, damage and salvage.
- [Events and series](events_and_series.md): event chunks, the time codec, series and summaries.
- [Numbers](numbers.md): sizes and speeds, and why fixed point comes first.
- [Damage and recovery](recovery.md): checksums, crashes, raw media and the limits of the format.
- [Tapes and logring](logring.md): how a tape differs from a device's log store, and when to use which.
- [Format reference](format.md): every part of the format, block by block.

The exact byte layouts are in the module docs of `ww_tape`, linked from each section; this chapter explains them and
does not repeat them.

## Records, tracks and clocks

A **record** is one item on one track at one time. A **track** is a named sequence of items of one type: the frames a
link received, the temperature readings of a sensor, the log lines of a firmware. A track's definition (`TrackDef`)
gives it a small number used inside the file, a stable key that names the same track across files, the WireWeaver
resource path it comes from, and the clock its times count.

A **clock** is anything that counts time: the host's nanosecond clock, a device's microsecond timer since boot, an audio
codec's sample clock. Its definition (`ClockDef`) gives an exact rate in ticks per second, an exact resolution, and an
**origin**: what tick 0 is. The origin is an identity, never a number. It may be a device's boot, the Unix epoch, the
start of a media file, the start of the producer's session, or nothing known (a free-running counter).

A **tick** is one step of a clock, and every record time is a signed 64-bit count of ticks. Rates are exact fractions,
so clocks that are not whole numbers of hertz stay exact. A video at 60000/1001 frames per second lives on its stream's
60 000 Hz time base, one frame every 1001 ticks, and every frame time is exact.

### Clocks that don't share a time base

Two clocks are related only by a **clock map**: a list of map segments, each saying `b = b0 + (a - a0) · slope` over a
range of clock `a`, with an error bound in ticks of clock `b`. The slope is a reduced fraction, so a fitted drift such
as 1000000007/999999931 stays exact. A map must increase strictly across all its segments, or the order of items would
flip between the clocks. A forward jump of the wall clock (a step from a time server) starts a new map segment; a
backward step is a gap or a new clock.

Wall time is one more clock, with the Unix epoch as its origin. No block carries a wall time of its own. A producer that
knows the wall time at its start writes a map from its clock to the wall clock; one that estimates drift over time
writes the map as a track of its own, whose items are map segments.

A device restart is a new clock: the same counter, with the boot count as part of its key. The two boots are related
only by an estimated map, because nothing guarantees how long the device was off.

### Epochs: long captures at fine ticks

Signed 64-bit ticks cover about 292 years at 1 ns, but only about 106 days at 1 ps. A capture that needs more is cut
into **clock epochs**: each epoch is a new clock of the same counter, and an exact map with slope 1 and error 0 says
that tick 0 of the new epoch is tick `b0` of the previous one. A reader joins the epochs into one long coordinate; the
stored ticks stay 64-bit.

### Example: 48 kHz audio on a 1 ns clock

A recorder stamps everything on the host's nanosecond clock and records a 48 kHz audio stream. One sample lasts 1/48000
s, which is 20833⅓ ns: not a whole number of ticks. The track stores its period as the exact fraction 62500/3 ticks per
sample, and sample `i` sits at tick `start + ⌊i · 62500/3⌋`:

| Sample | Exact tick | Stored tick |
|---:|---:|---:|
| 0 | 0 | 0 |
| 1 | 20833.33… | 20833 |
| 2 | 41666.66… | 41666 |
| 3 | 62500 | 62500 |
| 172 800 000 (one hour) | 3 600 000 000 000 | 3 600 000 000 000 |

Each sample's tick is rounded down on its own, so the rounding never adds up: after an hour the samples sit exactly one
hour after the first. A period rounded to 20833 ns would drift by 57.6 ms per hour. The same recorder could also put the
audio on a 48 kHz clock of its own and relate it to the host clock by a map; that is the right choice when the codec's
crystal drifts against the host.

## Blocks

A tape is a sequence of **blocks**. Every block has a 32-byte header, a body and a CRC-32C checksum over everything
before it. The header starts with the magic `WWTP` and gives the block's version, its kind, its total length, a count
(records, items or entries, by kind), a sequence number and a session tag. A file is blocks appended back to back, never
padded between them; a live feed sends one block per message.

The header and the trailing CRC are the same in every version of the format. A reader can therefore check any block and
skip it by its length, even a block of a kind or version it doesn't know. A later version changes only the bodies, and
an old reader takes a block it doesn't understand as one it skips, not as damage.

The **sequence number** counts a writer's blocks from 0, so a gap in it says how many blocks were lost. The **session
tag** names one writer's unbroken append: a producer's start, a device's boot. The session tag is how a reader tells
current data from stale data left on a medium by an earlier recording; [Damage and
recovery](recovery.md#sessions-and-stale-data) explains it.

The block kinds fall into four groups:

| Group | Kinds | Holds |
|---|---|---|
| Description | records on track 0 | what the tape is: the producer, clocks, maps, tracks, API bundles |
| Data | records, events chunk, series chunk | the items |
| Overview | summary | coarse views of long tracks, rebuilt from the data when lost |
| Structure | piece head, index, finish, tail | where things are, how the file ends, how files chain |

The byte layout of the header and of record blocks is in the [`ww_tape` crate
docs](https://github.com/vhrdtech/ww_tape/blob/main/ww_tape/src/lib.rs).

### Definitions on track 0

What a reader needs before the data is said by **definitions**: the producer, how blocks are placed (the layout), the
API bundles, every clock, clock map and track, and typed items such as a run record. Each definition is a record on
track 0, which is reserved for them, in an ordinary record block. A definition has a **slot**, worked out from the
definition itself: a track's slot is its number, a clock's its number, a map's the two clocks it links, and a bundle's
its hash. A newer definition in the same slot replaces the older one. A block that holds definitions holds nothing
else.

The application keeps the definitions it works with in a `Definitions` container. `Definitions::define` holds a
definition in its slot, checked against those held: a track names a clock held, and an epoch clock comes with its link
through `define_all`. Nothing is written then. The writer puts each definition before the first data that needs it,
so adding a track mid-piece costs one record, however many tracks the piece already has. `Definitions::close` drops a
track, and the clocks, maps and bundles no held track names go with it. The container reports how many definitions it
holds and their bytes (`len`, `bytes`); how many to keep is the application's choice.

The producer and the layout are the writer's own: `WriterOptions::producer` and `WriterOptions::layout`. The writer
puts them after every piece head, before the first block of a file and when a block starts another session. A
caller's block of definitions is refused. A relay passes a live producer's blocks of puts on with
`TapeWriter::relay_block`, which holds their definitions and puts their records by the writer's own rule.

### What an application does

An application that writes a tape does three things:

1. It keeps the definitions of what it works on in a `Definitions`, and drops a track it stops using.
2. It passes them with every write of data (`write_block`, `write_chunk`, `defer_block`) and every put.
3. It puts a state track's item each time it uses it (`TapeWriter::put`).

The writer does the rest: it puts the definitions a block needs before it, writes a state item only where the same
item doesn't lie near behind, and adds copies at later uses. For example, a recorder of 10 000 sensors keeps 10 000
track definitions and passes them with each block; a block of 3 sensors' readings gets those 3 definitions and their
clock written before it, once per 32 MiB plus one copy.

```rust
let mut defs = Definitions::new();
defs.define_all(&[clock, temperature_track, mode_track])?;
writer.put(&defs, MODE, ticks, &mode_bytes)?; // a state item, at its use
writer.write_block(&defs, &readings)?; // the definitions it needs go first
```

### State tracks and near use

Some tracks hold a **state** rather than a stream of events: a link's session, the run a recorder is in. A reader
that starts in the middle needs the item in force, not the history. A track says so with `TrackRepeat::State`, which
carries a `StatePolicy`: how near behind a place its item must lie (`near`, 32 MiB by default) and how many copies an
item gets (`copies`, one by default). Track 0 is a state track too, with its policy in the layout.

The producer **puts** a state item with `TapeWriter::put` each time it uses it. The writer writes the item only when
the newest record of its slot isn't the same item within `near` bytes behind; items are compared by a 128-bit BLAKE3
hash. Before every block of data the writer puts the definitions the block's tracks need in the same way: the clocks,
maps and bundles they name, then the track itself. The distance is measured to where the data lands, after the puts.
So every block has the definitions it needs not far behind it, and a piece that never uses a track never repeats its
definition.

Copies are written at uses, never ahead of them. While fewer than `1 + copies` records of an item lie in the piece, a
use at least `copy_spacing` after the newest one (1 MiB by default, `NearPolicy`) writes the item again, so one
damaged block doesn't lose it. For example, a track added an hour in costs one record, and its next use 1 MiB later
writes a copy.

Puts and copies go in blocks marked with the header flag `FLAG_REPEAT`. Such a block carries the seq of the block
before it and is not counted, so the producer's seq stays its own count. A reader holding the same item in that slot
takes the record as nothing new. Puts that belong with the block after them are also marked `FLAG_WITH_NEXT`: a
resume drops them when they end what it keeps, so the call, given again, writes them again with the same bytes. The
writer's memory of what it wrote is capped (`NearPolicy::memory_max`) and forgets the slot written longest ago first;
a slot forgotten costs one more write, never a wrong read.

A reader looks state up by the index instead of reading everything: each state record has its own index entry with
its slot and hash. A piece lists at most `IndexPolicy::state_entries_max` of them (about a million by default); a
write past that is refused, and `PieceChain` cuts to a new piece. `TapeReader::state_at` gives the record of a slot in
force at a place, `item_by_hash` an item by its hash, and `definitions_at` every definition in force at a place. The
reader keeps the state entries it sorted in one cache over all pieces, at most `OpenOptions::state_cache_max` entries
(about two pieces' worth), and drops the piece used longest ago first. A block that doesn't read is reported as lost,
and the next older record of the slot is taken. A stretch the index doesn't cover (`uncovered`: a gap a scan read past, the bytes past a cut) is reported
too, since a newer record may lie there: for `state_at` between the record and the place, for `definitions_at` before
the place, for `item_by_hash` after the record. A lookup reads at
most `OpenOptions::state_read_max` bytes (1 GiB by default), and charges a coded body's decoded length before it
decodes it. When that budget runs out, the lookup stops and sets `spent`: the answer is incomplete, but nothing in the
file is damaged.

A block of summaries or other small data can wait for a gap with `TapeWriter::defer_block`. The writer puts the
definitions the block needs at once, when the block is deferred, and places the block later with no puts of its own.
In an aligned file it waits for the gap before a chunk's payload, at most `DEFER_WINDOW` (1 MiB) of writes, so it lies
at most its tracks' `near` plus that window after their definitions; in a file that isn't aligned it is placed at
once. A cut or a finish places every deferred block first, so a deferred block never leaves its piece. The definitions are
in the [describe module docs](https://github.com/vhrdtech/ww_tape/blob/main/ww_tape/src/describe.rs) and the
slots in the [state module docs](https://github.com/vhrdtech/ww_tape/blob/main/ww_tape/src/state.rs).

### Record blocks

A **record block** holds records of any tracks back to back. Each record is a head (the track number and which of the
block's clocks it counts), a time delta, the item's length and the item's bytes, all as variable-length integers. The
delta is against the previous record on the same clock in the same block, and the first one is absolute, so every block
reads alone.

Record blocks suit tracks that produce a few items at a time, where waiting to fill a chunk would delay a live reader.
Large tracks go into chunks, which the [next page](events_and_series.md) describes.

## Anatomy of a file

A complete file is a **piece**: a piece head, the layout and producer, the data with the definitions it needs put
near it and periodic index blocks between it, and at the end a final index, a finish block and a tail. [Tape
concepts](concepts.md#one-file-start-to-end) shows one piece in a diagram.

A reader of a finished file starts at the end. It reads the last 164 bytes, the **tail**, which give the piece's length
and where its finish block is. The **finish** block holds a BLAKE3 hash of every byte before it, per-track totals
(items, ticks), and the offsets of the final index and the newest checkpoint. The **final index** lists every block of
the piece sorted by track, with a directory in its head, so a reader of one track out of thousands reads that directory
and that track's list.

While the file is written there is no tail. The writer places a **checkpoint** index block about every 4 MiB, listing
every block placed since the previous checkpoint and pointing back at it. A reader of an unfinished file finds the
newest checkpoint from the end, follows the chain back, and scans only the stretch after the newest one. All offsets
count from the start of the piece, so an index stays valid wherever the piece sits. The layouts are in the module docs
of [index](https://github.com/vhrdtech/ww_tape/blob/main/ww_tape/src/index/mod.rs) and
[piece](https://github.com/vhrdtech/ww_tape/blob/main/ww_tape/src/piece/mod.rs).

A file that a writer started without a piece head (a plain stream of blocks) is still a valid tape; it is one piece with
no ids.

### Pieces and chains

A long recording can be cut into several pieces, each a complete file that reads alone. A `PieceChain` cuts by
size, by chunk count or by age, always before a chunk, and names the files `00000000.wwt`, `00000001.wwt` and so on. Bounded files
make retention simple (delete the oldest piece), and a crash costs only the piece being written.

The pieces of one recording form a **chain**, checked in both directions. Each piece head names the previous piece by
the content hash in its finish block. The tail cannot name the next piece's hash, since that piece isn't written yet, so
it names the next piece's id instead, derived from its own id and hash at the cut. The next head repeats that id. Each
piece also names the recording it belongs to (the owner id, opaque to `ww_tape`), its sequence number in the chain, and
for each track where its items continue.

Pieces written back to back are one valid tape: `cat 00000000.wwt 00000001.wwt` gives a file any reader takes. A reader
finds the last piece's tail at the end, the piece's start from it, and the previous piece's tail just before that start,
so it walks back through concatenated pieces with one small read each. An unfinished piece after them (one being
written, or cut off by a crash) is found from its checkpoints or by a scan, and the finished ones still by their tails.

A finished piece is a multiple of 4 KiB long. The writer pads the finish block with zeros so the tail, still the last
bytes of the piece, ends on the boundary; the padding is part of the finish block and its CRC, and the content hash
covers the bytes before the finish as before. Files are then cloned back to back on btrfs and XFS (`cp --reflink`,
`copy_file_range`, which `std::io::copy` uses between files on Linux): one file holds many pieces with no byte copied
and nothing between them. Only the end is aligned: blocks inside a piece stay where the writer put them, and a piece
without a finish ends where it ends, until a resume finishes it on a boundary. A chain whose first pieces were deleted
reads as "head cut", not as damage.

### Reading by position

`TapeReader` reads a tape through any byte source that reads at an offset: a slice in memory, a file, or an HTTP
resource fetched by range requests. It needs no file system and no threads, so it runs in a browser. A finished piece is
opened with a few small reads (tail, finish, the final index's directory) and then only the blocks a query needs.

## Same input, same bytes

A writer given the same input writes the same bytes. Blocks hold no wall time, definitions go in the order they are
made, puts and copies follow from the bytes before them, blocks are cut by size rather than by time, and every codec is integer-only or uses arithmetic that every platform
rounds the same way. A producer that seals blocks on a timer, or cuts pieces by age, gives up this property for the
blocks it seals that way.

One exception: an lz4-coded payload column is byte-identical only for one version of the lz4 library on targets of the
same pointer width and byte order. Decoding is unaffected.

Deterministic output makes files comparable by hash, lets a test pin a file's exact bytes, and lets two machines that
recorded the same feed confirm they hold the same data.

## See also

- [Format reference](format.md): every part of the format, block by block.
- [The link tap](../transport/tap.md): the producer that records a WireWeaver link into tapes.
