# Reading a tape

A reader of a tape usually wants a small part of it: one track out of thousands, one hour out of a week, or an overview
of a signal. This page explains how `TapeReader` gets there with few reads, how it finds the definitions and state in
force at a place, and what it does with damage. It assumes [Tape concepts](concepts.md).

## Byte sources

`TapeReader` reads through a **byte source** (`ByteSource`): anything that has a length and reads bytes at an offset.
The crate provides byte sources for these:

- a byte slice or a `Vec<u8>`;
- a file, through `ww_tape_fs::FileSource::open`;
- a tape still being written into memory, through `MemSink::tape`.

An HTTP resource read by range requests can be a byte source too. The reader needs no file system and no threads, so
it builds for WebAssembly and runs in a browser: it needs the crate's `alloc` feature, not `std`. Only the block walk
`FileBlocks` of the last section needs `std`.

## Opening from the tails

`TapeReader::open(source)` finds the pieces from the end of the bytes, because a finished piece describes itself
there. It works in four steps:

1. It reads the last 164 bytes (`TAIL_LEN`). A good **tail** gives the last piece's length, and so its start, and the
   offset of its finish block.
2. The previous piece's tail, if there is one, lies just before that start. The reader steps back piece by piece with
   three small reads each: the tail, the finish and the piece head.
3. A piece without a tail is one still being written or one cut off by a crash. The reader finds it from its newest
   **checkpoint**, an index block the writer places every 4 MiB by default. It looks backwards from the end for one,
   at most `OpenOptions::scan_back_max` (256 MiB by default). It follows the chain of checkpoints back to list the
   blocks before the newest one, and scans the blocks after it one by one.
4. A piece without any checkpoint is, with the default policy, shorter than 4 MiB. The reader scans it from its start,
   or from the end of the finished piece before it.

`TapeReader::pieces` then lists each piece as a `PieceInfo`. It says where the piece starts and ends, holds its head,
finish and tail when it has them, and says how the piece was found (`Found::Tail`, `Found::Checkpoint` or `Found::Scan`,
with the bytes scanned). A scan stops at the first block that isn't good, and `PieceInfo::cut` names that offset.

A scan also reports what it read past. `PieceInfo::gaps` lists the blank or bad stretches the data goes on after, and
`PieceInfo::replaced` lists blocks that were written twice with other bytes. A piece of another format version has
`PieceInfo::version` set, and every read of it is refused with `ReadError::Version`.

