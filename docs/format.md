---
level: advanced
---

# Format reference

The parts of a tape, block by block, in format version 1: how a file is written and read back, definitions and
state tracks, the index, finish and tail, pieces, resuming after a crash, reading by position, sessions and damage,
and the sample, series and payload codecs. [Tapes](index.md) introduces the
format, [Events and series](events_and_series.md) and [Damage and recovery](recovery.md) explain it with examples.

## Files

A tape file is [tape blocks](#tape-blocks) appended back to back. `ww_tape::TapeWriter` writes it the way databases
write their logs: a new file as `NAME.partial`, renamed when finished, the file and its directory synced after the
create and after the rename; the data synced by a `SyncPolicy` (none, every N blocks, or an interval, 1 s by default:
`fdatasync` on Linux, `F_FULLFSYNC` on macOS, `FlushFileBuffers` on Windows); and a failed write or sync is the end of
the file, never retried (after a failed `fsync` Linux may drop the dirty pages, and a retry succeeds without them).
`ww_tape::FileBlocks` reads the longest valid prefix: it stops at the first block with a bad magic, length or CRC,
a cut one or one whose seq goes back, and says where; blank bytes at the end (all 0x00 or all 0xFF) are the end. `Recovery::Resync` reads on past
damage instead. A file ends with an index, a finish block and a tail ([below](#index-finish-and-tail)); cutting a recording into several files is [tape pieces](#tape-pieces); preallocating space is not built yet. Track 0 can carry the producer's API
bundles as [definitions](#definitions-on-track-0), so a reader explains a recording on any PC without the producer
(WireWeaver's `ww explain FILE` does so for link captures).

## Tape blocks

`ww_tape` is a generic block format: nothing in it knows its producers (WireWeaver's link tap is one). A block is a 32 byte header
(magic `WWTP`, version, kind, length, record count, the block number in its session, the session tag), a body and a
CRC-32C, as long as its records and never padded, except a finished piece's finish block ([below](#index-finish-and-tail)). A session is one writer's unbroken append (a producer start, a
device boot): its blocks carry the same random 32-bit session tag (`ww_tape::SessionTag`, never 0 nor FFFFFFFFh,
derived from the piece id
for a piece chain so the same input gives the same bytes), and their numbers count from 0. The version byte is 1.
Block kind 1 is a records block; kind 2 is reserved (`RESERVED_KIND`), never written, and read as a kind the reader
doesn't know. The body of a records block starts with a clock table (the clocks its
records count), then records: a head (track and clock slot), a time delta, a length and the item bytes, as LEB128
varints. Record times are signed i64 ticks of the track's clock, each delta against the previous record on the same
clock, so a block reads alone and one clock costs no bit. Records on track 0 have no time delta. No wall time is in a block.

A **chunk** block holds one chunk of one track, read in place: an events chunk (up to a few thousand items of an event track) or a series chunk (a run of samples of a fixed-rate series). Its head says where it belongs without the definitions or an index: the track's identity hash and number, the first item number, the item count, the first and last tick and the latest end of any item (so an overlap query needs no decode of the ends), then the time and end modes, in a series chunk its grid (grid index, grid tick and the scale of its numbers, see [Series tracks](#series-tracks)), one entry per column (what it holds, its codec id, its channel, its offset) and a CRC-32C per 64 KiB segment of the payload. The columns follow back to back, then 16 zero bytes, so an unaligned 16-byte load at any column byte stays in the block; numbers are little endian. The head has a CRC of its own: when a payload is damaged, `Recovery::Resync` still trusts the head, skips the block by its length and hands it out with its bad segments marked (`Block::is_intact` false, `Chunk::bad_segments`, `Chunk::column_damaged`), so a flipped bit in a 4 MiB series chunk loses one 64 KiB segment. A block with a bad head is skipped one byte on, to the next magic. Block kind 5 is reserved for parity blocks; none are written yet.

A **summary** block holds a batch of consecutive nodes of one level of a track's summary tree, so an overview of a long track reads a few hundred KB instead of its items. Nodes run over item numbers, not time: a level-0 node of an event track holds 2048 items, each level up 16 nodes of the level below, and node `n` holds items `[n << shift, (n + 1) << shift)`, so the first and last node of a tape piece may hold fewer items and the nodes of two pieces line up. An event node holds its first and last item's start, the OR of the items' flags and, for spans, the latest end; counts follow from the item range. A series node holds `2^base_shift` samples and, per channel, the statistics the track declares (`SeriesSummarySpec`): minimum, maximum, sum, sum of squares, a count of clipped or NaN samples, the flags OR, and for complex samples the mean and maximum of `I² + Q²`; no ticks, since a series' ticks follow from its chunks' grids and its period. Every statistic merges in item order (`EventNode::merge`, `ChannelStats::merge`), so a reader building coarser bins gets the bits the writer stored one level up. A chunk may change the scale of a series' numbers mid-node (an autoranging device changes prefix); such a node is kept at the common scale of its samples (`common_scale`: `10⁻⁶` for `10⁻³` and `10⁻⁶`), the coarser numbers multiplied by a whole factor, so minimum and maximum stay exact integers in the sample type's unit. A series block states its scale in its head, the common scale of all its nodes, so it is read alone without the chunks; `SeriesNode::merge` merges nodes of two scales (two blocks, a view cache across a change) at their common scale, and refuses scales with no common one (a ratio that isn't a fraction of integers) rather than mixing them. The head names the track, the level, the node size, the element, channels and scale of a series, the statistics stored, the items covered and their tick range, so an index lists the block without reading its body; the body is one column per field, little endian. The writer (`EventTree`, `SeriesSummaries`) writes a block when a level's batch fills and the partial nodes at `finish`, up to a level with a single node. The blocks are small (a few KiB) and go through `TapeWriter::defer_block`, so in an aligned file they fill the gaps before chunk payloads and cost no space; a lost one is rebuilt from the data. `SeriesSummaries::push` keeps every statistic in one pass over the samples: integers up to 32 bits through vectorised kernels with exact integer sums (AVX-512 or AVX2 chosen at run time on x86-64), 64-bit integers and floats one sample at a time in their order, since float sums depend on it. On an AMD EPYC Genoa core it summarises complex i16 at about 5 GB/s and i32 at about 7 GB/s (`just bench-summaries`).

The **times** of an events chunk are coded by `ww_tape::times`, and every read runs on the chunk bytes as they are, with no decode into a list first. Starts are either *implicit* (the chunk's first tick plus a period per item, for evenly spaced items) or a *linear frame of reference* per group of 128 items: a 25-byte record per group (its first tick, a slope in Q16 ticks per item, the smallest residual, a width and an offset), then each item's residual against the line, bit-packed at the group's width. Periodic items cost their jitter, not their period: 1 ms with ±300 ns jitter takes about 1.48 bytes per item, exactly periodic ones almost nothing, bursty nanosecond traffic about 3.8 (8 for raw i64). A get is one record and one load, a lower bound searches the group first ticks, then only the items the slope leaves possible. Ends are absent (instants), the next item's start (spans back to back, nothing stored) or packed durations, whose group minimum plus 2^width bounds the group's longest item for overlap queries. Flags are an optional u32 per item, packed per group with the OR of the group's flags, so a zoomed-out view reads the ORs without a decode. Groups decode with hand-written kernels (`ww_tape::bitpack`: AVX-512, AVX2 or NEON, picked at run time, else scalar) at a fraction of a nanosecond per item. A chunk whose CRC failed still reads through `EventTimes::salvage`: group records are checked one by one, and only the items of a group that doesn't hold up are marked suspect.

A file states how its blocks are placed in its layout definition (`Layout`, alignment 0 or 4096; none means 0). In an aligned file (series files) every chunk payload starts on a 4 KiB boundary: the chunk's head sits in front of it, small blocks waiting for a place (`TapeWriter::defer_block`) fill the gap before the head, and what is left is zeros inside the head. Recovery looks for a block at every byte, aligned or not, and takes one only when its CRCs match.

## Definitions on track 0

**Definitions** say what a reader needs before the data. Each is a record on **track 0**, which is reserved for
them: one `Describe` per record, the item's whole bytes, with no time delta. A definition is one of:

| Definition | Says |
|---|---|
| `Producer` | the producer's name |
| `Layout` | how blocks are placed: alignment, `gap_max`, `resume_align`, and the `StatePolicy` of track 0 (`definitions`) |
| `Bundle` | an API bundle (a `ww_self` `ApiBundle` in its ShrinkWrap encoding) and its hash, the hash as the producer, `TrackDef::bundle` and `TypeRef::bundle` name it |
| `Clock` | a `ClockDef`: number, stable key, exact rate and resolution as `Magnitude`s, origin (free-running, boot, Unix epoch, media start, session start, or the next epoch of a counter, fixed by an exact offset map) |
| `Map` | a `ClockMap` of segments `b = b0 + (a - a0) · slope`, the slope an exact_units `Ratio64`, with an error bound, strictly increasing across segments; the tap maps its own clock to the wall clock read at its start |
| `Track` | a `TrackDef`: number, stable key, clock, dimensions that are part of its identity, API, bundle, resource path, the keys of [keyed arrays](#track-identity) on it, optional item type, label, its `repeat` (`TrackRepeat`) and its kind: events, with or without ends and flags, or a [series](#series-tracks) |
| `Item` | a `TypedItem`: any wire_weaver value by bundle hash and type path, such as a run record |

Definitions sit in records blocks of their own, with no record of another track, each at most `DEFINITION_BLOCK_LEN`
(64 KiB) unless one record is longer. `Describe` is an evolvable enum: a variant a reader doesn't know reads as
`Describe::Unknown` and is skipped.

Each definition fills a **slot** (`ww_tape::Slot::of`): the producer and the layout one each, a track and a clock by
its number, a map by its two clocks. A bundle, a typed item or a variant the reader doesn't know is a slot of its own,
by its hash. The definition in force at a place is the newest record of its slot behind it.

A writer writes definitions only as [puts](#state-tracks). The application holds the definitions it works with in a
`Definitions` (`define`, `define_all`, `close`) and passes them with each write; data on a track not held is refused,
and so is a caller's block of definitions (`DescribeError::DefinitionBlock`). Before a block of data the writer puts
the definitions its tracks need: the clocks, maps and bundles a track names, then the track. The producer and the
layout are the writer's own (`WriterOptions`): it puts them after every piece head, before the first block of a file
without one, and where a caller's block starts another session. Definitions go in the order they are put, so the
same input gives the same bytes.

## State tracks

Track 0 and every track whose `repeat` is `TrackRepeat::State` hold **state**, not events: a reader at any place
takes the newest record of each slot behind it. A state track's records fill one slot, the track's. A
`StatePolicy` says how a writer keeps such records near their use: `near` (32 MiB by default) is how far behind
a use the item must lie in the same piece, and `copies` (1 by default) how many more records an item gets after its
first one in a piece. Track 0's policy is in the layout (`Layout::definitions`). A series track has no records and
takes the plain policy: its chunks are data and carry their own timing ([Series tracks](#series-tracks)).

An item is known by its **hash** (`ItemHash`): BLAKE3 of its bytes, cut to 128 bits. A **put** writes an item unless
the newest record of its slot is the same item at most `near` bytes behind, measured to where the data lands. The
application puts a state track's item at each use (`TapeWriter::put`, `put_item`); a caller's records on a state
track in a block of data are refused. The writer puts definitions the same way before every block of data. A put of
an item that lies near behind still writes it as a **copy** while fewer than `1 + copies` records of it lie in the
piece and the newest lies at least `NearPolicy::copy_spacing` (1 MiB) behind. Copies are written only at uses,
never ahead of them. The writer remembers at most `NearPolicy::memory_max` slots and forgets the one written longest
ago first: a forgotten slot costs one more write, never a wrong read.

Puts and copies go in records blocks marked **`FLAG_REPEAT`** (header bit 1). Such a block carries the seq of the
block before it and is not counted, so the producer's seq stays its own count. A reader holding an item of the same
hash in the same slot takes the record as nothing new. A block of puts that belongs with the block after it (the
block of data its puts serve, or the next block of a put too long for one) is also marked **`FLAG_WITH_NEXT`**
(header bit 2, only on a block marked `FLAG_REPEAT`). A run of such blocks ends in the block they belong to, all from
one writer call. `TapeWriter::put_block` puts a producer's block of puts by the writer's own rule;
`relay_block` passes a live producer's stream on.

`TapeWriter::defer_block` puts the definitions of a deferred block when it is deferred and places the block later
with no puts. A deferred block waits at most `DEFER_WINDOW` (1 MiB) of writes, so it lies at most its tracks' `near`
plus that window after their definitions. A cut and a finish place every deferred block first.

## Track identity

A track's stable key is the same for the same resource in every file and every session of a producer, whatever ids or
indices that session used. `ww_tape::track_key` hashes the resource's **name path** under its API: `api_root_id` of the
API's crate name (no version: a breaking change is a new generation of the same API), then one `Segment` per step,
each with a tag byte and its length, so `a.b` and `ab` never collide:

| Step | Segment |
|---|---|
| a resource, `link`, `rx_frames` | `Name`: its identity name; a resource renamed with `#[moved_from]` keeps its oldest name |
| an index into a plain array, `link[2]` | `Index(2)` |
| an element of a [keyed array](../api/arrays.md), `flow[{key}]` | `Key`: the key's shrink_wrap bytes, not the index |

So `link[2].rx_frames` of the tap is `track_key(api_root_id("ww_tap"), &[Name("link"), Index(2), Name("rx_frames")])`
(`ww_tap::track_key(2, ids::RX_FRAMES)`). Two sessions of a dissector that number flows in another order give each
flow's tracks the same key, since its key, not its index, is hashed.

`TrackDef.path` stays numeric; `TrackDef.keys` holds the key bytes for each index into a keyed array, by its
position in `path`. `ww_tape_bundle::name_path` builds the name path from the API bundle, the numeric path and the
keys, and `key_in_bundle` the key under the bundle's own crate: a reader rebuilds the name of every track and checks
its key. A producer that sees an element of a keyed array before its key reads `key[i]`
first and writes the track's definition when it knows it.

The key's shrink_wrap bytes change when its type gains a field, even a defaulted one, so every keyed track then gets
a new key. Before the first long-lived recordings this is to be revisited: a canonical hash of the key's value, which
leaves out fields at their default, would survive such a change. A resource renamed within its trait keeps its key;
one moved to another trait gets a new name path, and with it a new key, even with `#[moved_from]`.

## Index, finish and tail

A writer lists what it places. Every few MiB (`ww_tape::IndexPolicy`, 4 MiB by default; by time only when asked, since that makes the bytes depend on the clock) it writes a checkpoint **index** block: one 72-byte entry per block placed since the previous checkpoint (offset, length, kind, track id and number, first item number, item count, first and last tick, latest end; a summary block's level and tree), and a pointer back at that checkpoint. Chunk, summary and record blocks are all listed; a record block of data carries no track id or ticks, only its record count under track id 0. In an aligned file a checkpoint goes into the gap before the next chunk payload. Blocks the file structure adds (index, finish, tail, piece head) and blocks marked `FLAG_REPEAT` carry the seq of the block before them and are not counted, so seq stays the producer's count of its own blocks and a pass-through capture keeps its order. A crash costs at most the chunks not yet written, and a reader rescans only the stretch after the newest checkpoint.

A block of state records (a block whose first record is on track 0, or any block marked `FLAG_REPEAT`) gets one
**state entry** per record instead, under track id `STATE_TRACK_ID` (`u64::MAX`), so a reader finds the item in
force at a place, or an item by its hash, without reading the blocks. A state entry holds, beside the block's offset,
length and kind:

| Bytes | Field |
|---|---|
| 13 | the slot: 0 a state track's record, else what the definition defines (`DefinitionKind`: 1 producer, 2 track, 3 bundle, 4 clock, 5 map, 6 item, 7 layout, 15 unknown) |
| 15 | bit 0: the block is marked `FLAG_REPEAT` |
| 24..28 | the state track, or the number the definition defines (a track's, a clock's, a map's from clock) |
| 28..32 | a map's to clock, else 0 |
| 32..40 | 0, reserved for the record's item number |
| 40..48 | 0, reserved |
| 48..64 | the item's hash (`ItemHash`) |
| 64..72 | the record's time in ticks, 0 on track 0 |

A piece lists at most `IndexPolicy::state_entries_max` state entries (1 Mi by default, 72 MiB of final index); a
write past that is refused (`IndexError::StateEntries`), and `PieceChain` cuts to a new piece.

`TapeWriter::finish` ends the file: a **final index** listing every block again sorted by track (state entries of one block in block order), with a directory of tracks in its head and a CRC per track's entries (a reader of one track out of thousands reads the head and that track's list), the **finish** block (complete, continued or aborted; the BLAKE3 content hash of every byte before it; where the newest checkpoint and the final index are, offset 88 reserved; per track the chunks, items and ticks the file holds) and a fixed-size **tail** of 164 bytes as the last bytes of the file, which says where the file's piece starts and where its finish is. Nothing is repeated at the finish. Offsets count from the start of the piece, so the index stays valid when files are concatenated. A lost index block costs a scan to the neighbouring checkpoint; a lost tail or finish only the content-hash check.

A finished piece is a multiple of `PIECE_ALIGN` (4 KiB) long. The writer pads the finish block's body with zeros after its tracks (`piece::finish_padding`), states the padding at finish offset 172, and the CRC covers it; the content hash still covers the bytes before the finish, and the tail stays the piece's last bytes. Finished pieces so clone back to back into one file (reflinks on btrfs or XFS) with nothing between them. Only the end is aligned: the blocks inside stay where they are, and an unfinished piece ends where it ends.

## Tape pieces

A recording can be cut into **pieces**, each a complete file that reads alone: a piece head block, the layout and the producer, the data with the definitions and state it needs put close behind it, its checkpoints, the final index, the finish and the tail. `ww_tape::PieceChain` writes `dir/00000000.wwt`, `00000001.wwt`, ... and cuts before a chunk by size, chunk count or age (`CutPolicy`); `TapeWriter::cut` cuts by hand. The head names the owner, the piece's sequence number, its id, the previous piece's content hash and id, and per track where its items continue; the previous piece's finish and tail name this piece's id, derived at the cut from the previous piece's id and the content hash in its finish, so the chain is checked both ways (`ww_tape::piece::check_chain`). Pieces written back to back are one valid tape. A chain whose first pieces were deleted (a rolling window) reads as *head cut*, not as damage; one whose last piece has no tail is open (being written, or cut off by a crash).

## Resuming after a crash

[Damage and recovery](recovery.md#resuming-after-a-crash) explains it with examples.

`TapeWriter::resume` reopens an unfinished piece (`NAME.partial`) and goes on writing it. It finds the piece as `TapeReader` does, then walks its bytes from the start to where the reader stopped, block by block, and rebuilds the writer as it was: the content hash, the index entries, the track ranges, the state records each block holds (the writer's memory of what lies near), the counts, the session and the next seq. A damaged block the index lists stands for what its entries say; a state record in it leaves its slot unknown, so the next put writes it.

The piece is kept up to the end of its last good block, counted or not. What ends a piece (final index, finish, tail) is dropped, and so are blocks marked `FLAG_WITH_NEXT` at the end of what is kept: the call they belong to is given again whole and writes them again. Since what the writer adds depends only on the blocks before it, the calls since and its `WriterOptions`, the same calls after a resume give the bytes and content hash of a piece written without the crash. A put is idempotent, so an application puts again what it uses and needs no record of which puts made it. The writer keeps no definitions: the application passes its own with each call after the resume. The layout and the producer of the options are checked against those the piece names, before anything is cut off or written (`WriteError::Layout`, `WriteError::Producer`). `PieceChain::resume` also reads the finished pieces back and handles a crash between a cut's finish and the next piece.

## Reading by position

`ww_tape::TapeReader` reads a tape through a `ByteSource`, anything with a size that reads bytes at an offset: a byte slice, a `File`, or an HTTP resource read by range requests. It needs no file system and no threads, so it builds for `wasm32-unknown-unknown` (feature `alloc` without `std`). It finds the pieces from the end: a good tail gives a piece's start and finish, and the previous piece's tail sits just before that start, so concatenated pieces cost one small read each. A piece without a tail is found from its newest checkpoint, looked for backwards from the end, and the stretch after it is read block by block; a piece without any checkpoint is read from the start. `entries` and `track_entries` give where a piece's blocks are, `read_block` reads and checks one, `verify` checks the content hash, `chain` how the pieces link up. A raw medium is read forward instead (`OpenOptions::medium`, [below](#sessions-media-and-damage)).

State is looked up by the index's state entries, not by a scan: `state_at` gives the record of a slot in force at a place, `item_by_hash` an item by its hash, `definitions_at` every definition in force at a place, `state_entries` a piece's entries. A lookup takes the newest entry of the slot whose block starts before the place, and the next older one when that block doesn't read or its record isn't the one the entry names. Damage is never an error: what didn't read is listed, and so are the stretches between the record and the place the index doesn't cover (`uncovered`: a gap a scan read past, the bytes past a cut), since a newer record may lie there. The reader keeps the sorted entries in one cache over all pieces, at most `OpenOptions::state_cache_max` entries (`STATE_CACHE_MAX`, 2 Mi, by default), the piece used longest ago out first. A lookup reads at most `OpenOptions::state_read_max` bytes (`STATE_READ_MAX`, 1 GiB, by default) and charges a coded body's decoded length before it decodes it; when the budget runs out it stops and sets `spent`, which is not damage.

The header and the CRC at the end stay the same in every version: a later version changes only the body. A block of another version that goes on with the data ends the piece the reader is in and starts a piece of that version, which the reader lists with its version (`PieceInfo::version`) and refuses to read. A reader skips a block of a kind it doesn't know by its length, as it skips definitions of a variant it doesn't know.

## Sessions, media and damage

[Damage and recovery](recovery.md) explains these rules with examples.

A **session** is one writer's unbroken append: a producer start, a device boot, one pass of an importer. Every block of a session carries the same session tag in header bytes 24..28, and its seq counts from 0. A writer picks the tag at random, except for the first piece of a chain, whose tag is derived from the piece id so the same input gives the same bytes. A piece head names the session that ended before it (`PieceHead::prev_session`). A reader keeps to one session at a time: a block of another session is stale data, unless a session may start there. In a file a new session starts at a records block whose first record is on track 0 (another producer appended) or a piece head; on a raw medium only at a piece head that names the current session as the one before it. A piece on a raw medium is therefore one session: a writer that carries a producer's blocks, each with the producer's own session, renumbers them into its own session with `TapeWriter::adopt_block`. Lost blocks stay visible: a jump in the producer's seq leaves the same jump in the writer's, and a producer restart (another session) leaves none. Passed through with `write_block`, they read whole from a file but as stale data on a raw medium.

The **medium** decides where a reader starts. A file is read from its start, or its pieces are found from its end. A raw medium (an SD card, raw NAND, a partition, `Medium::Raw`) may hold blocks of earlier recordings behind the end of the current data, so a reader walks it forward from its start or from a hint and never trusts what it finds at the end. For example, a card recorded twice holds the second recording's blocks followed by the tail of the first: the first recording's session tag ends the walk.

**Blank** space is bytes that are all 0x00 or 0xFF, as an erased or preallocated medium reads. A blank or damaged stretch is a **gap**, not the end, when the current session's next block follows it within the layout's `gap_max`, found at the first non-blank byte or at a multiple of `resume_align`. The layout definition states both (`Layout`):

| Medium | `gap_max` | `resume_align` |
|---|---|---|
| file (`Layout::UNALIGNED`, `ALIGNED`) | 0: no gaps | 1 |
| SD card or eMMC (`on_sd_card`) | 4 MiB | 512 bytes, a sector |
| raw NAND (`on_raw_nand`) | two erase blocks | one page |

A reader's cost per gap is bounded. A valid `Layout` has a `gap_max` of at most 64 MiB (`GAP_MAX_MAX`) and at most 8192 resume places in it (`GAP_PROBES_MAX`, `gap_max / resume_align`); a layout definition of any other doesn't decode. Past a blank or bad stretch a reader reads one block header per resume place, and checks a block whole only when its header says it goes on with the data, at most 4 times per gap (`GAP_CHECKS_MAX`). When none of them does, the stretch is the end of data. On a raw medium, blank space longer than `gap_max` is the end too, and a reader reads nothing after it: an erased 64 GB card costs one gap's worth of reads.

A writer on raw NAND programs each page once, in page order: at a sync it pads the rest of the page with 0xFF and starts the next block on the next page. The padding reads as a gap, so the format needs nothing else; the cost is up to one page per sync. Keeping MLC paired pages of unsynced writes apart from synced data is the medium layer's task.

The same session and seq twice is a **duplicate**, not a cut: a writer that retried a write may leave a block twice. With the same bytes the second one is dropped; with other bytes the later one is taken and reported (`FileError::Replaced`, `PieceInfo::replaced`).

Header **flags** (bytes 6..8) mark three things; a reader ignores bits it doesn't know. Bit 0 (`FLAG_BODY_CODED`) marks a records body as coded: a codec byte (0 stored, 1 lz4), the decoded length and the coded bytes. Coding is off unless a writer calls `ww_tape::code_body`; a reader decodes with `Block::decode` and takes a body of a codec it doesn't know as a body it doesn't know. Chunk blocks are never coded, since their columns are read in place. Bit 1 (`FLAG_REPEAT`) marks a block of state records a writer put or copied, and bit 2 (`FLAG_WITH_NEXT`) a block of puts that belongs with the block after it ([State tracks](#state-tracks)).

**State near its use** keeps a piece readable when a block of definitions is damaged: a reader at any place finds what it needs within `near` behind, and an item first written in a piece gets `copies` more records at later uses, at least 1 MiB apart. A lookup that meets a damaged block lists it and takes the next older record of the slot ([Reading by position](#reading-by-position)).

The **item of an events record** whose track has spans or flags is an `EventRecord`: the duration (spans only), the flags (only when the track has flags), then the payload, the record's time being the start. A track of plain instants carries the payload alone. State records of events tracks carry the same item.

A chunk column whose codec byte has bit 7 set is **outer-coded**: an outer codec byte, the decoded length, then the bytes the inner codec (bits 0..6) applies to after decoding. No outer codec exists yet, so every reader refuses such a column. In general a reader refuses a column whose kind or codec it doesn't know and still reads the chunk's other columns.

Some numbers are reserved so later features need no format change; today's readers refuse each:

| Reserved | For |
|---|---|
| payload codec 49 (`Codec::SERIES`) | a payloads column holding one shrink_wrap `Series<T>` of the chunk's items, alone, at most 65535 items |
| sample codec 36 (`Codec::RUNS`) | run-coded discrete lanes (`U8`, `U16`, `U32`, `Bool`) |
| element 13 (`Element::LEAVES`) | a sample that is a struct of leaves of different element types |
| column kind 7 (`ColumnKind::PAYLOAD_LEAF`) | one leaf of the items' payload struct, the param naming the leaf |
| summary head bytes 54..56 | the first channel or leaf of the block, 0 today |

## Sample codecs

[Events and series](events_and_series.md#series) explains series, sample codecs and why fixed point comes
first.

A series chunk's sample columns (`ColumnKind::Samples`) are coded by the column's codec id; what a sample is comes from the track: `ww_tape::SampleShape`, an `Element` (`I8` to `U64`, `F32`, `F64`, `ComplexI16`, `ComplexF32`, `Bool`; its code, 0 to 12, is how other blocks name it) and the channels interleaved in the column. `ww_tape::Samples` reads a column in place and decodes any range of samples; `ww_tape::samples::encode` writes one. Every codec gives the same bytes for the same values.

| Id | Codec | Elements | Stored as |
|---:|---|---|---|
| 0 | `RAW` | all | little endian; `Bool` 8 per byte, the first in bit 0 |
| 16 | `BIG_ENDIAN` | integers, floats | big endian |
| 17, 18 | `PACKED10_LSB`, `PACKED10_MSB` | integers of 16 bits and more | 10 bits per value |
| 19, 20 | `PACKED12_LSB`, `PACKED12_MSB` | integers of 16 bits and more | 12 bits per value, 2 values in 3 bytes |
| 21, 22 | `PACKED24_LSB`, `PACKED24_MSB` | integers of 32 bits and more | 24 bits per value |
| 24, 25 | | | reserved: refused as an unknown codec |
| 26 | `ALP` | floats | decimals as integers, frame of reference, bit-packed; other floats by their front bits through a dictionary |
| 32, 33 | `DELTA`, `DELTA2` | integers | differences (or differences of differences), minus their minimum, bit-packed |
| 34, 35 | | | reserved: refused as an unknown codec |
| 48 | `LZ4` | payload columns | one lz4 block after its decoded length (`ww_tape::lz4`) |

- **Raw device formats** (0 to 22) are what a device sends, stored as it is: the reader converts only the samples it reads, and any sample reads in place. An `_LSB` column holds value `i` at bits `i·w` up of the column read as one little-endian number; an `_MSB` column counts bits from the top of byte 0. A packed value of a signed element is sign-extended.
- **Delta codecs** (32, 33) code groups of 128 samples on the bit-pack primitive the event time codec uses (`ww_tape::bitpack`). Per lane, a group holds the first value, the minimum difference and a width, then the differences minus that minimum at that width, so a ramp or a constant costs no bits per sample.
- **ALP** (26) follows ALP (Afroozeh, Kuffó and Boncz, SIGMOD 2024) in groups of 128 samples. Floats that began as decimals (a reading times 0.1, a price, a value printed and parsed back) are stored as integers: per group and lane one exponent `e` and factor `f`, each value `d` such that `d × 10^f × 10^-e` gives its bits back exactly, the `d`s minus their minimum bit-packed. Values that don't come back bit for bit (NaN payloads, `-0.0`, infinities, more digits) are kept whole as exceptions inside the group. Other floats take ALP's second scheme, `rd`: up to 16 front bits through a dictionary of up to 8 entries, the rest bit-packed without the low zero bits the group shares. The encoder keeps per group the smallest form, so a group never grows by more than 7 bytes over raw; the layout is in the `ww_tape::samples::alp` docs.
- **Groups.** The grouped codecs start the column with a table of where each group ends, and every group restarts its prediction. A range decodes from its group's start, and damage costs only the groups it touches: `Samples::group_bytes` names a group's bytes, to check against `Chunk::range_damaged`.

For data a device controls, fixed-point numbers are the default (an exact_units `Fixed` in an integer column with `DELTA`, see [series](../serdes/series.md)): they take the fewest bits and decode fastest. ALP, the one float codec, is for float sources nobody controls. Ids 24, 25, 34 and 35 are reserved; `ww_tape_benchmarks` keeps Gorilla, Chimp and delta on ordered float bits as bench-only codecs to compare against. The codec id is stored per chunk, so a writer may try several on a chunk and keep the smallest.

`just bench-sample-codecs` measures every float codec on four kinds of data, each next to the same quantity stored as fixed-point integers with `DELTA` (first row of each set). Ratio is to the raw floats, in chunks of 4096 samples; times are per value on one core (AMD Ryzen AI 9 HX 370, AVX-512 kernels):

- **motor_sim**: every variable of `motor_sim`'s VESC preset (github.com/vhrdtech/motor_sim at 35eab07, the commit `Cargo.lock` names) for 1 s at 25 kHz, driven from its rotor angle with a fan load, as `f32` and as `f64` (its `f32` variables widened). Fixed point: `i32` at the finest power-of-ten step that keeps 24 bits at each variable's peak.
- **speech**: Roosevelt's Pearl Harbor address (Wikimedia Commons, public domain, `Roosevelt_Pearl_Harbor.ogg`, SHA-1 `90239988...`), decoded to `f32` by lewton. Fixed point: lewton's 16-bit PCM of the same decode.
- **ADC**: 12-bit counts of 4 slow noisy channels, 250 000 samples each, turned into floats as firmware does: `counts × 3.3/4096` and `counts × 0.1`. Fixed point: the counts.

| Data | Codec | Ratio | Bits/value | Encode ns | Decode ns |
|---|---|---:|---:|---:|---:|
| motor_sim `f32` | `DELTA` of `i32` | 3.10 | 10.3 | 2.1 | 0.66 |
| | `GORILLA` | 1.49 | 21.5 | 5.4 | 2.42 |
| | `CHIMP` | 1.74 | 18.4 | 7.0 | 3.78 |
| | `FLOAT_DELTA` | 1.93 | 16.6 | 2.3 | 0.86 |
| | `FLOAT_DELTA2` | 1.99 | 16.1 | 2.5 | 1.02 |
| | `ALP` | 1.64 | 19.5 | 14.1 | 1.17 |
| motor_sim `f64` | `DELTA` of `i32` | 6.20 | 10.3 | 2.1 | 0.65 |
| | `GORILLA` | 2.51 | 25.5 | 5.9 | 2.60 |
| | `CHIMP` | 2.50 | 25.6 | 11.3 | 2.87 |
| | `FLOAT_DELTA` | 1.50 | 42.6 | 2.4 | 1.29 |
| | `FLOAT_DELTA2` | 1.56 | 41.1 | 2.5 | 1.28 |
| | `ALP` | 2.51 | 25.5 | 17.6 | 1.24 |
| speech `f32`, 2 channels | `DELTA` of `i16` PCM | 3.43 | 9.3 | 2.3 | 0.71 |
| | `GORILLA` | 1.02 | 31.3 | 5.7 | 2.30 |
| | `CHIMP` | 1.23 | 26.0 | 8.2 | 5.59 |
| | `FLOAT_DELTA` | 1.10 | 29.1 | 2.5 | 0.96 |
| | `FLOAT_DELTA2` | 1.10 | 29.0 | 2.7 | 1.13 |
| | `ALP` | 1.16 | 27.6 | 15.7 | 1.27 |
| ADC × 3.3/4096 `f32` | `DELTA` of the counts | 7.31 | 4.4 | 2.2 | 0.65 |
| | `GORILLA` | 1.51 | 21.3 | 5.6 | 2.71 |
| | `CHIMP` | 1.76 | 18.2 | 10.5 | 6.87 |
| | `FLOAT_DELTA` | 1.84 | 17.4 | 2.4 | 0.81 |
| | `FLOAT_DELTA2` | 1.73 | 18.5 | 2.6 | 1.08 |
| | `ALP` | 1.59 | 20.2 | 15.1 | 1.17 |
| ADC × 3.3/4096 `f64` | `DELTA` of the counts | 14.63 | 4.4 | 2.2 | 0.65 |
| | `GORILLA` | 1.36 | 47.1 | 7.9 | 3.19 |
| | `CHIMP` | 1.57 | 40.8 | 13.1 | 7.28 |
| | `FLOAT_DELTA` | 1.37 | 46.7 | 2.5 | 1.32 |
| | `FLOAT_DELTA2` | 1.34 | 47.8 | 2.7 | 1.28 |
| | `ALP` | 1.32 | 48.6 | 21.9 | 1.59 |
| ADC × 0.1 `f32` | `DELTA` of the counts | 7.31 | 4.4 | 2.2 | 0.65 |
| | `GORILLA` | 1.53 | 21.0 | 5.5 | 2.67 |
| | `CHIMP` | 1.81 | 17.7 | 10.5 | 6.85 |
| | `FLOAT_DELTA` | 1.84 | 17.4 | 2.5 | 0.83 |
| | `FLOAT_DELTA2` | 1.73 | 18.6 | 2.6 | 0.96 |
| | `ALP` | 4.67 | 6.9 | 8.5 | 0.81 |
| ADC × 0.1 `f64` | `DELTA` of the counts | 14.63 | 4.4 | 2.2 | 0.65 |
| | `GORILLA` | 1.38 | 46.5 | 7.8 | 3.15 |
| | `CHIMP` | 1.59 | 40.3 | 13.1 | 7.27 |
| | `FLOAT_DELTA` | 1.37 | 46.7 | 2.5 | 1.36 |
| | `FLOAT_DELTA2` | 1.34 | 47.8 | 2.7 | 1.32 |
| | `ALP` | 9.02 | 7.1 | 8.9 | 0.72 |

What the numbers say:

- Fixed point is 1.6 to 9.3 times smaller than the best float codec and decodes fastest; on the motor its step keeps more precision than the `f32`s it replaces.
- `ALP` is the float codec for floats that began as decimals: 2.5 to 5.7 times smaller than the others there, decoding at bit-pack speed (0.7 to 0.8 ns). On widened `f32`s it matches `GORILLA`; on floats with full mantissas it ranges from 6 % ahead of the delta codecs (speech) to 18 % behind them (motor `f32`).
- The XOR codecs win on two sets, by 6 % (speech) and 15 % (ADC × 3.3/4096 `f64`), both `CHIMP`, and decode 2 to 10 times slower than the bit-packed codecs.
- A writer that tries `ALP` and `FLOAT_DELTA` on each chunk and keeps the smaller is the best float codec on four of the seven sets and within 13 % of the best on the others.

The bench runs in about 20 s: `just bench-sample-codecs` fetches the speech file into `target/bench-data` once, checks its hash, and prints the table; `--chunk` and `--repeats` change the chunk size and the timed passes. Its code is `ww_tape_benchmarks`.

## Series tracks

A series track holds samples at an exact period with no time per sample. Its `TrackDef` kind is a `SeriesDef`: the type of one value as the API declares it (a `TypeRef` to an exact_units `Fixed<Ampere, Milli, i32>`, a `ww_si` quantity or a plain float), the stored element and channels derived from that type, the period in ticks per sample as an exact `Ratio64` (48 kHz on a 1 ns clock is `62500/3`), the scale of the stored numbers as a `Magnitude` (`10⁻³` for that `Fixed`), the summaries the writer builds (`SeriesSummarySpec`) and, for samples kept in a media file, the stream. `SeriesDef::of::<Fixed<Ampere, Milli, i32>>(..)` takes the element and the scale from the type (`ww_tape::SeriesValue`).

A series track has no records: its samples are in series chunks, and each chunk's head carries the grid its samples are on (`SeriesGrid`), so a chunk times and scales its samples alone. The grid is a sample index at or before the chunk's first sample, that sample's tick and the scale of the chunk's stored numbers: sample `i` is at `grid_tick + (i - grid_index) · period`, rounded down, and a stored number means `number · scale` of the sample type's unit. It takes 17 bytes and the scale, 19 bytes for a power of ten (`10⁻⁶` is `f4 00`). The head's first and last tick are its first and last sample's on that grid (`SeriesGrid::of_chunk` checks them), so the index, which lists each chunk's first sample and first tick, finds the chunk of a sample or a time without reading a head. Consecutive chunks of one stretch of recording carry the same grid, so no sample time moves by rounding however the samples are cut into chunks. A prefix change of an autoranging device keeps the grid at another scale (`SeriesGrid::rescaled`): a chunk has one scale, so a writer seals the open chunk there. A gap or a restart on the same clock starts a new grid (`SeriesGrid::start`) more than a period, rounded up, after the last tick of the chunk before (`series::is_gap`). The chunks are data like event chunks, written once and never repeated, so a series track takes the plain policy (`TrackRepeat::Stream`); a writer refuses a record, a put or an events chunk on it, and a series chunk whose ticks are not on its grid.

A **gap** is a chunk on another grid than the chunk before it: its first sample lies more than a period, rounded up, after that chunk's last tick, and no sample lies between them, so a reader draws no line across. The next sample on the same grid lies at most that far on, so the first and last ticks of two neighbouring index entries tell a gap from a continuation or from lost chunks without reading a head; a new grid less than that far on (a phase shift) is refused. Sample indexes missing between two chunks of one grid are lost samples (a chunk damaged, or in another piece): their ticks are known, their values are not, and a reader shows them as missing. `ww_tape::SeriesTimeline` collects a track's chunk heads in any order (`from_chunks`, `push_chunk`), checks each against its grid and its neighbours (no sample on two grids or at two scales, a new grid more than a period after the chunk before and before the chunk after) and keeps consecutive chunks of one grid and scale as one `SeriesStretch`; it gives any sample's tick and scale and says where the gaps are (`gap_before`). A received `Series<Fixed<..>>` batch goes into chunk columns as integers: each item's stored number is put into the column as is (`SeriesValue::write_parts`), and a `ww_si` quantity's prefix becomes the grid's scale, a change of prefix sealing the chunk where the `Series` restarts its numeric column. The wire codecs of `Series<T>` and these sample codecs are separate layers: a batch is decoded once on the host and coded again for random access in the file.

## Payload codec

An events chunk's payloads take two columns, both or neither (`ww_tape::payloads`): `EncodedPayloads::encode` writes them from the items' bytes, `Payloads::read` reads them back by item.

- **Lengths** (`ColumnKind::PayloadLengths`, codec 4, `payloads::LENGTHS`): per group of 128 items a 13-byte record (where the group's first payload starts, the group's shortest length, a width of 0 to 32 bits, where its packed lengths start), then each group's lengths less that minimum, bit-packed at its width. Payloads of one length cost the records only, and the record's start places a group's payloads without the groups before it.
- **Payloads** (`ColumnKind::Payloads`): the items' bytes back to back, `RAW` and read in place, or `LZ4` (48) when that is smaller, decoded once when the chunk is opened.

A group's lengths are stored in one form only: its minimum is its shortest length, its width the bits the largest packed value needs, and the bits after its last value are 0. A reader refuses any other, so damage to a width or a minimum can't make a group read as other lengths of the same total. `Payloads::salvage` reads a chunk whose payload CRC failed: each group of lengths is checked against its neighbours, and only the items of a group that fails, or raw items with a byte in a bad segment, are suspect (`Payloads::is_suspect`). A damaged lz4 column makes every item suspect.

