---
level: advanced
---

# Reading a tape

A reader of a tape usually wants a little of it: one track out of thousands, one hour out of a week, an overview of a
signal. This page explains how `TapeReader` gets there with few reads, how it finds the definitions and state in force
at a place, and what it does with damage. It assumes [Tape concepts](concepts.md).

## Byte sources

`TapeReader` reads through a **byte source** (`ByteSource`): anything that has a length and reads bytes at an offset. A
byte slice, a file and the bytes of a memory sink being written (`MemSink::tape`) are byte sources, and an HTTP resource read by range requests can be
one. The reader needs no file system and no threads, so it also builds for WebAssembly and runs in a browser: it needs the
crate's `alloc` feature, not `std`. The block walk `FileBlocks` of the last section needs `std`.

## Opening from the tails

`TapeReader::open(source)` finds the pieces from the end of the bytes, because a finished piece describes itself there:

1. It reads the last 164 bytes. A good **tail** gives the last piece's length, so its start, and where its finish block
   is.
2. Just before that start lies the previous piece's tail, if there is one. The reader steps back piece by piece with a few
   small reads each: the tail, the finish and the piece head.
3. A piece without a tail, one being written or cut off by a crash, is found from its newest **checkpoint**. The reader
   looks backwards from the end for an index block, at most `OpenOptions::scan_back_max` (256 MiB by default). It
   follows the chain of checkpoints back to list the blocks before it, and scans the blocks after it one by one.
4. A piece without any checkpoint, one shorter than about 4 MiB, is scanned from its start, or from the end of the
   finished piece before it.

`TapeReader::pieces` then lists each piece as a `PieceInfo`: where it starts and ends, its head, finish and tail when it
has them, and how it was found (`Found::Tail`, `Checkpoint` or `Scan`, with the bytes scanned). A scan stops at the
first block that isn't good, and `PieceInfo::cut` names that offset. `PieceInfo::gaps` lists blank or bad stretches
the scan read past, and `PieceInfo::replaced` lists blocks that were written twice. A piece of another format version
has `PieceInfo::version` set; every read of it is refused with `ReadError::Version`.