A raw medium, such as an SD card, isn't read from its end, because an older recording's tail there may pass for the
newest piece. Open it with `TapeReader::open_with` and set `OpenOptions::medium` to `Medium::Raw`. The reader then
walks forward from the medium's start, or from the offset `OpenOptions::start` its medium layer gives. [Damage and
recovery](recovery.md#sessions-and-stale-data) explains why.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/seek.svg"
<figcaption>Above, a finished piece: the reader reads the tail (1), the finish (2), the final index's directory (3) and
one track's list of blocks, and then only the blocks it needs (4). Below, an unfinished piece: the newest checkpoint
and its back pointers list every block before it, and only the blocks after it are read one by one.</figcaption>
</figure>

## Finding blocks through the index

A reader finds blocks through the piece's index and never walks the whole file. Each block has an **index entry**
(`IndexEntry`): its offset, length, kind and track, its first item number and item count, its first and last tick,
and the latest end of any of its items. These calls give entries and read what they name:

- `TapeReader::entries(i)` gives every block of piece `i`, in file order.
- `TapeReader::track_entries(i, track_id)` gives the blocks of one track. In a finished piece it reads only the final
  index's directory and that track's list, each checked by its own CRC. A reader of one track out of thousands so reads
  a few hundred bytes of index. Track id 0 gives the record blocks of data, and `STATE_TRACK_ID` the state entries.
- `TapeReader::read_block(i, entry, buf)` reads the block an entry names and checks it.
- `TapeReader::read_chunk_head(i, entry, buf)` reads only the head of the chunk an entry names, up to its payload, and
  checks it by the head CRC. It tells what a chunk holds, and a series chunk's grid and scale, without its samples.
- `TapeReader::series_timeline(i, track_id, period)` reads the head of every chunk of a series track and builds its
  `SeriesTimeline`. It returns the errors of the heads it left out beside the timeline ([Events and
  series](events_and_series.md#grids-where-samples-are-in-time)).
- `TapeReader::state_entries(i)` gives piece `i`'s state entries, each with its index entry.
- `TapeReader::relation_breaks(track_id, owner)` lists the chunks of a track, over all pieces, where its monotone
  relations start again ([relation breaks](format.md#tape-blocks)).

An entry tells a reader whether it needs a block before it reads the block. For example, a query for the items
between two ticks takes the entries whose tick range overlaps them. A query for item 1 000 000 takes the entry whose
item range holds it.

## State at a place

Before a reader can decode items, it needs the definitions in force where the items are: what each track is, which
clock it counts and what type its items have. A track that holds state needs its item in force too. The format has no
"current state" of a whole file, because what is in force depends on the place. The reader looks it up through the
index, without reading the blocks in between.

Every state record has its own index entry, which holds its slot and the hash of its item. Three lookups use these
entries:

- `TapeReader::state_at(i, slot, offset)` gives the record of one slot in force at `offset` in piece `i`. That is the
  newest record of the slot whose block starts before the place.
- `TapeReader::definitions_at(i, offset)` gives every definition in force at a place, by slot (`DefinitionsAt`). Pass
  `u64::MAX` for the end of the piece.
- `TapeReader::item_by_hash(i, hash)` gives the newest record, of any slot, whose item has that hash. An application
  that refers to a state item by its hash finds it this way.

The writer keeps every state item near its use. The record a lookup finds therefore usually lies within `near` (32 MiB
by default) behind the place. For example, a viewer opens a week-long piece at 15:00 on Wednesday. It finds the offset
of that hour in the index and passes it to `definitions_at`. A few small reads give it the definitions it needs,
however much lies before.

Damage is a reason to look further, never an error. When the newest record of a slot doesn't read, the lookup lists it
as lost and takes the next older record. It also names the stretches the index doesn't cover (`uncovered`), such as a
gap a scan read past or the bytes past a cut, since a newer record may lie there. These are the stretches between the
record and the place for `state_at`, those before the place for `definitions_at`, and those after the record for
`item_by_hash`.

A lookup reads at most `OpenOptions::state_read_max` bytes (1 GiB by default). When that budget runs out, the lookup
stops and sets `spent`. The answer is then incomplete, but nothing in the file is damaged. `is_whole` says that nothing
was lost, uncovered or left unread.

## Reading items

How a reader gets at the items depends on the kind of block:

- **A record block.** `Block::records` walks its records in order: track, time and item bytes. A body coded with lz4
  (header flag `FLAG_BODY_CODED`) is decoded first with `Block::decode`, whose result has the same `records`.
- **An events chunk.** `Block::chunk` gives the chunk's head and columns, and `EventTimes::read` reads its times in
  place. `start(j)`, `end(j)` and `flag(j)` give item `j`'s start tick, end and flags. `starts().lower_bound(t)` gives
  the first item at or after tick `t`, and `starts().last_at_or_before(t)` the last item at or before it.
  `Payloads::read` gives each item's bytes with `get(j)`, without decoding the items before it. A raw payload column
  is read in place, and an lz4 one is decoded once, whole. `RelationColumn::read` gives the values of a relation the
  track stores per item.
- **A series chunk.** `Samples::of` takes the samples column, and `Samples::decode` decodes any range of samples from
  the group that holds its first sample. The chunk's grid gives each sample's tick and its scale. A
  `SeriesTimeline` collects the grids of a track's chunks, checks each chunk against its neighbours and says where the
  gaps are.
- **A summary block.** It holds coarse statistics of a long track, for an overview without reading the items.
  `Block::summary_head` reads its head, and the crate `ww_tape_summary` reads its nodes (`Summary::of_block`).
  [Events and series](events_and_series.md#summaries) describes summaries.

Item `j` of a chunk is item `first item number + j` of its track. A reader therefore goes from an item number to the
chunk through the index, and from the chunk to the item by its position. [Pieces and
files](pieces_and_files.md#addressing-one-item) explains why items are addressed this way.

## Checking a piece

`TapeReader::verify(i)` checks that finished piece `i` is the piece its writer wrote. It hashes the piece's bytes
before its finish and compares the result with the content hash the finish states. A match proves the piece unchanged
since it was written.

Verify also replays the piece block by block as its writer placed them. A writer that lied in its index or finish
therefore fails too. Verify checks these points:

- Each entry of the final index is the entry the writer makes of the block at its offset: offset, length, kind,
  track, items, ticks, a chunk's unit hash of its bytes, and the range of its range relation from its own column.
- The index and the walk are the same set. Every block walked that its writer lists is listed, except a
  byte-identical copy of the listed block before it, which adds to no track.
- Every entry names a block walked, or a damaged block that ends before the next block walked and before the finish.
- Each finish row states the hash its track's listed blocks fold to, starting from the head row's hash. A row may say
  the hash is incomplete, but only exactly where the head, or a damaged block a resume kept, leaves units out.
- Each monotone relation of a track starts each chunk at or above the chunk before it. A chunk marked as a [relation
  break](format.md#tape-blocks) may start lower. Any other decrease gives `ReadError::RelationOrder`, as
  `TapeReader::track_entries` gives for the range relation's ranges.

Verify costs a bounded number of steps per block and entry, and bounded memory. It reads the final index as it walks
the piece and merges the index's track lists, each in file order, into one stream. It so holds one entry per track
list and the entries of one block, whatever the size of the piece. A final index of more than `VERIFY_LISTS_MAX` track
lists names more tracks than a writer gives a piece, and doesn't hold.

`TapeReader::chain` checks the links between the pieces at hand. It checks each piece head against the piece before
it, and each tail's next piece id against the next piece. A piece without a finish has no hash to check, and that is
how a reader knows the piece wasn't closed.

## Damage and salvage

Every block carries a CRC-32C, and a reader checks it before it uses the block. Most damage therefore costs one block,
and the index still says what that block held: which track, which items and which ticks.

Inside a chunk, the reader can save more:

- A chunk's head has a CRC of its own, and its payload has a CRC per 64 KiB. When the head is good and the payload is
  damaged, `Block::salvage` hands out the chunk with its bad segments marked (`Block::is_intact` false,
  `Chunk::bad_segments`, `Chunk::range_damaged`).
- `EventTimes::salvage` and `Payloads::salvage` check an events chunk's columns group by group, each group of 128 items
  against its neighbours. Only the items of a group that fails are marked suspect (`is_suspect`). In a raw payload
  column, so are the items with a byte in a bad segment. A damaged lz4 payload column makes every item suspect. The
  other items read as usual.
- `FileBlocks` with `Recovery::Resync` walks a damaged file block by block and finds the next good block after a bad
  stretch. The default, `Recovery::Prefix`, stops at the first bad block.

[Damage and recovery](recovery.md) gives the full picture: what each kind of damage costs and what the format can't
protect against.
