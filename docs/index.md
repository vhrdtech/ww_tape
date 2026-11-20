<!-- The logo is inlined (assets/logo, `just logo`) so the book needs no image files; one of the two shows per
scheme, by the rules of stylesheets/hero.css. -->
<div class="ww-tape-hero ww-tape-hero--light" markdown="0" role="img" aria-label="ww_tape">
--8<-- "assets/logo/ww_tape-light.svg"
</div>
<div class="ww-tape-hero ww-tape-hero--dark" markdown="0" role="img" aria-label="ww_tape">
--8<-- "assets/logo/ww_tape-dark.svg"
</div>

# Tapes

A **tape** is a recording of typed data over time: what a device streamed, what crossed a link, or what a program
computed from either. One format serves three uses:

- A live feed hands each block to a consumer as soon as the block exists.
- A file on a personal computer (PC) holds a recording.
- A raw medium, such as the Secure Digital (SD) card of a data logger, holds a recording without a file system.

This book explains the format to readers who have not seen its code. It shows what a file holds, how a tape stores
time, and what survives when something goes wrong.

Tapes belong to WireWeaver, a framework for typed APIs between devices and hosts. Every item on a tape is a value of a
WireWeaver type.

[WireWeaver and logring](related_books.md) introduces WireWeaver and logring, a log store for devices. Read it first
if you need that background.

## What the format is for

The format has four goals, and most of its choices follow from them:

- **Typed data:** the file carries the API bundle that defines each item's WireWeaver type. A reader on another PC,
  years later, knows what the bytes mean without the device.
- **Exact time:** times are integer ticks of a named clock with an exact rate. The format rounds nothing to whole
  nanoseconds, and one file holds tracks on many clocks.
- **Reads in place:** the format stores large tracks in columns. A reader queries a column as it lies in the file,
  without decoding it into a list first.
- **Damage stays local:** a crash, a torn write or a flipped bit costs the bytes it touched. A reader reports what it
  lost.

Nothing in the format knows any particular producer. WireWeaver's link tap records a link into tapes, and a device
log, a simulator or an analysis program writes the same blocks.

## The crates

Four crates implement the format. An application that writes files takes `ww_tape` and `ww_tape_fs`; one that reads
or draws summaries adds `ww_tape_summary`.

| Crate | What it holds |
|---|---|
| `ww_tape` | the format: blocks, definitions, chunks, the index, pieces, the writer (`TapeWriter`, `PieceChain`) and the reader (`TapeReader`); it builds without the standard library |
| `ww_tape_codecs` | the column codecs, which know nothing of blocks: bit packing, cyclic redundancy checks (CRC-32C), varints, lz4, sample, time and length codecs; `ww_tape` re-exports them |
| `ww_tape_fs` | everything that names a path: the file sink `FileSink`, the store of piece files `Files`, the file reader `FileSource`, and writers and chains opened by path |
| `ww_tape_summary` | summary trees: `EventTree` and `SeriesSummaries` build summary blocks, and `Summary::of_block` reads their nodes |

`ww_tape` itself checks a summary block and reads its head. Its index therefore lists the block without decoding any
node.

## How to read this book

A reader new to the format starts with [Tape concepts](concepts.md), which names every part of a tape. The groups
below give the pages in the order a reader needs them.

### The format in overview

- [Tape concepts](concepts.md): the words: items, tracks, clocks, blocks, definitions and state.
- [Sessions, pieces and chains](sessions_and_pieces.md): how blocks group, cuts, and one picture of a file.
- [Records, tracks and clocks](records_and_clocks.md): how a tape stores time, and the maps between clocks.
- [Blocks and definitions](blocks.md): the block header, definitions on track 0, records blocks and deferred blocks.
- [Anatomy of a file](anatomy.md): checkpoints, the end of a piece, reading by position, and same bytes from same input.

### Writing

- [Writing a tape](writing.md): the writer, its sinks and stores, definitions and puts, and the forms of data.
- [Sinks, files and locks](sinks_and_locks.md): writing to files and memory, the lock on a file, and firmware.
- [Syncing, cuts and relays](syncing_and_cuts.md): syncing, checkpoints, cuts, the close, and relays.
- [Resuming a writer](resuming.md): reopening a piece or a chain after a crash.
- [Pieces and files](pieces_and_files.md): chains, many pieces in one file, copies, and addressing one item.
- [Track owners and hashes](track_hashes.md): tracks of several owners, the track hash and the hash of a range.