A raw medium, such as an SD card, isn't read from its end, where an older recording's tail may pass for the newest
piece. Open it with `TapeReader::open_with` and `OpenOptions::medium` set to `Medium::Raw`: the reader then walks
forward from the medium's start, or from a hint its medium layer gives. [Damage and
recovery](recovery.md#sessions-and-stale-data) explains why.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/seek.svg"
<figcaption>Above, a finished piece: the reader reads the tail (1), the finish (2), the final index's directory (3) and
one track's list of blocks, and then only the blocks it needs (4). Below, an unfinished piece: the newest checkpoint
and its back pointers list every block before it, and only the blocks after it are read one by one.</figcaption>
</figure>

## Finding blocks through the index

A reader finds blocks through the piece's index, never by walking the whole file:

- `TapeReader::entries(i)` gives every block of piece `i`, in file order, as `IndexEntry`s: offset, length, kind, track,
  first item number, item count, first and last tick, and the latest end of any item.
- `TapeReader::track_entries(i, track_id)` gives one track's blocks. In a finished piece it reads only the final index's
  directory and that track's list, each checked by its own CRC, so a reader of one track out of thousands reads a few
  hundred bytes of index. Track id 0 gives the record blocks of data, and `STATE_TRACK_ID` the state entries.
- `TapeReader::read_block(i, entry, buf)` reads the block an entry names and checks it.
- `TapeReader::read_chunk_head(i, entry, buf)` reads only the head of the chunk an entry names, up to its payload, and
  checks it by the head CRC: what a chunk is, and a series chunk's grid and scale, without its samples.
- `TapeReader::series_timeline(i, track_id, period)` reads the head of every chunk of a series track and gives its
  `SeriesTimeline`, beside the errors of the heads it left out ([Events and
  series](events_and_series.md#grids-where-samples-are-in-time)).
- `TapeReader::state_entries(i)` gives piece `i`'s state entries, each with its index entry.

An entry tells a reader whether it needs a block before reading it. A query for the items between two ticks takes the
entries whose tick range overlaps; a query for item 1 000 000 takes the entry whose item range holds it.

## State at a place

Before a reader can decode items it needs the definitions in force where the items are: what each track is, which clock
it counts, what type its items have. A track that holds state needs its item in force too. The format has no "current
state" of a whole file: what is in force depends on the place. The reader looks it up through the index, without reading
the blocks in between.

Every state record has its own index entry, with its slot and the hash of its item. Three lookups use them:

- `TapeReader::state_at(i, slot, offset)` gives the record of one slot in force at `offset` in piece `i`: the newest one
  whose block starts before the place.
- `TapeReader::definitions_at(i, offset)` gives every definition in force at a place, by slot (`DefinitionsAt`). Pass
  `u64::MAX` for the end of the piece.
- `TapeReader::item_by_hash(i, hash)` gives the newest record whose item has that hash, of any slot. An application that
  refers to a state item by its hash finds it this way.

The writer keeps every state item near its use, so the record a lookup finds usually lies within `near` (32 MiB by
default) behind the place. For example, a viewer that opens a week-long piece at 15:00 on Wednesday looks up the offset
of that hour in the index, asks `definitions_at` for it, and gets the definitions it needs from a few small reads,
however much lies before.

Damage is a reason to look further, never an error. A lookup whose newest record doesn't read lists it as lost and takes
the next older record of the slot. It also names the stretches the index doesn't cover (`uncovered`), such as a gap a scan read
past or the bytes past a cut, since a newer record may lie there: for `state_at` those between the record and the
place, for `definitions_at` those before the place, and for `item_by_hash` those after the record. A
lookup reads at most `OpenOptions::state_read_max` bytes (1 GiB by default). When that budget runs out, the lookup stops
and sets `spent`: the answer is incomplete, but nothing in the file is damaged. `is_whole` says that nothing was lost,
uncovered or left unread.

## Reading items

How a reader gets at the items depends on the block:

- **A record block.** `Block::records` walks its records in order: track, time and item bytes. A body coded with lz4
  (header flag `FLAG_BODY_CODED`) is decoded first with `Block::decode`, whose result has the same `records`.
- **An events chunk.** `Block::chunk` gives the chunk's head and columns. `EventTimes::read` reads its times in place:
  `starts().get(j)` gives item `j`'s start tick, `lower_bound(t)` the first item at or after tick `t`,
  `last_at_or_before(t)` the last item at or before it, and `end(j)` and `flag(j)` its end and flags. `Payloads::read`
  gives each item's bytes with `get(j)`, without decoding the items before it: in place for a raw payload column,
  and from one decode of the whole column for an lz4 one.
- **A series chunk.** `Samples::of` takes the samples column, and `Samples::decode` decodes any range of samples from the group that holds its first sample. The
  chunk's grid gives each sample's tick and its scale; `SeriesTimeline` collects the grids of a track's chunks, checks
  each chunk against its neighbours and says where the gaps are.
- **A summary block.** It holds coarse statistics of a long track, for an overview without reading the items. [Events
  and series](events_and_series.md#summaries) describes them.

Item `j` of a chunk is item `first item number + j` of its track. A reader so goes from an item number to the chunk
through the index, and from the chunk to the item by its position. [Pieces and
files](pieces_and_files.md#addressing-one-item) explains why items are addressed this way.

## Checking a piece

`TapeReader::verify(i)` hashes piece `i`'s bytes before its finish and compares the result with the content hash the
finish states. A match proves the piece unchanged since it was written. It also replays the piece block by block as its
writer placed them, so a writer that lied in its index or finish fails too: each entry of the final index must be the
entry its writer makes of the block at its offset (offset, length, kind, track, items, ticks, a chunk's unit hash of its
bytes and the range of its range relation from its own column), and each finish row must state the hash its track's
listed blocks fold to, from the head row's, or say it incomplete exactly where the head or a damaged block a resume
kept leaves units out. The index and the walk must be the same set. Every block walked that its writer lists is listed,
except a byte-identical copy of the listed block before it (a block written again), which adds to no track. Every entry
names a block walked, or a damaged block in bytes no block explains that ends before the next block walked and before
the finish. Each block and entry costs a bounded number of steps, and a piece listing more than `VERIFY_ENTRIES_MAX`
entries is refused (`ReadError::VerifyEntries`). `TapeReader::chain` checks the links between the pieces at hand: each piece head against the piece before it,
and each tail's next id against the next piece.

A piece without a finish has no hash to check. That is how a reader knows the piece wasn't closed.

## Damage and salvage

Every block carries a CRC-32C, and a reader checks it before using the block. Most damage therefore costs one block, and
the index still says what that block held: which track, which items, which ticks.

Inside a chunk the reader can save more:

- A chunk's head has a CRC of its own, and its payload a CRC per 64 KiB. With a good head and a damaged payload,
  `Block::salvage` hands out the chunk with its bad segments marked (`Block::is_intact` false, `Chunk::bad_segments`,
  `Chunk::range_damaged`).
- `EventTimes::salvage` and `Payloads::salvage` check an events chunk's columns group by group, each group of 128 items
  against its neighbours. Only the items of a group that fails are marked suspect (`is_suspect`), and of a raw payload column also the items
  with a byte in a bad segment; the rest read as usual. A damaged lz4 payload column makes every item suspect.
- `FileBlocks` with `Recovery::Resync` walks a damaged file block by block and finds the next good block after a bad
  stretch, where the default `Recovery::Prefix` stops.

[Damage and recovery](recovery.md) gives the full picture: what each kind of damage costs and what the format can't
protect against.