### Reading and the data

- [Reading a tape](reading.md): opening a file, the index, state at a place, damage and salvage.
- [Reading a growing piece](growing_pieces.md): several closes, the unclosed tail, and noticing new bytes.
- [Events and series](events_and_series.md): chunks, event times, ends, flags and payloads.
- [Series](series.md): samples, fixed point, grids and series files.
- [Summaries](summaries.md): the trees that let a viewer draw a long track without reading it.
- [Numbers](numbers.md): sizes and speeds, and why fixed point comes first.

### Damage and the reference

- [Damage and recovery](recovery.md): checksums, and resuming after a crash.
- [Reading a damaged file](damaged_files.md): reading past damage, what a damaged file still gives, and the limits.
- [Sessions and raw media](raw_media.md): stale data on a reused card, gaps and duplicate blocks.
- [Format reference](format.md): every part of the format, block by block.

### Byte layouts and other books

The logring book compares a tape with a device's log store in [Tapes and
logring](https://lr.vhrd.tech/tapes_and_logring/).

The module docs of the `ww_tape` crate hold the exact byte layouts, and each page links them. This book explains those
layouts and does not repeat them.

## What an application does

An application that writes a tape does three things. [Blocks and definitions](blocks.md#definitions-on-track-0)
explains definitions, and the next section explains puts.

1. It keeps the definitions of what it works on in a `Definitions` container, and closes a track it stops using.
2. It passes those definitions with every write of data (`write_block`, `write_chunk`, `defer_block`) and every put.
3. It puts a state track's item each time it uses the item (`TapeWriter::put`).

The writer does the rest. It puts the definitions a block needs before the block.

It writes a state item only where the same item doesn't lie close behind. It also adds copies at later uses.

For example, a recorder of 10 000 sensors keeps 10 000 track definitions, and passes all of them with each block.
Before a block with the readings of 3 sensors, the writer writes those 3 definitions and their clock.

The writer writes them again only every 32 MiB, plus one copy.

```rust
let mut defs = Definitions::new();
defs.define_all(&[clock, temperature_track, mode_track])?;
writer.put(&defs, MODE, ticks, &mode_bytes)?; // a state item, at its use
writer.write_block(&defs, &readings)?; // the definitions it needs go first
```

## State tracks and near use

Some tracks hold [state](concepts.md#state-near-and-copies) rather than a stream of events: a link's session, the
run a recorder is in. A reader that starts in the middle of a tape needs the item in force at that place, not the
history.

A track declares state with `TrackRepeat::State`, which carries a `StatePolicy` of two numbers:

- `near` says how close behind a place the item must lie, 32 MiB by default.
- `copies` says how many extra records the item gets, one by default.

Track 0 is a state track too, and its policy is in the layout (`Layout::definitions`).

### Puts

Each time the producer uses a state item, it puts the item: it calls `TapeWriter::put` with it. The writer compares
items by their 128-bit hash, made with BLAKE3 (a cryptographic hash function).

The writer writes the item only when the newest record of its slot is a different item, or lies more than `near` bytes
behind. It measures the distance to where the data lands, after the puts.

Before every block of data, the writer puts the definitions that the block's tracks need in the same way. It puts
first the clocks, maps and bundles they name, then the track itself.

Every block therefore has the definitions it needs close behind it. A piece that never uses a track never repeats its
definition.

### Copies

The writer writes copies at uses, never ahead of them. While the piece holds fewer than `1 + copies` records of an
item, a use at least `copy_spacing` after the newest record writes the item again.

`copy_spacing` is a field of `NearPolicy`, 1 MiB by default. With that spacing, one damaged block doesn't lose the
item.

For example, a track added an hour into a piece costs one record. Its next use, 1 MiB later, writes one copy.

### Flags and memory

A block that holds puts or copies carries the header flag `FLAG_REPEAT` and the sequence number of the block before
it. Such a block does not count as a block of its own.

The producer's sequence numbers therefore stay its own count. A reader that already holds the same item in that slot
takes the record as nothing new.

The flag `FLAG_WITH_NEXT` also marks puts that belong with the block after them. When a resume after a crash finds
such puts at the end of what it keeps, it drops them.

The application then gives the same call again, and the writer writes the puts again with the same bytes.

The writer remembers what it wrote for at most `NearPolicy::memory_max` slots. It forgets the slot written longest ago
first, and a forgotten slot costs one more write, never a wrong read.

### Looking up state

A reader looks state up through the index, instead of reading everything. Each state record has its own index entry
with its slot and hash.

A piece lists at most `IndexPolicy::state_entries_max` state entries, about a million by default. The writer refuses a
write past that limit, and a `PieceChain` then cuts to a new piece.

`TapeReader` answers three kinds of lookup:

- `state_at` gives the record of a slot in force at a place.
- `item_by_hash` gives an item by its hash.
- `definitions_at` gives every definition in force at a place.

The reader keeps the state entries it has sorted in one cache over all pieces. The cache holds at most
`OpenOptions::state_cache_max` entries, about two pieces' worth, and drops the piece used longest ago first.

#### What a lookup reports

A lookup reports what it could not see. It lists a block that doesn't read as lost, and takes the next older record
of the slot.

It lists a stretch that the index doesn't cover as `uncovered`, since a newer record may lie there. Such a stretch is
a gap a scan read past, or the bytes past a cut.

Which stretch counts depends on the lookup:

- for `state_at`, a stretch between the record and the place;
- for `definitions_at`, a stretch before the place;
- for `item_by_hash`, a stretch after the record.

A lookup reads at most `OpenOptions::state_read_max` bytes, 1 GiB by default. It charges a coded body's decoded length
before it decodes the body.

When that budget runs out, the lookup stops and sets `spent`. The answer is then incomplete, but nothing in the file
is damaged.

## Pieces and chains

A [piece](sessions_and_pieces.md#pieces-and-chains) is a complete file that reads alone; [Anatomy of a
file](anatomy.md) shows what it holds. A writer can cut a long recording into several pieces.

A `PieceChain` cuts by its `CutPolicy`: by size, by chunk count or by age, always before a chunk. Bounded files make
retention simple: deleting the oldest piece frees space.

A crash costs only the piece being written.

### Where the pieces go

A `PieceChain` keeps its pieces in a store of tapes. `ww_tape_fs::chain::create` opens a chain on files: each piece is
a file of a directory, named `00000000.wwt`, `00000001.wwt` and so on.

With `PieceTapes::One` in the chain options, every piece goes into one file instead.

### How pieces link

The pieces of one recording form a chain, which a reader checks in both directions. Each piece head names the
previous piece by the content hash in its close record.

The tail cannot name the next piece's hash, since the writer hasn't written that piece yet. It names the next piece's
id instead.

The writer derives that id from the piece's own id and hash at the cut, and the next head repeats it. Each piece also
names three more things:

- the recording it belongs to: the **owner id**, which is opaque to `ww_tape`;
- its sequence number in the chain;
- for each track, where its items continue.

### Pieces back to back

Pieces written back to back are one valid tape: `cat 00000000.wwt 00000001.wwt` gives a file any reader takes. A
reader finds the last piece's tail at the end, then the piece's start from that tail.

The previous piece's tail lies just before that start. The reader walks back through concatenated pieces with one
small read each.

An unclosed piece may follow them: one still being written, or one a crash cut off. The reader finds it from its
checkpoints or by a scan, and still finds the closed pieces by their tails.

### Pieces end on 4 KiB

A closed piece is a multiple of 4 KiB long. The writer pads the close record with zeros, so that the tail ends on
the boundary and stays the last bytes of the piece.

The padding is part of the close record and its CRC-32C checksum. The content hash covers the bytes before the close,
as before.

The alignment lets file systems clone pieces back to back. On btrfs and XFS (two Linux file systems), `cp --reflink`
and `copy_file_range` build one file of many pieces.

They copy no byte, and leave nothing between the pieces. On Linux, `std::io::copy` uses `copy_file_range` between
files.

The writer aligns only the end: blocks inside a piece stay where the writer put them. A piece without a close ends
where it ends, until a resume closes it on a boundary.

A chain whose first pieces someone deleted reads as "head cut", not as damage.

## See also

- [Format reference](format.md): every part of the format, block by block.
- [The link tap](https://ww.vhrd.tech/transport/tap/), in the WireWeaver book: the producer that records a WireWeaver
  link into tapes.
