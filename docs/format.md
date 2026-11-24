---
level: nomicon
---

# Format reference

This page describes format version 1 of a tape, block by block. It starts with files, blocks and their layouts, then
covers definitions, state, the index and the end of a piece.

Later sections cover resuming after a crash, reading by position, sessions and damage, and the codecs of chunk columns.

The chapters of the book explain the format with examples, starting at [Tapes](index.md). Pages of earlier editions
point to where their sections went:

- [Tape concepts](concepts.md) and [Records, tracks and clocks](records_and_clocks.md);
- [Events and series](events_and_series.md);
- [Sessions, pieces and chains](sessions_and_pieces.md) and [Pieces and files](pieces_and_files.md);
- [Sinks, files and locks](sinks_and_locks.md) and [Syncing, cuts and relays](syncing_and_cuts.md);
- [Damage and recovery](recovery.md).

The byte layouts on this page are copied from the module docs of the crate `ww_tape`, and a test checks that they match.

## 1 Conventions {#conventions}

The layouts on this page share a few rules. A reader in another language needs them before the first block.

- **Numbers** are little endian: `u8` to `u64` unsigned, `i64` signed in two's complement. Only the sample codecs that
  say so store big endian ([Sample codecs](#sample-codecs)).
- **`uvar`** is LEB128: 7 bits per byte, low bits first, the top bit set on every byte but the last, at most 10 bytes.
  **`ivar`** is a signed number, zigzag-coded (`(n << 1) ^ (n >> 63)`) and then written as a `uvar`.
- **CRC-32C** is the Castagnoli CRC (`CRC_32_ISCSI`): polynomial 82F63B78h reflected, initial value and final XOR
  FFFFFFFFh. The nine ASCII bytes `123456789` give E3069283h. `ww_tape_codecs::crc32c` runs it on SSE4.2 on x86-64
  and on the CRC extension on aarch64, and through a table elsewhere, with the same result. Every block ends with one, and chunk and index blocks
  also check their heads with one.
- **BLAKE3** gives the content hash, the unit and track hashes, the item hashes and the piece ids. An item hash
  (`ItemHash`) is BLAKE3 of the item's bytes, cut to its first 16 bytes.
- **Offsets** in an index entry, a close or a tail count from the start of the piece. Offsets inside a
  block count from the start of that block.
- **Times** are `i64` ticks of a clock that a definition names ([Definitions on track 0](#definitions-on-track-0)).
- **Sizes** use binary units: 1 KiB is 1,024 bytes and 1 MiB is 1,048,576 bytes. 1 Mi entries is 1,048,576 entries.
- **Reserved** bytes are written as 0. Whether a reader ignores or refuses a reserved field that is not 0 is stated
  per field.

A reader checks each value it takes from the bytes against a cap before the value sizes anything ([Caps on what
a reader takes](#caps-on-what-a-reader-takes)).

## 2 Files {#files}

A tape file is a sequence of [tape blocks](#tape-blocks) appended back to back. A closed file ends with an index, a
close record and a tail ([below](#index-close-and-tail)).

A recording cut into several files is a chain of [tape pieces](#tape-pieces).

### 2.1 Writing a file durably {#writing-a-file-durably}

`ww_tape::TapeWriter` makes a file durable the way databases make their logs durable:

- It creates a new file under its final name `NAME`, and never over a file that is there. A reader tells from the end of
  a piece, its close and tail, whether it is closed, never from its name.
- It syncs the file and its directory after the create, so the name survives a crash.
- A `SyncPolicy` decides when the data is synced: never, every N blocks, or at most an interval apart (1 s by default).
  A sync is `fdatasync` on Linux, `F_FULLFSYNC` on macOS and `FlushFileBuffers` on Windows.
- A failed write or sync ends the file, and the writer never retries it. After a failed `fsync`, Linux may drop the
  dirty pages, and a second `fsync` then succeeds without them.

### 2.2 Locks {#locks}

A file sink holds an exclusive lock on the open file `NAME` itself, from its open to its drop. No other file holds the
lock, so a crash leaves nothing behind.

- On unix, the lock is an `flock` lock, which belongs to the open file. Every other open of the file conflicts with it,
  in the same process too. A reader that opens and closes the file releases nothing.
- A child process forked while a writer holds the lock would share the open file, and the lock with it, until its
  `exec`. A fork handler (`pthread_atfork`) points the child's copy at `/dev/null` instead. The parent's `fork`
  returns only after the child let go, so a resume right after a writer is dropped never fails because of a child.
- On Windows, the writer opens the file sharing reads but not writes. A second writer's open fails, also on a network
  share. Readers open the file as usual.
- One file has one lock, whatever name reaches it: a link and a hard link lock the same file.
- A device, such as a raw SD card, takes the lock as a file does. A FIFO special file, a socket or a directory is
  refused (`FsError::NotAFile`).
- On NFS, SMB, FUSE and other network or user-space file systems, the server or the program behind the files decides
  whether the lock holds. On Linux, macOS and FreeBSD the sink reads the file system's type, and `FileSink::warning`
  returns an `FsWarning` that names it. The sink writes all the same.
- A file on a read-only mount fails with `FsError::ReadOnly` and the path.

### 2.3 Reading a file back {#reading-a-file-back}

`ww_tape::FileBlocks` reads the longest valid prefix of a file. It stops at the first of these blocks and reports where:

- a block with a bad magic, length or CRC;
- a cut block;
- a block whose seq goes back.

Blank bytes at the end (all 0x00 or all 0xFF) are the end of the file, not damage. With `Recovery::Resync`, the reader
reads on past damage instead.

Track 0 can carry the producer's API bundles as [definitions](#definitions-on-track-0).

A reader then explains a recording on any computer without the producer. WireWeaver's `ww explain FILE` does this for
link captures.

Preallocating file space is not built.

## 3 Tape blocks {#tape-blocks}

The block is the unit of the format: a producer hands blocks over, a live consumer reads them, and a file is made of
them. `ww_tape` is a generic block format.

Nothing in it knows its producers, and WireWeaver's link tap is only one of them.

A block is a 32-byte header, a body and a CRC-32C. A block is as long as its content and never padded.

The one exception is the close record of a closed piece ([below](#index-close-and-tail)). The header ([`ww_tape`
crate docs](https://github.com/vhrdtech/ww_tape/blob/main/ww_tape/src/lib.rs)):

```text
off size field
  0   4  magic "WWTP"
  4  u8  version, 1
  5  u8  kind: 1 records, 3 and up other kinds (BlockKind); 2 is reserved (RESERVED_KIND)
  6 u16  flags: bit 0 body coded (FLAG_BODY_CODED, records blocks only), bit 1 state records a writer put or
         copied (FLAG_REPEAT), bit 2 puts or a chunk that belong with the next block (FLAG_WITH_NEXT), bit 3 on a
         chunk its payload unchecked (FLAG_PAYLOAD_UNCHECKED: segment CRCs 0, the block CRC over the head), bit 4 on an
         events chunk a break in its track's monotone relations (FLAG_RELATION_BREAK), bit 5 on a summary block
         a grid tile's tree, its tile after the scale (FLAG_TILE: it changes the layout, so a reader
         without it refuses the block); others 0, and a reader ignores them
  8 u32  len: the whole block, header and CRC included
 12 u32  records in the block
 16 u64  seq: the block number in its session, from 0, +1 per block; a gap is lost blocks. Index, close, tail and
         piece head blocks, and blocks marked FLAG_REPEAT, carry the seq of the block before them and are
         not counted (index::is_structural)
 24 u32  session tag (SessionTag): the same in every block of one writer's unbroken append, never 0 nor FFFFFFFFh
 28 u32  head CRC in chunk and index blocks (their own docs), else 0
 32 ...  body
len-4 u32 crc over bytes 0..len-4
```

A **session** is one writer's unbroken append, such as a producer start or a device boot. Its blocks carry the same
random 32-bit session tag (`ww_tape::SessionTag`), and their seq counts from 0.

For a chain's first piece, the writer derives the tag from the piece id: the same input then gives the same bytes.
[Sessions, media and damage](#sessions-media-and-damage) says how a reader uses both.

The figure below shows a records block that `BlockWriter` wrote. Its head CRC is 0: only chunk and index blocks have
one.

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/block.svg"
<figcaption>A records block of two records, byte by byte, drawn by <code>ww_tape_figures</code>. The body holds the
clock table and each record's track, time delta, length and item.</figcaption>
</figure>

### 3.1 Block kinds {#block-kinds}

The header's kind byte says what the body holds (`BlockKind`):

| Kind | Block |
|---:|---|
| 1 | records |
| 2 | reserved (`RESERVED_KIND`), never written |
| 3 | events chunk |
| 4 | series chunk |
| 5 | parity, reserved: none are written |
| 6 | summary |
| 7 | index |
| 8 | close |
| 9 | tail |
| 10 | piece head |
| 11 | postings of a relation, reserved: none are written |
| 12 | dictionary of a relation, reserved: none are written |
| 13 | append head |
| 14 | close summary |

A reader reads a block of kind 2, or of a kind not in this table, as a block whose body it doesn't know. It skips such a
block by its length, and the block is not damage.

### 3.2 Header flags {#header-flags}

Header bytes 6..8 hold flags. A reader ignores the bits it doesn't know, except bit 5 on a summary block. That bit
moves the block's body behind the tile, so a reader without it refuses the block.

| Bit | Flag | Marks |
|---:|---|---|
| 0 | `FLAG_BODY_CODED` | a coded records body ([below](#records-blocks)) |
| 1 | `FLAG_REPEAT` | a block of state records a writer put or copied ([State tracks](#state-tracks)) |
| 2 | `FLAG_WITH_NEXT` | puts or a chunk that belong with the block after it ([State tracks](#state-tracks), [Space overviews](#space-overviews)) |
| 3 | `FLAG_PAYLOAD_UNCHECKED` | a chunk whose payload went unchecked; reserved, and a reader refuses it |
| 4 | `FLAG_RELATION_BREAK` | an events chunk where a track's monotone relations start again ([below](#relation-breaks)) |
| 5 | `FLAG_TILE` | a summary block of a grid tile's tree, its tile after the scale ([Summaries of tiles](#summaries-of-tiles)) |

### 3.3 Records blocks {#records-blocks}

A **records block** (kind 1) holds records: items on tracks at times. Its body starts with a clock table, which lists
the clocks its records count, then holds the records back to back.

The body uses `uvar` and `ivar` ([Conventions](#conventions)):

```text
uvar n, n × uvar clock number (ascending, 1 and up): the clocks of the block's records; slot i is the i-th
per record:
  uvar head: track << bits | slot, bits the fewest that hold n - 1 (0 for one clock)
  ivar time delta in ticks, against the previous record of the same slot in the block (the first against 0);
       absent for track 0
  uvar item length, the item's bytes
```

Record times are signed i64 ticks of the track's clock. Each time is a delta against the previous record on the same
clock in the block.

A block therefore reads alone, and a block of one clock spends no bit on the slot. Records on track 0 have no time
delta.

No wall time is in a block: wall time is a clock of its own, mapped to the others.

The **item of an events record** is an `EventRecord` when the track has spans or flags. It holds the duration (spans
only), the flags (only when the track has flags), then the payload.

The record's time is the start. A track of plain instants carries the payload alone, and a state record of an events
track carries the same item.

With header flag `FLAG_BODY_CODED`, the body is coded
([`body`](https://github.com/vhrdtech/ww_tape/blob/main/ww_tape/src/body.rs)).

Coding is off unless a writer calls `ww_tape::code_body`, and a writer keeps the plain body when coding doesn't shrink
it:

```text
u8    codec (BodyCodec): 0 stored, 1 lz4 block
uvar  decoded length: the body as it would be stored, at most MAX_BLOCK_LEN - 36
...   coded bytes: stored, the body itself; lz4, one lz4 block (no frame, no checksum)
```

The block CRC covers the coded bytes: a reader finds damage before it decodes. A reader decodes with `Block::decode` and
takes a body of a codec it doesn't know as a body it doesn't know.

Chunk blocks are never coded, since their columns are read in place.

### 3.4 Chunk blocks {#chunk-blocks}

A **chunk** block holds one chunk of one track, read in place. An events chunk (kind 3) holds up to a few thousand items
of an event track.

A series chunk (kind 4) holds a run of samples of a fixed-rate series. The header's count is the chunk's item or sample
count.

#### 3.4.1 The chunk head {#the-chunk-head}

The chunk's **head** says where the chunk belongs without the definitions or an index. It holds the track's identity
hash and number, the first item number, the first and last tick, and the latest end of any item.

With the latest end, an overlap query needs no decode of the ends. A series chunk's head also carries its grid ([Series
tracks](#series-tracks)).

The head then lists one entry per column: its kind, its codec id, a parameter and its offset. It also holds one CRC-32C
per **segment** of the payload. Writers make segments 64 KiB long. The head states the segment size, and a reader takes
any size from 4 KiB to 16 MiB (`SEGMENT_SHIFT_MIN`, `SEGMENT_SHIFT_MAX`).

The columns follow back to back, then 16 zero bytes. These zeros keep an unaligned 16-byte load at any column byte
inside the block.

The head has a CRC of its own. A reader therefore trusts the head when only the payload is damaged.

`Recovery::Resync` then skips the block by its length. It hands the block out with its bad segments marked
(`Block::is_intact` false, `Chunk::bad_segments`, `Chunk::column_damaged`).

A flipped bit in a 4 MiB series chunk therefore loses one 64 KiB segment.

For a block with a bad head, the reader moves on one byte and looks for the next magic. The head ([chunk module
docs](https://github.com/vhrdtech/ww_tape/blob/main/ww_tape/src/chunk/mod.rs)):

```text
off size field
 24 u32  session tag, as in every block
 28 u32  head CRC: CRC-32C over bytes 0..28, then 32..payload offset
 32 u32  payload offset from the block start (in an aligned file a multiple of the alignment in the file)
 36 u32  track number (TrackDef::number)
 40 u64  track id, the identity hash of the track (TrackDef::id): the chunk places itself without the definitions
 48 u64  first item number of the track (sample index in a series)
 56 i64  first tick: the first item's start (the first sample's tick)
 64 i64  last tick: the last item's start (the last sample's tick); at least the first
 72 i64  latest end: the largest end of any item, at least the last tick; the last tick when every item is an
         instant (EndMode::Absent) and in a series chunk. An overlap query and a rebuilt index need no decode of the
         ends column.
 80  u8  segment shift: the payload is checked in segments of 2^shift bytes (16: 64 KiB)
 81  u8  columns n, 1 and up
 82  u8  mode: an events chunk's time mode, the time codec's; a series chunk's fixed axes a, 0 to
         GRID_FIXED_AXES_MAX (0: a series of no fixed axes, GridDef)
 83  u8  end byte: an events chunk's end mode (EndMode); a series chunk's overview level k, 0 for the cells
         as written, 1 to GRID_OVERVIEW_LEVELS_MAX for space overview k of a grid of fixed axes
 84 ...  series chunks only, the grid of the samples (SeriesGrid):
         84 u64  grid index: the sample the grid counts from, at most the first item
         92 i64  grid tick: the tick of that sample; sample i is at grid tick + (i - grid index) · period,
                 rounded down, the period the track's GridDef gives
        100  u8  scale length s, 1 to SCALE_LEN_MAX
        101 ...  scale: what a stored number is worth in the sample type's unit, as the shrink_wrap bytes of an
                 exact_units Magnitude (10⁻³ is fa 00)
            ...  a × u32 tile coordinate: the chunk's tile along each fixed axis, its first cell over the axis'
                 tile extent (TileCoords; none for a series)
    ...  n × column: kind u8, codec u8, param u16, offset u32 (from the payload start; the first 0, ascending)
    ...  segment CRCs: one u32 per segment of the payload, CRC-32C of its bytes
    ...  zeros up to the payload offset
 payload: the columns back to back, then 16 zero bytes, so an unaligned 16-byte load at any column byte stays in
          the block
len-4 u32 CRC over bytes 0..len-4, as in every block
```

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/chunk_head.svg"
<figcaption>The head of the first <code>phase_a</code> chunk of the drive's first piece, byte by byte: 1,500 samples on the
grid from sample 0 at tick 0, one samples column coded <code>DELTA</code> (32), and one segment CRC. The payload starts
at byte 115, right after the head, since the file is not aligned. Drawn by <code>ww_tape_figures</code> from the
drive's recording at 1/1000 of its data.</figcaption>
</figure>

The column kinds (`ColumnKind`):

| Kind | Column | Holds |
|---:|---|---|
| 1 | `Starts` | the items' starts ([event times](#event-times)) |
| 2 | `Ends` | the items' ends, as the head's end mode says |
| 3 | `Flags` | a u32 flags word per item; in a series chunk, runs of flagged samples ([Sample flags](#sample-flags)) |
| 4 | `PayloadLengths` | each item's payload length ([Payload codec](#payload-codec)) |
| 5 | `Payloads` | the items' payloads back to back |
| 6 | `Samples` | a series' samples; the param names the first channel ([Sample codecs](#sample-codecs)) |
| 7 | reserved (`ColumnKind::PAYLOAD_LEAF`) | |
| 8 | `Relation` | one relation's values, a u64 per item ([below](#relation-columns)) |
| 9 | `OverviewMin` | an overview chunk's smallest value per cell and channel ([Space overviews](#space-overviews)) |
| 10 | `OverviewMax` | an overview chunk's largest value per cell and channel |
| 11 | `OverviewMean` | an overview chunk's mean per cell and channel |

The codec ids (`Codec`):

| Id | Codec |
|---:|---|
| 0 | `RAW`: plain little-endian values or bytes |
| 1 to 3 | the event time codec: `STARTS`, `DURATIONS`, `FLAGS` |
| 4 | payload lengths, `LENGTHS` |
| 5 | a series chunk's flag runs, `FLAG_RUNS` |
| 16 to 47 | the sample codecs |
| 48 | `LZ4` payloads |
| 49 | `SERIES`, reserved |

A reader refuses a column whose kind or codec it doesn't know, and still reads the chunk's other columns.

Bit 7 of the codec byte marks an **outer-coded** column. Such a column holds an outer codec byte, the decoded length,
then the bytes the inner codec (bits 0..6) applies to after decoding.

No outer codec exists yet, and every reader refuses such a column.

### 3.5 Relation columns {#relation-columns}

A **relation** links a track's items to something else, such as the items of another track. `TrackDef::relations`
declares a track's relations ([Definitions on track 0](#definitions-on-track-0)).

A **relation column** (kind 8, `RelationColumn`, `EncodedRelation`) holds one u64 per item for a relation whose values
are stored.

The column's param is the relation's index in `TrackDef::relations`. An events chunk has exactly one column per stored
relation.

Writers check this, and readers check it against the track's definition (`StoredRelations`, `TimesError::Relation`). A
damaged column reads by `RelationColumn::salvage`.

#### 3.5.1 Relation column codecs {#relation-column-codecs}

Every relation column starts with its item count (u32). A reader refuses a column whose count isn't the chunk's: the
values never rest on the chunk head alone.

One of two codecs follows the count:

- **Codec 1** (`STARTS`) holds values that never decrease. It starts with a 17-byte head: the starts column's time mode,
  then the first and the last value.
  - Each value in the head is an order-keeping i64: the u64 with its top bit flipped.
  - A starts column of those i64s follows: one period for values a fixed step apart, else a line per group of 128.
- **Codec 2** (`DURATIONS`) holds any values, coded as the ends column codes durations. Per group, it stores the
  smallest value, a width and an offset, then the values less that minimum.

A writer takes codec 1 when a chunk's values never decrease. A reader then finds the first item at or above a value by
search (`RelationColumn::lower_bound`).

A monotone relation is always in codec 1.

### 3.6 Relation breaks {#relation-breaks}

A **monotone** relation's values never decrease along its track, across chunks and pieces: 10, 10, 11, 15 is monotone,
10, 12, 11 is not.

A **[[relation break]]** marks the chunk where a track's monotone relations start again. Only the writer's caller can make
one.

A writer keeps each monotone relation's newest value. When the chunk holding that value doesn't read, as after a resume
over damage, the writer refuses the track's next chunk (`DescribeError::RelationUnread`).

The caller goes on with `TapeWriter::accept_relation_break` and gives the value the relation continues from. The writer
never accepts a break by itself.

The writer checks the track's next events chunk against that value and marks the chunk in two places:

- bit 4 of its header flags (`FLAG_RELATION_BREAK`, `Block::is_relation_break`, `Chunk::relation_break`);
- bit 3 of its index entry's flags (`IndexEntry::relation_break`).

A reader refuses the entry flag on an entry of another kind.

#### 3.6.1 Reading across a break {#reading-across-a-break}

Across the start of a marked chunk, a reader assumes no order of the track's monotone relations. Values before the chunk
may be larger than its first.

From the marked chunk on, the order holds again. A resume or a later piece takes the track's newest values from the
marked chunk on, never from before it.

Anywhere else, a decrease across chunks is damage or a lie. `TapeReader::verify` refuses it from the relation columns,
and `TapeReader::track_entries` from the range relation's ranges in the index entries (`ReadError::RelationOrder`).

`TapeReader::relation_breaks` lists a track's marked chunks over all pieces. A lookup by a relation's value then knows
where the order starts again.

### 3.7 Summary blocks {#summary-blocks}

A **summary** block (kind 6) holds a batch of consecutive nodes of one level of a track's summary tree. An overview of a
long track then reads a few hundred KB instead of its items.

`ww_tape` reads and checks a summary block's head and lists the block in its index without reading a node. The crate
`ww_tape_summary` builds the nodes (`EventTree`, `SeriesSummaries`) and reads them (`Summary`).

#### 3.7.1 Summary nodes {#summary-nodes}

Nodes run over item numbers, not time. A level-0 node of an event track holds 2048 items (`EVENT_BASE_SHIFT`, 11),
and a node one level up holds 16 nodes of the level below (`FANOUT_SHIFT`, 4). A series' level-0 node holds
`2^base_shift` samples, `base_shift` from 4 to 24 (`SeriesSummarySpec::BASE_SHIFT_MIN`, `BASE_SHIFT_MAX`).

Node `n` of a level holds items `[n << shift, (n + 1) << shift)`.

The first and last node of a piece may therefore hold fewer items, and the nodes of two pieces line up.

A node holds:

- in an event tree, its first and last item's start, the OR of the items' flags and, for spans, the latest end. Counts
  follow from the item range.
- in a series, `2^base_shift` samples and, per channel, the statistics the track declares (`SeriesSummarySpec`), from
  this set:
  - minimum and maximum;
  - sum and sum of squares;
  - a count of clipped or NaN samples;
  - the OR of the flags;
  - for complex samples, the mean and maximum of `I² + Q²`.

A series node holds no ticks, since the chunks' grids and the period give them.

Every statistic merges in item order (`EventNode::merge`, `ChannelStats::merge`). A reader that builds coarser bins
therefore gets the bits the writer stored one level up.

#### 3.7.2 Scales of series nodes {#scales-of-series-nodes}

A chunk may change the scale of a series' numbers mid-node, as when an autoranging device changes prefix. Such a node is
kept at the common scale of its samples (`common_scale`): for `10⁻³` and `10⁻⁶` that is `10⁻⁶`.

The coarser numbers are multiplied by a whole factor. Minimum and maximum therefore stay exact integers in the sample
type's unit.

A series summary block states one scale in its head, the common scale of all its nodes. The block therefore reads alone,
without the chunks.

`SeriesNode::merge` merges nodes of two scales, such as those of two blocks, at their common scale. It refuses two
scales with no common scale (a ratio that isn't a fraction of integers) rather than mixing them.

#### 3.7.3 Writing summary blocks {#writing-summary-blocks}

The builders write a block when a level's batch fills, and the partial nodes at `close`, up to a level with a single
node. A lost summary block is rebuilt from the data.

The blocks are small, at most 4,096 bytes for an event tree (`EVENT_BLOCK_BYTES`) and 2,048 for a series
(`SERIES_BLOCK_BYTES`) by default. They go through `TapeWriter::defer_block` as deferred blocks. In an aligned file, they
therefore fill the gaps before chunk payloads and cost no space.

`SeriesSummaries::push` keeps every statistic in one pass over the samples. Integers up to 32 bits go through vector
kernels with exact integer sums.

On x86-64, the kernels use AVX-512 or AVX2 (Advanced Vector Extensions), chosen at run time. 64-bit integers and floats
take one sample at a time in order, since a float sum depends on the order.

[Sizes and speeds](sizes.md#how-fast-summaries-keep-up) gives their speed per element type and kernel.

The head names the track, the level, the node size, the statistics stored, the items covered and their tick range. For a
series, it also names the element, channels and scale.

An index therefore lists the block without reading its body. The header's count is the node count. Bytes 28..32
describe a series' element when byte 47 is 13, and are 0 otherwise
([summary module docs](https://github.com/vhrdtech/ww_tape/blob/main/ww_tape/src/summary/mod.rs)):

```text
off size field
 28  u8  bits of one part of a described element (ElementDesc::bits), 0 for a coded element and for events
 29  u8  its exponent bits, 0 for an integer
 30  u8  bit 0 signed, bit 1 msb_first (always 0: a summary's element has no byte order)
 31  u8  its parts, 1 or 2
 32 u64  track id (TrackDef::id)
 40 u32  track number (TrackDef::number)
 44  u8  tree: 1 events, 2 series (Tree)
 45  u8  level, 0 and up
 46  u8  node shift: a node holds 2^shift items (events: 11 + 4 · level; series: base_shift + 4 · level)
 47  u8  element of a series' summaries: an Element code, or 13 for the element bytes 28..32 describe
         (ElementDesc::summary_element); 0 for events
 48 u16  fields: the statistics stored (EventFields, SeriesStats bits)
 50 u16  channels of a series, 1 and up; 0 for events
 52 u16  scale length: bytes of the scale after the head, 1 to SCALE_LEN_MAX for a series, 0 for events
 54 u16  first channel or leaf of this block: reserved, 0 (the block's channels start at channel 0); a reader
         refuses another value
 56 u64  first item: the first item the block covers (the first node's first item, or later in a partial node)
 64 u64  items the block covers, 1 and up; the nodes are those holding items [first, first + items)
 72 i64  first tick: the first covered item's start
 80 i64  last tick: the last covered item's start, at least the first tick
 88 i64  latest end: the largest end of a covered item, at least the last tick
 96 ...  scale of a series: what a stored minimum, maximum or sum is worth in the sample type's unit, as the
         shrink_wrap bytes of an exact_units Magnitude (10⁻³ is fa 00); squares and powers are at its square
96+s ... a tile's block only (header flag FLAG_TILE): u8 fixed axes a, 1 to GRID_FIXED_AXES_MAX, then a × u32 tile
         coordinates, as a grid chunk's head carries them (TileCoords)
 ...  ... body: columns of n values each, n the node count, little-endian, back to back
len-4 u32 CRC over bytes 0..len-4, as in every block
```

The body holds one column per field:

- An event block holds the first ticks (i64) and the last ticks (i64). The flags ORs (u32) and the latest ends (i64)
  follow when its fields include them.
- A series block holds one column per statistic, channel and component of a sample, in a fixed order of statistics. A
  pair has two components for minimum, maximum, sum and sum of squares.
- A series block's element is the samples' summary element. For an element with a zero `p/q`, it holds `q · sample - p`.
  That number is a signed integer of twice the container's bits, at the block's scale over `q`.
- A head names a predefined element by its code. Any other element has code 13 and its description, and a reader
  refuses a description of a predefined one.
- In a series block, minimum and maximum take 8 bytes and sums are f64. The clipped count is u64, flags u32, and the
  mean and maximum power f32.

### 3.8 Event times {#event-times}

`ww_tape::times` codes the **times** of an events chunk. Every read runs on the chunk's bytes as they are, with no
decode into a list first.

Starts take one of two time modes:

- **Implicit**: item `j` starts at the chunk's first tick plus `j` periods. Evenly spaced items take this mode.
- **Linear frame of reference**: a 25-byte head per group of 128 items holds the group's first tick and a slope in Q16
  ticks per item. The head also holds the smallest residual, a width and an offset; each item's residual against the
  line follows, bit-packed at the group's width.

Periodic items therefore cost their jitter, not their period, against 8 bytes for a raw i64. [Sizes and
speeds](sizes.md#what-event-times-cost) gives the bytes per item of each workload.

An exact period takes almost nothing, and bursty nanosecond traffic takes the most.

Fetching an item reads one group head and one value. A lower bound searches the groups' first ticks, then only the items the
slope leaves possible.

#### 3.8.1 Ends, flags and decoding {#ends-flags-and-decoding}

Ends take one of three end modes: absent (instants), the next item's start (spans back to back, nothing stored), or
packed durations. A group's minimum duration plus 2^width − 1 bounds the group's longest item, which serves overlap
queries.

Flags are an optional u32 per item, packed per group with the OR of the group's flags. A zoomed-out view reads the ORs
without a decode.

Groups decode with hand-written kernels at a fraction of a nanosecond per item. `ww_tape::bitpack` picks AVX-512, AVX2
or NEON (the Arm vector extension) at run time, else a scalar path.

A chunk whose CRC failed still reads through `EventTimes::salvage`, which checks the group heads one by one. It marks
only the items of a group that fails as suspect.

### 3.9 Aligned files {#aligned-files}

A file's layout definition (`Layout`) states how its blocks are placed: alignment 0 or 4096, and 0 when a file has no
layout definition. In an aligned file, such as a series file, every chunk payload starts on a 4 KiB boundary.

The chunk's head sits in front of the payload. Small blocks waiting for a place (`TapeWriter::defer_block`) fill the gap
before the head, and zeros inside the head fill what is left.

Recovery looks for a block at every byte, aligned or not, and takes one only when its CRCs match.

## 4 Definitions on track 0 {#definitions-on-track-0}

**Definitions** say what a reader needs before the data: the producer, the clocks, the tracks and the API bundles that
type the items. Each definition is a record on **track 0**, which is reserved for them.

A record holds one `Describe`, the item's whole bytes, with no time delta. A definition is one of:

| Definition | Says |
|---|---|
| `Producer` | the producer's name |
| `Layout` | how blocks are placed: alignment, `gap_max`, `resume_align`, and the `StatePolicy` of track 0 (`definitions`) |
| `Bundle` | an API bundle (a `ww_self` `ApiBundle` in its ShrinkWrap encoding) and its hash, by which the producer, a track's `Items::WireWeaver` and `TypeRef::bundle` name it |
| `Clock` | a `ClockDef`: number, stable key, exact rate and resolution as `Magnitude`s, and origin |
| `Map` | a `ClockMap` between two clocks |
| `Track` | a `TrackDef`: what a track is ([below](#track-definitions)) |
| `Item` | a `TypedItem`: any wire_weaver value by bundle hash and type path, such as a description of a run |
| `Schema` | a schema a foreign format names, by its BLAKE3 hash ([Items of a track](#items-of-a-track)) |

A clock's origin is free-running, a boot, the Unix epoch, a media start, a session start, or the next epoch of a
counter. A counter's epochs are fixed to each other by an exact offset map.

A `ClockMap` is a list of segments `b = b0 + (a - a0) · slope`, the slope an exact_units `Ratio64`, each with an error
bound. A map is strictly increasing across its segments.

For example, WireWeaver's link tap maps its own clock to the wall clock it reads at its start.

Definitions sit in records blocks of their own, with no record of another track. Each such block is at most
`DEFINITION_BLOCK_LEN` (64 KiB), unless one record is longer. A definition or a state item is at most
`STATE_ITEM_MAX` (67,108,722) bytes: one that long fills a block of `MAX_BLOCK_LEN` alone.

`Describe` is an evolvable enum: a variant a reader doesn't know reads as `Describe::Unknown` and is skipped.

### 4.1 Track definitions {#track-definitions}

A `TrackDef` holds:

| Field | What |
|---|---|
| `number` | the number records use, 1 and up |
| `key` | the stable key ([Track identity](#track-identity)) |
| `clock` | the clock its times count |
| `dimensions` | name and value pairs that are part of its identity |
| `items` | what one item is: a WireWeaver stream's value, a ShrinkWrap value, plain bytes, a number or bytes of a foreign format ([below](#items-of-a-track)) |
| `label` | a name for people |
| `repeat` | events or state (`TrackRepeat`, [State tracks](#state-tracks)) |
| `kind` | events, with or without ends and flags, or a [series](#series-tracks) |
| `owner` | 32 opaque bytes that tell apart tracks of one identity ([below](#track-owners)) |
| `relations` | what its items relate to (`RelationDef`) |

A track declares at most `RELATIONS_MAX` (16) relations, with unique names of at most `RELATION_NAME_MAX` (64)
bytes. Each `RelationDef` has a name and a target: a run's 16 opaque
bytes and a track id, explicit and never bound by position.

A `RelationDef` also says where its per-item values are (none, a column, or only an index), whether they never decrease,
and which index they feed.

An index is none, a range in the index entry, postings or a dictionary, and a track has at most one range.

### 4.2 Items of a track {#items-of-a-track}

`TrackDef::items` says what one item of a track is. It changes no byte of a record or a chunk. An events item's bytes
are stored as given in every form, after the duration and flags an `EventRecord` puts before them. `ww_tape` never
reads a payload. Items a reader doesn't know therefore leave the bytes as they are, never damage.

| `Items` | One item is | Fields |
|---|---|---|
| `WireWeaver` | a value of a stream of a WireWeaver API, in its shrink_wrap encoding | `api`; `bundle`, the hash of a `Bundle` definition, or empty; `path`, the stream's numeric resource path; `keys`, of [keyed arrays](#track-identity) on the path; `item`, the type when the path doesn't give it |
| `ShrinkWrap` | a value of a type in a bundle, with no API stream | a `TypeRef` |
| `RawBytes` | plain bytes with no type: a frame as received, a file's contents | none |
| `Numbers` | one number, little-endian in its container | `element`, an `ElementDesc`; `unit`, an exact_units type as a `TypeRef`, or None |
| `Foreign` | bytes of a format the user names | a `Foreign`: `name`, `media_type`, `version`, `schema` |

A series' items are `Numbers`: the element of each channel of a sample, and the value type that gives its unit.

An `ElementDesc` describes a number without a new code for it:

| Field | What |
|---|---|
| `bits` | stored bits of one part, 1 to 64 |
| `exponent_bits` | 0 for an integer; a float has at least 2, and a mantissa bit |
| `signed` | whether an integer is signed; a float always is |
| `msb_first` | the byte order of a part |
| `parts` | numbers per element, 1 to `ELEMENT_PARTS_MAX` (16); 2 for an I/Q pair |
| `zero` | the stored number that stands for zero: `255/2` for 8-bit IQ offset to 127.5; None for 0 |

A part is read into its scalar, `ElementDesc::scalar`:

- An integer is read into the smallest of 8, 16, 32 and 64 bits that holds it, of its sign. A 1-bit unsigned one is a
  `bool`.
- A float is read into an `f32` when an `f32` holds each of its values exactly, else into an `f64`.

A writer takes an element that `ElementDesc::is_valid` passes:

- A float has 2 to 11 exponent bits and a mantissa bit. It has at most 32 bits or is an `f64`, and has no zero.
- A zero sits only on an integer of at most 32 bits. Each stored value shifted by it fits the summary element.

`ElementDesc::of` gives the description of each predefined `Element`.

A `Foreign` format has four fields, each may be empty:

| Field | What |
|---|---|
| `name` | the user's name for the format, such as `com.example.frame` |
| `media_type` | a media type (RFC 6838, parameters allowed), such as `application/vnd.tcpdump.pcap` |
| `version` | the format's version as the user writes it |
| `schema` | the hash of a `Schema` definition that describes the format |

A writer refuses a foreign format with neither a name nor a media type. A name or a version is at most
`ENCODING_NAME_MAX` (255 bytes) of printable ASCII. A schema hash is 32 bytes or empty. A media type is printable ASCII
with a `/` between a type and a subtype, at most `MEDIA_TYPE_MAX` (255 bytes). Numbers of an element that does not fit
the rules above are refused too.

A **schema** (`Describe::Schema`) is any blob that describes a format: a protobuf file, a JSON schema, a Kaitai
Struct file. It holds its BLAKE3 hash, a media type and the bytes. It fills a slot of its own by hash, as a bundle
does. The writer puts a track's schema before the track's definition, and a cut carries it with the track. A writer
refuses a schema whose hash isn't BLAKE3 of its bytes.

`Items` is evolvable. A variant a newer ww_tape adds reads as `Items::Unknown` with the rest of the definition intact.
A reader passes such a track's items on as bytes. A relay that takes a producer's block holds such an events track and
stores its items as given. A program's own definition never carries an unknown variant: the writer refuses one (test
`raw.rs`).

The bytes a track definition takes, as its `Describe` item, for each kind of items. The track is an events track with
a label of three letters and no dimensions or relations (test `raw.rs`):

| `items` | Bytes |
|---|---|
| `WireWeaver`: `drive_api 0.1.0`, a path of one segment, no bundle hash | 73 |
| `RawBytes` | 56 |
| `Foreign`: media type `application/vnd.tcpdump.pcap` | 90 |
| `Foreign`: name `com.example.frame`, version `3`, a schema hash | 112 |
| `Numbers`: 8-bit IQ with zero `255/2` | 65 |

A schema definition takes 54 bytes over the schema's own bytes.

### 4.3 Slots {#slots}

Each definition fills a **slot**, which `ww_tape::Slot::of` gives. The producer and the layout fill one slot each, a
track and a clock one per number, and a map one per pair of clocks.

A bundle, a schema, a typed item or a variant the reader doesn't know fills a slot of its own, by its hash. The definition in
force at a place is the newest record of its slot behind that place.

### 4.4 How a writer puts definitions {#how-a-writer-puts-definitions}

A writer writes definitions only as [puts](#state-tracks). The application holds the definitions it works with in a
`Definitions` (`define`, `define_all`, `close`) and passes them with each write.

The writer refuses data on a track not held, and it refuses a caller's block of definitions
(`DescribeError::DefinitionBlock`).

Before a block of data, the writer puts the definitions its tracks need: the clocks, maps and bundles a track names,
then the track. The producer and the layout are the writer's own (`WriterOptions`).

The writer puts them after every piece head, before the first block of a file without one, and where a caller's block
starts another session.

A call that puts a definition or a state item also uses the producer and the layout. Each is put with the records of
that call by track 0's policy, as a definition is. It is copied once the copy spacing is reached and put again past
`near`.
Such a copy is skipped when the piece's state entries are full: it never refuses a call. A reader takes the newest
layout record, the copy included, for the gap rules.

Definitions go in the order the writer puts them: the same input then gives the same bytes.

## 5 State tracks {#state-tracks}

Track 0 and every track whose `repeat` is `TrackRepeat::State` hold **state**, not events. A reader at any place takes
the newest record of each slot behind it.

A state track's records fill one slot, the track's.

A `StatePolicy` says how a writer keeps state records near their use. `near` (32 MiB by default) is how far behind a use
the item must lie in the same piece.

`copies` (1 by default) is how many more records an item gets after its first one in a piece. Track 0's policy is in the
layout (`Layout::definitions`).

A series track has no records and takes the plain policy, since its chunks are data and carry their own timing ([Series
tracks](#series-tracks)).

An item is known by its **hash** (`ItemHash`): BLAKE3 (a cryptographic hash function) of its bytes, cut to 128 bits.

A **put** writes an item unless the newest record of its slot is the same item, at most `near` bytes behind. The
distance is measured to where the data lands.

For example, a writer puts a track's definition before every block of the track's data.

With the default policy, the writer writes the definition at the first block. It writes it once more as a copy at a use
at least 1 MiB later.

After that, the writer writes the definition again whenever its newest record lies more than 32 MiB behind.

### 5.1 Puts and copies {#puts-and-copies}

The application puts a state track's item at each use (`TapeWriter::put`, `put_item`). The writer refuses a caller's
records on a state track in a block of data.

The writer puts definitions the same way before every block of data.

A put of an item that lies near behind still writes it as a **copy** while two conditions hold:

- fewer than `1 + copies` records of it lie in the piece;
- the newest lies at least `NearPolicy::copy_spacing` (1 MiB) behind.

The writer writes copies only at uses, never ahead of them.

The writer remembers at most `NearPolicy::memory_max` slots and forgets the one written longest ago first. A forgotten
slot costs one more write, never a wrong read.

### 5.2 Marking blocks of puts {#marking-blocks-of-puts}

Puts and copies go in records blocks marked **`FLAG_REPEAT`** (header bit 1). Such a block carries the seq of the block
before it and is not counted.

The producer's seq therefore stays its own count. A reader that holds an item of the same hash in the same slot takes
the record as nothing new.

A block of puts that belongs with the block after it is also marked **`FLAG_WITH_NEXT`** (header bit 2). The flag
appears on a block marked `FLAG_REPEAT` and on a chunk of a unit (`TapeWriter::write_chunk_unit`), such as a grid tile
chunk and its overviews ([Space overviews](#space-overviews)).

A block belongs with the next one in four cases:

- it serves the block of data after it;
- the next block holds the rest of a put too long for one block;
- it is a block of a group of puts but the last ([Groups of puts](#groups-of-puts));
- it is a chunk of a unit but the last.

A **run** is such blocks in a row and the block they belong to, all from one writer call. It has at most
`RUN_BLOCKS_MAX` (4096) blocks, and a writer refuses a call that would make a longer one (`InputError::RunTooLong`).
Readers take a run whole or not at all:

| Reader | A run |
|---|---|
| a scan of a piece | its entries only once the block it belongs to is taken; a run cut by the end of the bytes, damage, a gap or the end of a piece is listed in `PieceInfo::dropped_runs` and its blocks in no entry |
| a follower (`ChainFollower`) | its blocks only with the last one; until then it keeps their places, at most `RUN_BLOCKS_MAX`, and reads their bytes again then (`FollowError::RunTooLong` past it) |
| a resume | drops a run at the end of what it keeps (`Resumed::dropped_run`) |
| the index of a closed piece | every block: only a resume closes a crashed piece, and it drops the torn run first |

So no reader shows part of a run that a crash tore. Damage inside a run of a closed piece hurts its blocks
only. A lookup lists the block it lost, as for any other block.

`TapeWriter::put_block` puts a producer's block of puts by the writer's own rule. `relay_block` passes a live producer's
stream on.

### 5.3 Groups of puts {#groups-of-puts}

`TapeWriter::put_group` puts items on several state tracks as one **group**: a reader takes all of them or none.
Each put decides by the near rule as `TapeWriter::put` does, so a group writes only the items not in force close
behind. The records it writes and the definitions they need go into blocks of puts one after the other, every block
but the last marked `FLAG_WITH_NEXT`. A group is so a run whose last block is a block of puts. The bytes are those of
puts; a group adds no new field.

The writer checks the whole group before it writes the first block:

| Check | Refusal |
|---|---|
| every track a state track the definitions hold | `DescribeError::NotState`, `UnknownTrack` |
| each item at most `STATE_ITEM_MAX` bytes | `DescribeError::TooLong` |
| no track twice in the group | `InputError::Group(GroupRefusal::SlotTwice)` |
| the items at most `GROUP_BYTES_MAX` (64 MiB) together | `InputError::Group(GroupRefusal::TooLong)` |
| the piece's room for the state entries of all it writes | `IndexError::StateEntries` |

`PieceChain::put_group` cuts before a group that the piece has no room for and puts it whole into the next piece.
No cut falls inside a group, nor a checkpoint or a deferred block.

A crash inside a group leaves a run of marked blocks at the end of the piece. The resume drops it and names it in
`Resumed::dropped_run`, and the application gives the group again. The group given again writes the bytes the run
without the crash has.

### 5.4 Deferred blocks {#deferred-blocks}

`TapeWriter::defer_block` puts the definitions of a deferred block when the block is deferred, and places the block
later with no puts.

A deferred block waits at most `DEFER_WINDOW` (1 MiB) of writes. It therefore lies at most its tracks' `near` plus that
window after their definitions.

A writer keeps at most `DEFERRED_BYTES_MAX` (1 MiB) of deferred blocks and writes them out once they pass it. A cut
and a close place every deferred block first.

## 6 Track identity {#track-identity}

A track's **stable key** (`TrackDef::key`) names the same resource in every file and every session of a producer,
whatever ids or indices that session used. `ww_tape::track_key` hashes the resource's **name path** under its API.

The hash starts from `api_root_id` of the API's crate name, without a version: a breaking change is a new generation of
the same API.

One `Segment` per step follows, each with a tag byte and its length. Therefore `a.b` and `ab` never collide:

| Step | Segment |
|---|---|
| a resource, `link`, `rx_frames` | `Name`: its identity name; a resource renamed with `#[moved_from]` keeps its oldest name |
| an index into a plain array, `link[2]` | `Index(2)` |
| an element of a [keyed array](https://ww.vhrd.tech/api/arrays/), `flow[{key}]` | `Key`: the key's shrink_wrap bytes, not the index |

For example, `link[2].rx_frames` of WireWeaver's link tap has the key `track_key(api_root_id("ww_tap"), &[Name("link"),
Index(2), Name("rx_frames")])`. The function `ww_tap::track_key(2, ids::RX_FRAMES)` computes it.

A dissector may number flows in a different order in two sessions. Each flow's tracks still get the same key, since the
flow's key is hashed, not its index.

The `path` of a track's `Items::WireWeaver` stays numeric. Its `keys` holds the key bytes for each index into a keyed
array, by its position in `path`.

In WireWeaver, `ww_tape_bundle::name_path` builds the name path from the API bundle, the numeric path and the keys.
`key_in_bundle` builds the key under the bundle's own crate.

A reader therefore rebuilds the name of every track and checks its key.

A producer that sees an element of a keyed array before its key reads `key[i]` first. It writes the track's definition
once it knows the key.

Two changes give a track a new key:

- The key's shrink_wrap bytes change when the key's type gains a field, even a defaulted one. Every keyed track then
  gets a new key.
  - A canonical hash of the key's value, which leaves out fields at their default, would survive such a change. Such a
    hash is not built.
- A resource renamed within its trait keeps its key. One moved to another trait gets a new name path and therefore a new
  key, even with `#[moved_from]`.

<a id="index-finish-and-tail"></a>

## 7 Index, close and tail {#index-close-and-tail}

A writer lists every block it places in **index** blocks (kind 7). A reader then finds blocks without a scan.

Every few MiB of blocks (`ww_tape::IndexPolicy`, 4 MiB by default), the writer places a **checkpoint**. A checkpoint is
an index block with one 128-byte entry per block placed since the previous checkpoint, and a pointer back at that
checkpoint.

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/checkpoints.svg"
<figcaption>The drive's first piece, closed. Each checkpoint points back to the one before, so the newest one leads to all.
At the end, the close names the final index and the newest checkpoint, and the tail names the close. Drawn by
<code>ww_tape_figures</code> from the drive's recording at 1/1000 of its data, with a checkpoint every 3 KiB instead
of every 4 MiB.</figcaption>
</figure>

A policy can also place checkpoints by time, but only when asked, since that makes the bytes depend on the clock.

### 7.1 Index entries {#index-entries}

An index entry holds:

- the block's offset, length and kind;
- its track id and number;
- the first item number and the item count;
- the first and last tick and the latest end;
- for a summary block, its level and tree;
- for a chunk, its unit hash ([below](#track-hash)).

Chunk, summary and records blocks are all listed. A records block of data carries no track id or ticks, since its
records may be on many tracks.

The entry of a records block of data holds only its record count, under track id 0.

In an aligned file, a checkpoint goes into the gap before the next chunk payload. A crash costs at most the chunks not
yet written, and a reader rescans only the stretch after the newest checkpoint.

The index block follows ([index module docs](https://github.com/vhrdtech/ww_tape/blob/main/ww_tape/src/index/mod.rs)).
The header's count is the entry count:

```text
off size field
 24 u32  session tag, as in every block
 28 u32  head CRC: CRC-32C over bytes 0..28, then 32..entries offset
 32 u32  entries offset from the block start
 36 u32  tracks n in the directory, 0 in a checkpoint
 40 u64  self offset: where this block starts in the piece
 48 u64  previous index block of the piece (a checkpoint's previous checkpoint, a final block's previous final
         block), u64::MAX for none
 56 u16  flags: bit 0 final (entries sorted by track id, then offset, state records of one block in block order; a directory
         of tracks follows)
 58 u16  entry length, 128 (a reader reads an entry's first 120 bytes: it takes longer entries, and refuses
         shorter ones than 120)
 60 ...  n × track: id u64, first entry u32, entry count u32, CRC-32C of its entries u32, reserved u32
 ...     entries, each:
           0 u64 offset of the block in the piece    8 u32 its length        12 u8 its block kind
          13 u8  summary or overview level (else 0) 14 u8 summary tree      15 u8 flags: bit 0 repeat, bit 1 range, bit 2 unhashed (no unit hash), bit 3 relation break, bit 4 tile
          16 u64 track id (0: none)                 24 u32 track number      28 u32 0 (a state record's below)
          32 u64 first item number                  40 u64 items (samples, records)
          48 i64 first tick                         56 i64 last tick         64 i64 latest end
          72 u64 smallest value of the track's range relation in an events chunk (flag bit 1), else 0
          80 u64 its largest value, else 0
          72 u128 a grid chunk's tile number (flag bit 4) in place of the range, else 0
          88 [32] a chunk's unit hash, else 0
         120 [8] reserved: written zero, not read
len-4 u32 CRC over bytes 0..len-4, as in every block
```

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/index_entry.svg"
<figcaption>The entry of the same <code>phase_a</code> chunk in the piece's final index, at byte 412 of the index block: the
chunk's offset, length and kind, its track, its items and ticks, no range relation, and its unit hash. Drawn by
<code>ww_tape_figures</code> from the drive's recording at 1/1000 of its data.</figcaption>
</figure>

Bytes 72..88 of an events chunk's entry hold the range of its track's range relation. A grid chunk's entry holds its
tile number there instead ([The tile number](#the-tile-number)). A series chunk of no fixed axes leaves them zero.

A final index longer than `INDEX_LEN_MAX` (1 MiB) is several final blocks. Each holds whole tracks in ascending track id
and points back at the one before.

### 7.2 State entries {#state-entries}

A block of state records gets one **[[state entry]]** per record instead of one entry per block. Such a block is one whose
first record is on track 0, or any block marked `FLAG_REPEAT`.

State entries sit under track id `STATE_TRACK_ID` (`u64::MAX`).

A reader therefore finds the item in force at a place, or an item by its hash, without reading the blocks.

Beside the block's offset, length and kind, a state entry holds:

```text
 13 u8  the record's slot: 0 a state track's record, else what it defines (DefinitionKind)
 15 u8  bit 0: the block is marked FLAG_REPEAT
 24 u32 the state track, or the number the definition defines (a track's, a clock's, a map's from clock)
 28 u32 a map's to clock, else 0
 32 u64 0, reserved for the record's item number       40 u64 0, reserved
 48 16 bytes the item's hash (ItemHash)                 64 i64 the record's time in ticks, 0 on track 0
```

The slot codes (`DefinitionKind`) are:

- 1 producer, 2 track, 3 bundle, 4 clock;
- 5 map, 6 item, 7 layout, 8 schema;
- 9 to 15 for a definition the writer doesn't know: its discriminant plus 1, at most 15.

A code is a `Describe` variant's discriminant plus 1. A newer writer therefore lists a definition under the code an
older reader computes for it. The older reader's content check then agrees with the entries.

Byte 14 is 0, and bytes 16..24 hold `STATE_TRACK_ID`.

A piece lists at most `IndexPolicy::state_entries_max` state entries, 1 Mi by default. A write past that is refused
(`IndexError::StateEntries`), and `PieceChain` then cuts to a new piece and writes it there.

### 7.3 Close and tail {#close-and-tail}

`TapeWriter::close` ends a piece with three blocks:

1. A **final index** lists every block again, sorted by track, with the state entries of one block in block order. Its
   head holds a directory of tracks and a CRC per track's entries.
   - A reader of one track out of thousands reads the head and that track's list.
2. The **close** block (kind 8) states why the writer stopped: closed or aborted (`TapeWriter::close_as` takes the
   reason). It holds the BLAKE3 content hash of
   every byte before it and where the newest checkpoint and the final index are.
   - A close that a cut wrote names the next piece.
   - Per track, the close also holds the chunks, items and ticks the piece holds.
3. The **tail** (kind 9) is a fixed-size block of 164 bytes (`TAIL_LEN`) and the last bytes of the file. It gives the
   piece's length, and therefore where the piece starts, and where its close is.

Nothing is repeated at the close. Offsets count from the start of the piece: the index stays valid when files are
concatenated.

A lost index block costs a scan to the neighbouring checkpoint, and a lost tail or close only the content-hash check.

#### 7.3.1 Aligned piece ends {#aligned-piece-ends}

A closed piece is a multiple of `PIECE_ALIGN` (4 KiB) long. The writer pads the close record's body with zeros after
its track rows (`piece::close_padding`).

The close states the padding at close offset 172, and the CRC covers it. The content hash still covers the bytes
before the close, and the tail stays the piece's last bytes.

Closed pieces therefore clone back to back into one file with nothing between them, as reflinks on btrfs or XFS (two
Linux file systems).

Only the end is aligned: the blocks inside stay where they are, and an unclosed piece ends where it ends.

#### 7.3.2 Closing more than once {#closing-more-than-once}

A close record ends a write session, not the piece. A writer can open a closed piece again and write on after its tail.

For example, an editor that opens a project file, records for a minute and saves it adds to the same piece each time.

The writer then places an **append head** (kind 13, 84 bytes, `APPEND_HEAD_LEN`) right after the tail. The append head
names the close record and the tail it goes on from, and the content hash that close states.

New blocks and checkpoints follow, then a final index, a new close and a new tail. Nothing before the append head
changes:

```text
piece head, blocks, checkpoints, final index, close 1, tail 1      closed once, a multiple of 4 KiB
append head naming close 1 and tail 1
blocks, checkpoints, final index, close 2 (previous: close 1), tail 2      closed twice
```

The new close's content hash chains over the old one:

- It is BLAKE3 of the old close's hash, then of the bytes from the old close record to the new one.
- Each chunk payload goes in by its hash, as in the first close.
- The writer never reads the blocks of data before the old close again, but the last chunk of each track.
- Of a piece with chunks, it reads the index too ([resuming a closed piece](#resuming-a-closed-piece)).
- The old close's hash still checks the piece up to it.

Each close names the one before it (close offset 88), and `TapeReader::closes` lists them oldest first.
`TapeReader::verify` checks each stretch between two closes against the hash the earlier close states.

##### 7.3.2.1 What an append costs {#what-an-append-costs}

Every close pads its piece to a multiple of `PIECE_ALIGN`, in the unaligned layout too. An append of one small block
therefore adds 4,096 bytes, most of them the close record's padding.

A writer that resumes a closed piece and closes it again with nothing appended writes nothing. The file keeps its bytes
and its modification time, and the close in force stays the last.

The padding lets closed pieces clone back to back, as [Aligned piece ends](#aligned-piece-ends) explains. Ten
thousand short appends thus add at least 40 MiB to a piece.

##### 7.3.2.2 The index after an append {#the-index-after-an-append}

The final index of a later close lists only the blocks since the close before it. Its first block points back at that
close's final index, so a reader of the tail still reaches every block of the piece.

A close after an append always follows a final index block of its own, an empty one when no block came since.
Checkpoints written after an append chain back into the old final index.

A reader that walks the index chain back from a checkpoint therefore meets final index blocks on the way. It takes the
close right after the newest of them.

##### 7.3.2.3 The close in force {#the-close-in-force}

The last close with a whole tail is the close **in force**: a reader takes the piece as closed by it. Bytes after its
tail are an **[[unclosed tail]]**, like the end of an unclosed piece.

A tail is whole only right after the close record it names, as the last bytes of the piece it states. Any other good
tail ends no close, and a reader never takes it as the end of the piece.

`TapeReader::unclosed` names the bytes after the close in force, and `TapeReader::verify` says how far it checked
(`Verification::unchecked`). `TapeReader::closed_entries` lists only the blocks the close in force covers.

An unclosed tail is a writer still writing, or one that a crash stopped. A reader reads its good blocks as it reads an
unclosed piece, never as damage of what the close covers.

A torn append head, or bytes that read as no block, leave the piece closed. `PieceInfo::cut` then names where they
start.

Only the last piece of a chain is written on. A close that names a next piece is never followed by an append head, so
the links between pieces never move.

##### 7.3.2.4 The close summary {#the-close-summary}

A close may carry a **[[close summary]]**: one block (kind 14) whose body the caller gives and the format never reads. A
journal, for example, keeps there the counts and names of what one input added.

`TapeWriter::close_with_summary` places the block right before the final index, and the close record names it by its
offset (close offset 176) and length (184).

The body holds at most `CLOSE_SUMMARY_MAX` bytes (1 MiB); a longer one is refused before anything is written.

The content hash covers the close summary like any other block, so a verify checks it.

`TapeReader::close_summary` reads it from the tail without a scan: the tail names the close record, which names the
block. The reader refuses a block that lies past the close record, is of another kind, or fails its CRC.

A resume takes the close summary as part of the end of a piece. A crash that tears the close or tail after it drops
the summary with the final index and the close.

The piece head, the close and the tail follow ([piece module
docs](https://github.com/vhrdtech/ww_tape/blob/main/ww_tape/src/piece/mod.rs)). Each header's count is the track count:

```text
piece head (kind 10)
 32 [16] piece id                 48 [32] owner id                80 u64 sequence number in the chain
 88 u32  flags: bit 0 has a previous piece                        92 u32 previous session tag (0: none or unknown)
 96 [32] previous piece's content hash                           128 [16] previous piece's id
144 u16  track row length, 96 (a reader takes longer rows and reads their first 96 bytes)
146 u8   integrity (Integrity): 0 Full, 1 CrcOnly, 2 None (a reader refuses another value)    147 [5] reserved
152 ...  tracks × 96, one per track carried over from the pieces before:
           0 u64 id    8 u32 number
          12 u32 flags, as a close row's: bit 0 incomplete (from the pieces before); bit 1, a head row's only:
                 not carried, the piece holds no definition of the track, its cut had no room for it
          16 u64 next item (the first item number this piece holds)
          24 i64 from tick (no item of the track here starts before it)
          32 [32] owner (TrackDef::owner; zeros: the piece's owner)
          64 [32] track hash before this piece ([TrackHash])

close (kind 8)
 32 u8   reason (1 closed, 2 aborted; a piece that goes on in the next one, next id below, is closed)
 33 u8   reserved
 34 u16  track row length, 136 (a reader takes longer rows and reads their first 136 bytes)
 36 u16  removed-piece row length, 56 (a reader takes longer rows and reads their first 56 bytes; 0 with no rows)
 38 [2]  reserved
 40 [32] content hash: BLAKE3 of the piece's bytes before this block, each chunk payload by its hash (below); of
         a later close, BLAKE3 of the previous close's hash, then of the bytes from it to this block, the same way
 72 u64  newest checkpoint index offset    80 u64 last final index offset
 88 u64  previous close record's offset (u64::MAX: the piece's first close)
         (offsets u64::MAX for none)
 96 [16] piece id    112 [16] next piece id    128 [32] owner id    160 u64 sequence number
168 u32  flags: bit 0 the piece has a head    172 u32 padding: zero bytes after the rows, below [PIECE_ALIGN]
176 u64  close summary offset, u64::MAX for none    184 u32 its length
188 u32  removed pieces, at most REMOVED_PIECES_MAX: their rows follow the tracks
192 ...  tracks × 136, one per track with a counted chunk or record in the piece, ascending id then owner:
           0 u64 id    8 u32 number    12 u32 chunks (0: records only, the range fields below 0)
          16 u64 first item    24 u64 items    32 i64 first tick    40 i64 last tick
          48 i64 latest end (of the chunk blocks in the piece)
          56 [32] owner (TrackDef::owner)
          88 [32] track hash after this piece ([TrackHash])
         120 u64 records: the track's counted records in the piece
         128 u32 flags: bit 0 incomplete, the track hash leaves out units a resume kept unread    132 u32 reserved, 0
    ...  removed pieces × 56, ascending distinct sequence numbers (RemovedPiece):
           0 u64 sequence number    8 [16] piece id    24 [32] content hash of its close in force
    ...  padding zeros, covered by the CRC

tail (kind 9), TAIL_LEN bytes, the last of the piece
 32 u64  piece length, from the piece's start to the end of this block
 40 u64  close offset    48 u32 close length    52 u8 reason    53 u8 flags (as the close's)    54 u16 reserved
 56 [32] content hash    88 [16] piece id    104 [16] next piece id    120 [32] owner id    152 u64 sequence
160 u32  CRC

close summary (kind 14), right before the final index of the close naming it
 32 [n]  the caller's body, at most CLOSE_SUMMARY_MAX bytes    32 + n u32 CRC

append head (kind 13), APPEND_HEAD_LEN bytes, right after the tail of the close it goes on from
 32 u64  that close record's offset    40 u64 that tail's offset    48 [32] that close record's content hash
 80 u32  CRC
```

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/tail.svg"
<figcaption>The tail of the drive's first piece: the last 164 bytes of its 8,192. It names the close at byte 6,486 and its
length, the reason 1 (closed) and the flags 1 (the piece has a head). The content hash, the ids and the sequence
number 0 follow. Drawn by <code>ww_tape_figures</code> from the drive's recording at 1/1000 of its data.</figcaption>
</figure>

### 7.4 Piece ids and session tags {#piece-ids-and-session-tags}

The writer derives the ids with BLAKE3 in its key derivation mode:

- A chain's first piece id is the first 16 bytes of `derive_key("ww_tape first piece", owner id ‖ chain number as u64
  LE)`.
- Each next piece id is the first 16 bytes of `derive_key("ww_tape piece id", content hash ‖ piece id)` of the piece
  before.
- A session tag from a seed is the first 4 bytes of `derive_key("ww_tape run tag", seed)`, made writable: never 0 nor
  FFFFFFFFh.

A piece id is never all zeros.

### 7.5 Content hash {#content-hash}

The **content hash** in the close and the tail is BLAKE3 over the piece's bytes before the close record, in order. One
exception applies: the payload of every chunk block that reads whole is replaced by the 32-byte BLAKE3 hash of that
payload.

The payload runs from the chunk's payload offset to its CRC: the columns and their 16 zero bytes. Every byte is
therefore covered, and each payload is hashed once, for the content hash and the track hash both.

The hash walk finds the blocks from the start of the piece. A block that passes its checks goes into the hash as a
block.

Bytes that don't form a valid block go in as they are, up to the next place where the block magic stands. The hash
therefore depends only on the bytes.

A resume and a reader thus get the same hash over damaged bytes that a resume kept.

A header can claim a long block that then fails its check, and the walk has read those bytes for nothing. The walk keeps
a count of such wasted bytes.

It stops trying places as blocks once the count passes 4 bytes per byte walked (`WALK_MISS_PER_BYTE_MAX`) plus 1 MiB
(`WALK_MISS_BASE_MAX`).

Crafted headers therefore cost at most a fixed multiple of the piece's length, never its square.

### 7.6 Track owners {#track-owners}

A **track** is told apart by its id and its **owner** (`TrackDef::owner`, 32 opaque bytes outside the identity). All
zeros mean the piece's owner.

Tracks of one identity from several owners therefore share a file, each with its own number, item numbers and hash. The
head and close rows carry the owner.

The index entries name the track number, which the close rows and the definitions resolve.

### 7.7 Track hash {#track-hash}

Each track has a running **track hash** over its content as written, in the order written. It is 32 zero bytes before
the track's first unit.

Each unit updates it to `h = BLAKE3(h ‖ BLAKE3(unit))`. The inner hash is the **unit hash**.

The writer computes a chunk's unit hash once and stores it in the chunk's index entry (bytes 88..120). The hash of a run
of whole chunks therefore folds from the index without reading a payload (`TapeReader::range_hash`).

The units are:

```text
chunk:  01, kind u8, count u32, the head's bytes 48..80 (first item, first tick, last tick, latest end) and
        81..end of the column entries (columns, time mode, end mode, a series grid and scale, the column
        entries), then the payload's BLAKE3 hash
record: 02, ticks i64 (0 without a time), item length u64, the item's bytes
```

Only counted blocks are units: chunk blocks and the records of records blocks. Puts and copies (blocks marked
`FLAG_REPEAT`), summaries, definitions and the blocks of the file structure are not.

The hash leaves out where a block lies: the session, seq, track number, payload offset, padding and segment CRCs. A
track written alone or among others, in one piece or many, in one file or many, therefore has the same hash.

A state track's hash stays 32 zero bytes, since its records are all puts. The piece head carries each track's hash
before the piece, and the close its hash after it.

`Closed::tracks` reports every track's hash when a piece ends.

A record's unit hash is not stored: records are listed per block, not per track. A reader therefore folds them by
reading their blocks.

### 7.8 Incomplete track hashes {#incomplete-track-hashes}

A resume may keep a damaged block that the index lists. It folds a damaged chunk by the unit hash in the chunk's entry.

The track hash is then that of the piece written without the damage.

The records of a damaged records block of data are in no entry, and the block's entry names no track. Each track that
may hold records leaves them out of its hash after the piece.

The next pieces carry that hash on. Such a track is one the piece defines or its head carries, other than a series or a
state track.

The close marks the track's row incomplete (flag bit 0, `TrackRange::incomplete`), as it does for a damaged chunk whose
entry holds no unit hash.

The mark travels with the hash. The next piece's head row carries it (row byte 12, the same flags,
`TrackStart::incomplete`), and every later close row of the track keeps it.

A piece therefore still says so when the piece that lost the units is gone. A reader refuses a row flag it doesn't know,
in a head row as in a close row.

A head row's flag bit 1 (`TrackStart::not_carried`) says the piece doesn't carry the track's definition: its cut had no
room for it in the index. A close row never sets it, and the next cut sets it afresh. A reader of the piece without
the pieces before names such a track in `DefinitionsAt::not_carried`. A marked row whose definition is in force all
the same is not named.

## 8 Tape pieces {#tape-pieces}

A recording can be cut into **pieces**. Each piece is a complete file that reads alone, and it holds, in order:

- a piece head block;
- the layout and the producer;
- the data, with the definitions and state it needs close behind it;
- its checkpoints;
- the final index, the close and the tail.

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/piece.svg"
<figcaption>A closed piece, each block to scale on rows of 1 KiB: records blocks and events chunks with the puts they need,
two checkpoints, the final index, the close with its padding, and the 164-byte tail ending on 12 KiB. Drawn by
<code>ww_tape_figures</code> from a small piece that <code>PieceChain</code> wrote.</figcaption>
</figure>

`ww_tape::PieceChain` writes the pieces as `dir/00000000.wwt`, `00000001.wwt` and so on, through
`ww_tape_fs::FilePieces`. It cuts before a chunk by size, chunk count or age (`CutPolicy`).

`TapeWriter::cut_with` cuts by hand.

The **piece head** (kind 10) names the owner, the piece's sequence number, its id, and the previous piece's content hash
and id. Per track, it says where the track's items continue.

The previous piece's close and tail name this piece's id, which the cut derives from the previous piece's id and
content hash. The chain is therefore checked both ways (`ww_tape::piece::check_chain`).

Pieces written back to back are one valid tape. With `PieceTapes::One`, the chain writes them this way into one file.

With `TapeWriter::cut_in_place`, a cut then starts the next piece right behind the close. A session that opens the
file again writes on after the last piece's close ([Closing more than once](#closing-more-than-once)).

A chain whose first pieces were deleted, as in a rolling window, reads as *head cut*, not as damage. A chain whose last
piece has no tail is open: it is being written, or a crash cut it off.

A close record may name pieces of its chain that were removed on purpose. Each takes 56 bytes after the track rows:
its sequence number, id and content hash. `check_chain` links over such pieces. A chain whose first pieces are named so
reads as removed, not head cut ([Pieces removed on purpose](pieces.md#pieces-removed-on-purpose)).

### 8.1 The chain report {#the-chain-report}

`check_chain` takes a `PieceSummary` of each piece at hand (`PieceInfo::summary` gives one) and returns a
`ChainReport`:

- each link between neighbours (`ChainLink`): linked, broken, a gap, removed pieces, or `HeadLost`, a link checked from
  one side only because the later piece's head is damaged;
- whether the chain's start was deleted (`head_cut`) or removed on purpose (`head_removed`, the count of pieces);
- how the chain ends;
- the pieces that do not belong (`strays`): a second piece with one sequence number, or a piece of another owner;
- the pieces whose head names an integrity level the reader does not know (`refused`);
- the pieces the closes at hand name as removed (`removed`).

`ChainReport::is_whole` says whether all is well, and takes removed pieces as part of a whole chain.

A hole of removed pieces is checked at its ends only. The piece before the hole names the first removed piece's id, and
the piece after it names the last removed piece's content hash and id. No piece at hand names the rows between, so a
hole of two pieces or more links whatever those rows say. A row that disagrees with either end reads as broken, and a
hole the rows do not cover whole reads as a gap. A head removed on purpose needs rows from sequence number 0 on.

Rows go on the last piece and name only pieces before it, so the last pieces of a chain cannot be named as removed. To
remove the piece that holds rows later, name its rows again on the new last piece. The writer refuses:

- a row for a piece that is not before its own;
- a second row for one piece with another id or hash;
- more than `REMOVED_PIECES_MAX` rows in one close.

## 9 Resuming after a crash {#resuming-after-a-crash}

[When the power fails](crashes.md) explains resuming with the drive's recording as its example.

`TapeWriter::resume_on` reopens an unclosed piece and goes on writing it. For a file, `ww_tape_fs::writer::resume`
opens `NAME`, closed or not.

The resume finds the piece as `TapeReader` does. It then walks the piece's bytes from the start to where the reader
stopped, block by block, as the content-hash walk does.

### 9.1 What a resume keeps {#what-a-resume-keeps}

The walk rebuilds the writer as it was:

- the content hash;
- the index entries;
- the track ranges;
- the state records each block holds (the writer's memory of what lies near);
- the counts;
- the session and the next seq.

In bytes that no block explains, the walk tells two cases apart. A block that the index lists is kept by its entry,
as a damaged block, when it ends before the next good block. Other bytes are junk: the resume keeps them and never
lists them.

A damaged block that the index lists stands for what its entries say. A state record in such a block leaves its slot
unknown, and the next put writes it again.

A damaged chunk goes into its track's hash by the unit hash of its entry. The records of a damaged records block
cannot go into a hash. The close therefore marks the hash of each track that may hold records as incomplete
(`TrackRange::incomplete`), and the piece still verifies.

The resume keeps the piece up to the end of its last good block, counted or not. It cuts off the rest only when that
is a torn end. A torn end may hold a final index, or a close without its tail.

It also drops blocks marked `FLAG_WITH_NEXT` at the end of what it keeps. The call they belong to is given again whole
and writes them again.

`Resumed` says what the resume kept: `kept_bytes`, `dropped_bytes`, `cut`, where the reader stopped taking blocks,
and `dropped_run`, the marked blocks dropped from `kept_bytes` on.

### 9.2 What counts as a torn end {#what-counts-as-a-torn-end}

A torn end is what a crash leaves of the last write: whole blocks, then a block that the bytes stop inside of. That
block has fewer bytes than a block header, or a header that names more bytes than are left.

Blank bytes (zeros) up to the end count as a torn end too. A file holds them where it grew but its data was never
written. Anything else in the bytes to cut is damage, and the resume refuses with `ResumeError::Damaged`.

A disk writes a 512-byte sector whole or not at all, so a write that a crash cut off leaves zeros from a sector
boundary. The resume reads the bytes to cut with that in mind:

| Bytes to cut | Resume |
|---|---|
| a record whose header names more bytes than are left | cut, unless its bytes show it was whole |
| a whole-length record that fails its CRC, its zeros crossing a sector boundary | cut |
| a whole-length record that fails its CRC, its zeros starting in its last sector, after it, or none | refused |
| bytes that start no record | refused |
| a whole tail that does not end the close record before it | refused |
| a whole close record, then a tail's length or more of bytes that are not all zero | refused |
| a tail blank from its first byte, or zeros from a sector boundary to the end | cut |

A record shows it was whole when a block start lies inside its bytes. It also does when its bytes read as a good block
once its length field names them. A tail lies inside one sector, so a crash leaves it whole, short or blank.

The zeros of a record are the zero bytes from inside it to the end of the file. They count from where those bytes
start, also where they were zero before, as in a close record's padding.

Rot that zeroes the end of a complete close across a sector boundary looks the same, so a resume cuts that close. The
bytes cannot tell it from a crash, and a stricter rule would refuse real crashes. Reading and verify never change a
file.

Take a 12,288-byte piece closed three times, whose last final index starts at offset 9513 of the file. Zeros from 9613
to the end cross the sector boundary at 9728, so a resume cuts the 2,775 bytes from 9513 on.

For example, one flipped bit makes the last tail fail its CRC, so the reader takes the close before it. The resume
refuses and keeps every byte, also when the tail's last byte is zero.

The bytes alone cannot tell the last row from a crash during the close. The resume then drops that close.

### 9.3 When a resume refuses {#when-a-resume-refuses}

A resume never cuts off a good block or damage. Where cutting could lose more than a torn end, it refuses and leaves
the bytes as they are:

| Refusal | When |
|---|---|
| `ResumeError::NoPiece` | there is no piece to resume; `ww_tape_fs` reports `FsError::NoPiece` with the path |
| `ResumeError::Damaged { offset }` | the bytes to cut hold damage rather than a torn end: a whole record that fails its check, bytes that start no record, or a tail that does not end its close |
| `ResumeError::GoodBlockAfterCut { offset }` | a good block of the piece's sessions lies behind the bytes the reader did not take; with no block kept, a good block of any session counts |
| `ResumeError::LookSpent { offset }` | the look for good blocks ran out before the end of the bytes |
| `ResumeError::HashWalkSpent { end }` | the kept bytes claim so many blocks that do not read that a reader's content-hash walk stops trying blocks before their end (`WALK_MISS_PER_BYTE_MAX`), so a block written after them would not verify |
| `ResumeError::Continued { next }` | the piece's close names a next piece: a cut ended it for good |
| `InputError::Layout`, `InputError::Producer` | the options name another layout or producer than the piece |

The look past the reader's stop reads at most `RESUME_LOOK_MAX` (128 MiB), twice the longest block. It checks at most
`RESUME_PROBES_MAX` (4) places whole, so crafted bytes cost a bounded read.

The resume checks the options before it cuts or writes anything. The newest layout and producer the piece names must
be the ones the options give, and each error names both. The chunk alignment matters because a reader trusts the
layout when it looks for the end of the data. The layout's gap settings need no check, because the writer never leaves
a gap.

A piece whose layout or producer record didn't survive is checked against nothing: the options say what it is.

### 9.4 A closed piece {#a-closed-piece}

A piece whose last close and tail read whole is closed, and a crash, if any, came after the close. The resume cuts off
a torn append after the tail and writes on after it, an append head first.

The append head goes out before the first block written, so a close with nothing appended writes no byte.

A good block after the tail refuses the resume (`ResumeError::GoodBlockAfterCut`), unless an append head that names the
close leads to it. A damaged append head is such a case: the resume never cuts off the blocks after it.

Blocks after an append head that names a whole close and tail are the piece going on. The resume keeps that close and
goes on after the blocks.

A close that names a next piece, a cut, ends its piece for good. The resume marks a tape in memory closed and returns
`ResumeError::Continued` with the next piece's id.

A piece cut off inside its close record or tail is opened again. Its final index and close are cut off, and the next
close pads the piece to 4 KiB again.

#### 9.4.1 Resuming a closed piece {#resuming-a-closed-piece}

A resume of a closed piece reads no block of data before its close in force but the last chunk of each track. Without
chunks, an append to a large piece therefore costs about as much as an append to a small one.

The writer's state at the close comes from what the close names:

- the close record: each track's range, hash and counted records, and the session and seq of its block id;
- the piece head, and the layout and producer put right after it, which the options are checked against;
- for a piece holding chunks, the index chain and each track's last chunk, for the newest relation values.

A piece holding chunks costs more. The resume reads every final index of the piece, newest first, about 211 bytes per
chunk:

| Piece | Closes | Bytes read |
|---|---|---|
| 1,000 chunks, 1.06 MB | 1 | 226,961 |
| 1,000 chunks, 1.07 MB | 10 | 218,513 |
| 10,000 chunks, 10.5 MB | 1 | 2,114,165 |

Each final index lists only the blocks since the close before it, so more closes read no more. Nothing caps the read
but the piece itself: each final index points back to an earlier one, down to the first. Bytes after the tail, such
as a torn append, make a resume read the index chain too.

An index chain that does not read leaves the tracks with chunks unread. The next chunk of such a track then needs a
break the caller accepts (`TapeWriter::accept_relation_break`), so damage before the close never stops an append.

The record of what lies near starts empty. The writer thus puts again, after the append head, the definitions the
blocks after it need.

Only the close record, its tail and the bytes after them are walked and replayed. A crash during an append resumes
from the same close, so the piece ends with the same bytes as an append without the crash.

### 9.5 Same bytes as without the crash {#same-bytes-as-without-the-crash}

What the writer adds depends only on its `WriterOptions`, the blocks before it and the calls since. The same calls after
a resume therefore give the bytes and content hash of a piece written without the crash.

The producer gives again every call from the block that `TapeWriter::next_seq` reports, with the same definitions. A
call that placed several counted blocks, such as a chunk with deferred blocks in its gap, is given again whole.

A put is idempotent: an application puts again what it uses and needs no record of which puts made it.

The writer keeps no definitions: after the resume, the application passes its own with each call, as before.

### 9.6 Resuming a chain {#resuming-a-chain}

`PieceChain::resume_on` (`ww_tape_fs::chain::resume` for files) reads the closed pieces back from their tails. It then
deals with the last piece.

| Last piece | Chain of a tape per piece | Chain in one tape (`PieceTapes::One`) |
|---|---|---|
| unclosed | resumed | resumed |
| closed naming a next piece: the crash came during a cut | the next piece starts | the next piece starts |
| closed or aborted, naming no next piece | written on after its close | written on after its close |
| its piece head is damaged | refused | refused |

A chain therefore gains a piece only at a cut: a session that opens it again goes on in its last piece. A crash inside
the head of the next piece starts that piece again.

The resume refuses a last piece whose head is damaged, with `WriteError::Index`. Keeping its blocks would leave them
outside the chain, and dropping them would lose more than a torn end.

The resume starts at the lowest piece at hand: the pieces before it were deleted, by file, `MemPieces::remove` or
`MemPieces::drop_front`, and the chain is head cut. A hole between pieces at hand is walked over only when the closes
at hand name every missing piece alike, as for `ChainLink::Removed`. The cut before the hole names the first one's id,
and the head after it names the last one's hash and id. Any other hole refuses with `ResumeError::Gap`, which gives
the missing sequence numbers.

A piece after removed pieces whose head was cut off can't be started again, since the piece before it is gone:
`ResumeError::HeadLost`. A damaged head with bytes after it refuses as the piece's own resume does, with
`ResumeError::GoodBlockAfterCut` or `ResumeError::Damaged`.

### 9.7 Monotone relations after a resume {#monotone-relations-after-a-resume}

A chain resume also restores the newest value of each monotone relation, a per-item value that a track declares never
to decrease. It takes the range relation's newest value from every closed piece's index entries, which it reads anyway.

The other relations' values are in chunks. For each track, the resume reads one chunk of at most `MAX_BLOCK_LEN` (64
MiB): the track's last chunk in the newest closed piece that holds one. A resume of a chain of N tracks therefore
reads up to N chunks besides each piece's head, close, tail and index.

The writer cannot vouch for an order it cannot read. Where that chunk does not read, it refuses the track's next chunk
(`DescribeError::RelationUnread`). The call `TapeWriter::accept_relation_break(defs, track, relation, from)` lets it go
on. It checks the track's next chunk against `from` for that relation. It then marks the chunk as a [relation
break](#relation-breaks) (`FLAG_RELATION_BREAK`), which readers see.

The break waits for the track's next chunk, also across cuts. A crash before that chunk drops the break, and the
resumed writer refuses again. The writer marks a chunk that it encodes itself. A caller's block carries the flag
(`mark_relation_break`) exactly when a break waits, and is refused otherwise (`InputError::Flags`).

## 10 Reading by position {#reading-by-position}

`ww_tape::TapeReader` reads a tape through a `ByteSource`: anything with a size that reads bytes at an offset. A byte
slice, a file (`ww_tape_fs::FileSource`) and an HTTP resource read by range requests are byte sources.

The reader needs no file system and no threads. It therefore builds for `wasm32-unknown-unknown` (feature `alloc`
without `std`).

The reader finds the pieces from the end. A good tail gives a piece's start and close, and the previous piece's tail
sits just before that start.

Concatenated pieces therefore cost one small read each.

A piece without a tail is found from its newest checkpoint, looked for backwards from the end. The reader then reads the
stretch after that checkpoint block by block.

A piece without any checkpoint is read from the start. A raw medium is read forward instead (`OpenOptions::medium`,
[below](#sessions-media-and-damage)).

To find the layout in force over the stretch after the newest checkpoint, a reader reads at most `LAYOUT_READ_MAX`
(256 MiB) of definition blocks behind it.

Each piece the reader finds has a `PieceInfo`. Its `found` says how the reader found it:

| `Found` | The piece was found |
|---|---|
| `Tail` | by its tail |
| `Checkpoint { scanned }` | by its newest checkpoint or final index block, with `scanned` bytes after it read block by block |
| `Scan { scanned }` | without an index, `scanned` bytes read block by block |

A `PieceInfo` also lists what a scan met:

- `cut`: where the bytes stopped being good blocks, when a scan stopped before the end of the piece's stretch;
- `gaps`: the blank or bad stretches the session's data goes on after, as offset and length;
- `replaced`: the blocks a later copy with the same session and seq but other bytes replaced ([Duplicates](#duplicates));
- `version`: the format version of a piece of another version, which every lookup refuses ([below](#versions-and-unknown-blocks));
- `head_error`: why a good piece head did not read as the head, as an integrity level this reader does not know.

Every lookup in a piece whose head is of such a level returns that error (`ReadError::Index`).

| Method | Gives |
|---|---|
| `entries`, `track_entries` | where a piece's blocks are |
| `read_block` | one block, read and checked |
| `verify` | the piece checked whole ([below](#what-verify-checks)) |
| `chain` | how the pieces link up |
| `state_at` | the record of a slot in force at a place |
| `item_by_hash` | an item by its hash |
| `definitions_at` | every definition in force at a place |
| `state_entries` | a piece's state entries |

`track_entries` takes a track id. Two ids name no track: `RECORDS_TRACK_ID` (0) gives the records blocks of data, and
`STATE_TRACK_ID` (`u64::MAX`) gives the state entries.

### 10.1 State lookups {#state-lookups}

State is looked up through the index's state entries, not by a scan. A lookup takes the newest entry of the slot whose
block starts before the place.

When that block doesn't read, or its record isn't the one the entry names, the lookup takes the next older entry.

Damage is never an error: a lookup lists what didn't read. It also lists the stretches the index doesn't cover
(`uncovered`), since a newer record may lie there.

Such a stretch is a gap a scan read past, or the bytes past a cut. Which stretches a lookup lists depends on the lookup:

- `state_at`: those between the record and the place;
- `definitions_at`: those before the place, `u64::MAX` standing for the end of the piece;
- `item_by_hash`: those after the record.

The reader keeps the sorted entries in one cache over all pieces, at most `OpenOptions::state_cache_max` entries
(`STATE_CACHE_MAX`, 2 Mi, by default). The piece used longest ago goes first. A piece with more state entries than
the cache holds is sorted again for each lookup.

A lookup reads at most `OpenOptions::state_read_max` bytes (`STATE_READ_MAX`, 1 GiB, by default). It charges a coded
body's decoded length before it decodes it.

When that budget runs out, the lookup stops and sets `spent`, which is not damage.

### 10.2 What verify checks {#what-verify-checks}

`TapeReader::verify` checks a closed piece whole. It walks the piece from its start to its close in force and reads
the final index as it goes. A piece holds up when every check passes:

1. The bytes before the close record hash to the close's content hash ([Content hash](#content-hash)).
2. The final index and the blocks walked are the same set. Each block walked that its writer lists has exactly the
   entries the writer made of it, field for field. A byte-identical copy of the listed block before it is the one
   exception.
3. Each entry names a block walked, or a block that does not read whole in bytes no block explains. Such a block ends
   before the close and before the next block walked.
4. Each close row states the hash its track's blocks fold to from the head row's hash. A row says its hash is
   incomplete exactly where a damaged block a resume kept, or the head, leaves units out.
5. Each monotone relation starts each chunk at or above the chunk before it in the piece, except at a chunk marked
   `FLAG_RELATION_BREAK` (`ReadError::RelationOrder`).

The fields of check 2 are the offset, length, kind, track, items and ticks, and a chunk's unit hash and range
relation's range. The reader computes the unit hash from the chunk's bytes and the range from its relation column.
`TapeReader::range_hash` takes these same values from the index unread, and verify is their strict check.

Verify merges the final index's track lists in file order. It holds one entry per track list and the entries of one
block, whatever the piece's size. A final index of more than `VERIFY_LISTS_MAX` (466,432) track lists fails. A writer
gives a piece at most `TRACKS_PER_PIECE_MAX` track lists, a state list and a list of records of data.

The check ends at the close in force. `Verification::unchecked` names the bytes after it that no close covers.

### 10.3 Versions and unknown blocks {#versions-and-unknown-blocks}

The header and the CRC at the end stay the same in every version: a later version changes only the body. A block of
another version that goes on with the data ends the piece the reader is in and starts a piece of that version.

The reader lists that piece with its version (`PieceInfo::version`) and refuses to read it. A reader skips a block of a
kind it doesn't know by its length, as it skips definitions of a variant it doesn't know.

## 11 Sessions, media and damage {#sessions-media-and-damage}

[When bytes go bad](damage.md) and [Where the data ends on a raw card](raw_media.md) explain these rules with examples.

### 11.1 Sessions {#sessions}

A **session** is one writer's unbroken append: a producer start, a device boot, one pass of an importer. Every block of
a session carries the same session tag in header bytes 24..28, and its seq counts from 0.

A writer picks the tag at random, except for the first piece of a chain, whose tag is derived from the piece id. A piece
head names the session that ended before it (`PieceHead::prev_session`).

A reader keeps to one session at a time: a block of another session is stale data, unless a session may start there.
Where a session may start depends on the medium:

- In a file, a new session starts at a piece head. It also starts at a records block whose first record is on track 0,
  where another producer appended.
- On a raw medium, a session starts only at a piece head that names the current session as the one before it.

#### 11.1.1 Adopting a producer's blocks {#adopting-a-producers-blocks}

A piece on a raw medium is therefore one session. A writer that carries a producer's blocks, each with the producer's
own session, renumbers them into its own session with `TapeWriter::adopt_block`.

Lost blocks stay visible: a jump in the producer's seq leaves the same jump in the writer's. A producer restart (another
session) leaves none.

Blocks passed through with `write_block` keep their sessions. They therefore read whole from a file but as stale data on
a raw medium.

### 11.2 Media {#media}

The **medium** decides where a reader starts. A file is read from its start, or its pieces are found from its end.

A raw medium (`Medium::Raw`) is a SD card, raw NAND flash or a partition. It may hold blocks
of earlier recordings behind the end of the current data.

A reader therefore walks a raw medium forward, from its start or from a hint, and never trusts what it finds at the end.

For example, a card recorded twice holds the second recording's blocks, followed by the rest of the first. The first
recording's session tag ends the walk.

### 11.3 Blank space and gaps {#blank-space-and-gaps}

**Blank** space is bytes that are all 0x00 or all 0xFF, as an erased or preallocated medium reads. A blank or damaged
stretch is a **gap**, not the end, when the current session's next block follows it within the layout's `gap_max`.

The reader looks for that block at the first non-blank byte and at each multiple of `resume_align`. The layout
definition states both (`Layout`):

| Medium | `gap_max` | `resume_align` |
|---|---|---|
| file (`Layout::UNALIGNED`, `ALIGNED`) | 0: no gaps | 1 |
| SD card or eMMC (`on_sd_card`) | 4 MiB | 512 bytes, a sector |
| raw NAND (`on_raw_nand`) | two erase blocks | one page |

A reader's cost per gap is bounded, and the layout sets the bounds:

- A valid `Layout` has a `gap_max` of at most 64 MiB (`GAP_MAX_MAX`).
- It has at most 8192 resume places in that gap (`GAP_PROBES_MAX`, `gap_max / resume_align`).
- Any other layout definition doesn't decode.

Past a blank or bad stretch, a reader reads one block header per resume place. It checks a block whole only when its
header says it goes on with the data, at most 4 times per gap (`GAP_CHECKS_MAX`).

When none of them goes on, the stretch is the end of data.

On a raw medium, blank space longer than `gap_max` is the end too, and a reader reads nothing after it. An erased 64 GB
card therefore costs one gap's worth of reads.

A `Recovery::Resync` walk looks for where the data goes on once per stretch too, and never past where a
`Recovery::Prefix` walk ends. It reads at most `RESYNC_AHEAD_MAX` (128 MiB) ahead: the longest gap a layout allows
(`GAP_MAX_MAX`) and the block that ends it.

#### 11.3.1 Writing on raw NAND {#writing-on-raw-nand}

A writer on raw NAND programs each page once, in page order. At a sync, it pads the rest of the page with 0xFF and
starts the next block on the next page.

The padding reads as a gap, and the format needs nothing else; the cost is up to one page per sync.

On MLC NAND, a power cut can also corrupt an earlier, synced page that is paired with the page being programmed.
Keeping the paired pages of unsynced writes apart from synced data is the medium layer's task, not the format's.

### 11.4 Duplicates {#duplicates}

The same session and seq twice is a **duplicate**, not a cut: a writer that retried a write may leave a block twice.
With the same bytes, the reader drops the second block.

With other bytes, the reader takes the later one and reports it (`FileError::Replaced`, `PieceInfo::replaced`).

A salvage walk (`Recovery::Resync`) can't tell a rewrite from a copy met in damage. It reports the second block as a
conflict (`FileError::Conflict`, with both offsets and the second block's bytes) and takes neither for the other.

This holds however far apart the two are, within the last `RESYNC_SEQS_MAX` (65536) data blocks the walk handed out. A
copy with the same bytes is no conflict.

The walk handed out the first of the two before it met the second. A caller that keeps what a salvage hands out
therefore holds it back or can take it back.

Blocks of puts (`FLAG_REPEAT`) share seqs by design and take no part in this.

### 11.5 Walking damaged bytes {#walking-damaged-bytes}

`FileBlocks` walks the blocks of a byte stream in one of two recovery modes. `Recovery::Prefix`, the default, reads the
longest valid prefix and stops at the first damage. `Recovery::Resync` reports the damage and reads on.

Resync reads every block Prefix reads. In a stretch that Prefix reads past as a gap, Resync hands out the good blocks it
meets. These include a session that starts in the gap and the blocks after it. Past the gap, Resync goes on with the
session it had, as Prefix does.

Each report is a `FileError` variant:

| Report | Mode | Meaning |
|---|---|---|
| `Cut` | Prefix | the bytes from this offset on are left out, for the reason given; the walk ends |
| `Gap` | both | a blank or bad stretch that the session's data goes on after; the walk goes on |
| `Replaced` | Prefix | a later copy of the block before it, with other bytes, replaces it |
| `Bad` | Resync | the bytes at this offset are not a good block; the walk skipped to the next magic |
| `Truncated` | Resync | the bytes end inside a block |
| `Conflict` | Resync | two good blocks with one session and seq but other bytes |
| `Stale` | Resync | a good block of another session where no session may start; skipped by its length |
| `Refused` | both | a piece head of an integrity level this reader does not know; not damage, stepped over by its length |

A closed piece whose final index does not read stays a closed piece. `TapeReader::entries` fails on it, while
`TapeReader::track_entries` still gives each track whose own list reads. The reader does not read such a piece from its
checkpoints instead.

### 11.6 State near its use {#state-near-its-use}

**State near its use** keeps a piece readable when a block of definitions is damaged. A reader at any place finds what
it needs within `near` behind.

An item first written in a piece gets `copies` more records at later uses, at least 1 MiB apart. A lookup that meets a
damaged block lists it and takes the next older record of the slot ([Reading by position](#reading-by-position)).

## 12 Series tracks {#series-tracks}

A **series track** holds samples at an exact period, with no time per sample. Its items are `Numbers`, which hold:

| Field | What |
|---|---|
| element | the stored `ElementDesc` of one channel of a sample |
| unit | the type of one value: a `TypeRef` to an exact_units `Fixed<Ampere, Milli, i32>` or a `ww_si` quantity; None for a plain number |

Its `TrackDef` kind is a `GridDef`, which holds:

| Field | What |
|---|---|
| channels | elements per sample, 1 to 65535, interleaved |
| period | ticks per sample as an exact `Ratio64`: 48 kHz on a 1 ns clock is `62500/3` |
| scale | what a stored number is worth, as a `Magnitude`: `10⁻³` for that `Fixed` |
| summaries | the summaries the writer builds (`SeriesSummarySpec`) |
| stream | for samples kept in a media file, the stream they are in |
| axes | the fixed axes beside time, none for a series ([Fixed axes and tiles](#fixed-axes-and-tiles)) |

`Items::numbers::<Fixed<Ampere, Milli, i32>>(..)` takes the element from the type, and `GridDef::of` the scale
(`ww_tape::SeriesValue`). A writer refuses a series whose element the raw codec doesn't code. Its summaries need an
element of at most 2 parts.

### 12.1 Grids {#grids}

A series track has no records. Its samples are in series chunks, and each chunk's head carries the **grid** its samples
are on (`SeriesGrid`).

A chunk therefore times and scales its samples alone. A grid is a sample index at or before the chunk's first sample,
that sample's tick, and the scale of the chunk's stored numbers.

Sample `i` is at `grid_tick + (i - grid_index) · period`, rounded down. A stored number means `number · scale` of the
sample type's unit.

A grid takes 17 bytes and the scale: 19 bytes for a power of ten (`10⁻⁶` is `f4 00`).

For example, take the period `62500/3` and grid index 96 at grid tick 5000. Sample 100 then lies at tick 5000 + 4 ·
62500/3, rounded down.

The head's first and last tick are its first and last sample's ticks on that grid (`SeriesGrid::of_chunk` checks them).
The index lists each chunk's first sample and first tick.

A reader therefore finds the chunk of a sample or a time without reading a head.

Consecutive chunks of one stretch of recording carry the same grid. No sample time therefore moves by rounding, however
the samples are cut into chunks.

#### 12.1.1 Changing scale or starting a grid {#changing-scale-or-starting-a-grid}

With `SeriesGrid::rescaled`, a prefix change of an autoranging device keeps the grid at another scale. A chunk has one
scale, and a writer therefore completes the open chunk there.

`SeriesGrid::start` starts a new grid at a gap or a restart on the same clock. Such a grid starts more than a period,
rounded up, after the last tick of the chunk before, as `series::is_gap` checks. A device restart that starts a new
clock is a new track on that clock, since a track's clock is part of its definition.

The chunks are data like events chunks, written once and never repeated. A series track therefore takes the plain policy
(`TrackRepeat::Stream`).

A writer refuses a record, a put or an events chunk on a series track. It also refuses a series chunk whose ticks are
not on its grid.

### 12.2 Gaps and lost samples {#gaps-and-lost-samples}

A **gap** is a chunk on another grid than the chunk before it. Its first sample lies more than a period, rounded up,
after that chunk's last tick, and no sample lies between them.

A reader therefore draws no line across. The next sample on the same grid lies at most that far on.

The first and last ticks of two neighbouring index entries therefore tell a gap from a continuation or from lost chunks
without reading a head.

A new grid at most that far on (a phase shift) is refused (`SeriesError::GridTooClose`).

Sample indexes missing between two chunks of one grid are **lost samples**: a chunk was damaged or is in another piece.
Their ticks are known and their values are not, and a reader shows them as missing.

#### 12.2.1 The series timeline {#the-series-timeline}

`ww_tape::SeriesTimeline` collects a track's chunk heads in any order (`from_chunks`, `push_chunk`). It checks each
chunk against its grid and its neighbours:

- no sample lies on two grids or at two scales;
- a new grid starts more than a period after the chunk before, and before the chunk after.

The timeline keeps consecutive chunks of one grid and scale as one `SeriesStretch`. It gives any sample's tick and
scale, and says where the gaps are (`gap_before`).

### 12.3 From a wire batch to chunk columns {#from-a-wire-batch-to-chunk-columns}

A received `Series<Fixed<..>>` batch goes into chunk columns as integers. `SeriesValue::write_parts` puts each item's
stored number into the column as it is.

A `ww_si` quantity's prefix becomes the grid's scale. A change of prefix completes the chunk at the same item where the
`Series` restarts its numeric column.

The wire codecs of `Series<T>` and these sample codecs are separate layers. A batch is decoded once on the host and
coded again for random access in the file.

### 12.4 Sample flags {#sample-flags}

A sample may carry a u32 flags word: an overrange, a dropout, a marker. A series chunk keeps its samples' words in a
`Flags` column in codec `FLAG_RUNS` (5), as runs of consecutive samples sharing a word ([series module
docs](https://github.com/vhrdtech/ww_tape/blob/main/ww_tape/src/series/mod.rs)):

```text
n × run, n from 1 to FLAG_RUNS_MAX, ascending:
  u32  first: the run's first sample, counted from the chunk's first
  u32  count: samples in the run, 1 and up
  u32  flags: their flags word, not 0
```

A sample in no run has the word 0. A chunk with no flagged sample has no such column. An unflagged series therefore
costs no byte.

The runs lie within the chunk's samples and apart. Two runs that touch have other words, since a writer merges them.
The column is therefore in one form only, and a reader refuses any other (`FlagRunsError`).

A chunk holds at most `FLAG_RUNS_MAX` (4096) runs, 48 KiB. A writer completes the chunk before samples that would start
one more.

The chunk is the source of the flags. The series summaries OR them into their nodes. A writer resumed after a crash
rebuilds those nodes from the kept chunks alone, their samples and runs (`SeriesSummaries::push_runs`). The piece
therefore goes on with the bytes it would have had without the crash.

`Chunk::flag_runs` reads the runs in place. `FlagRuns::spans` cuts a chunk's samples where their word changes, and
`FlagRunsBuilder` writes the column as samples come.

### 12.5 Fixed axes and tiles {#fixed-axes-and-tiles}

A series track may have **[[fixed axes|fixed axis]]** beside time: the pixels of an image, the members of an
ensemble. Time stays the one axis that grows and the one with a clock. A fixed axis
has a known extent in cells.

A `GridDef` holds at most `GRID_FIXED_AXES_MAX` (31) of them, 32 axes with time. A series is the case of none.

A series definition takes 0 or 1 byte more than before fixed axes. The empty list of axes is one nibble: it fills a free
half byte, or takes a new byte (`phase_a` of the drive, with summaries). Its chunk heads keep their bytes. Its
index entries keep bytes 0..120 at their places, and every entry grew to 128 bytes.

Each fixed axis (`FixedAxis`) holds:

| Field | What |
|---|---|
| name | 1 to `AXIS_NAME_LEN_MAX` (64) bytes: `x`, `latitude`, `member` |
| extent | cells along the axis, 1 and up |
| tile extent | cells of the axis one tile holds, 1 to the extent |
| unit, scale | an exact_units type and the `Magnitude` a coordinate number is worth in it, as a series' samples have; None for a plain number |
| coordinates | where the cells are: regular, cell `i` at `origin + i · step` with a step not 0, or listed, one value per cell |
| downsample | whether space overviews halve the axis: yes for x and y, no for an ensemble member or a pressure level |

Listed coordinates are strictly increasing or strictly decreasing, at most `GRID_LISTED_MAX` (65,536) of them. A reader
refuses a definition that breaks one of these rules when it decodes it.

#### 12.5.1 Tiles {#tiles}

A chunk of a grid holds one **[[tile]]**: a run of time steps and one box of cells along the fixed axes. Each fixed
axis is cut into tiles of its tile extent. The last tile of an axis holds the cells left over. A tile is named by
its **tile coordinates**, one per fixed axis: its first cell divided by the axis' tile extent.

The chunk head carries them after the scale, one u32 per fixed axis, and byte 82 holds their count
([The chunk head](#the-chunk-head)). A head thus reads without the definition. A writer refuses a chunk whose
coordinates name no tile of its grid (`SeriesError::TileOutside`).

Inside a tile the cells run time outermost, then the fixed axes in definition order, row-major. One time step of a
tile is therefore one contiguous slab, at most `GRID_TILE_CELLS_MAX` cells, and a series' samples keep their order.

#### 12.5.2 The tile number {#the-tile-number}

The chunk's index entry carries its coordinates packed into one **[[tile number]]**, a u128 at bytes 72..88 under
flag bit 4 ([Index entries](#index-entries)). An entry has a fixed size, so it holds one field for all fixed axes,
not a slot per axis.

Each fixed axis gets a bit field of ⌈log2(tiles)⌉ bits, its tiles being the extent over the tile extent, rounded up.
The last axis takes the lowest bits. The widths come from the definition and are never stored; together they are at
most `TILE_NUMBER_BITS` (128), which a reader checks when it decodes the definition.

Take a weather ensemble of 51 members, 37 pressure levels, 721 latitudes and 1440 longitudes, in tiles of 1, 1, 180
and 180 cells. Its axes have 51, 37, 5 and 8 tiles, which take 6, 6, 3 and 3 bits, 18 in all. Tile (7, 3, 2, 5) is
`7 << 12 | 3 << 6 | 2 << 3 | 5`.

A reader takes a coordinate back by shift and mask, without division and without the other axes. Twenty axes of 10
values each take 80 bits, a grid that a u64 of tiles could not number.

#### 12.5.3 Finding the tiles of a box {#finding-the-tiles-of-a-box}

A box query asks for a time range and a box of cells, a range of cells per fixed axis. `TileLayout::tile_box` turns the
box into a range of tile coordinates per axis (`TileBox`). `TapeReader::grid_tiles` then keeps the track's index
entries whose ticks meet the time range and whose tile number lies in every range.

The query reads the index alone: no chunk is read. An entry a reader made by scanning a stretch no index covers holds no
tile number. For such an entry the query reads the chunk's head, a few hundred bytes. So it does for an entry whose
tile number names no tile of the grid.

The box carries the layout it was made for, and a tile number reads only by its own grid's layout. The query
therefore refuses a box whose layout differs from the track's definition at a chunk (`ReadError::GridLayout`).

#### 12.5.4 Summaries of tiles {#summaries-of-tiles}

Each tile of a grid has a [summary tree](#summary-nodes) of its own, the time pyramid of that tile. An item of the
tree is a time step. A node holds `2^base_shift` time steps of the tile, and each cell of each step is a sample of its
statistics. A level-0 node of a tile of 6 cells and 16 time steps thus summarises 96 samples per channel. Nothing is
downsampled along the fixed axes.

A tile's summary block carries header flag 5 (`FLAG_TILE`). After the scale it holds the count of fixed axes and the
tile's coordinates, one u32 each, as the chunk head does ([Summary blocks](#summary-blocks)). Its index entry carries
the tile number under flag bit 4, as the tile's chunks' entries do. A series has one tree, and its blocks keep the
bytes they had before grids: no flag, no tile. A writer refuses a summary block of a grid's tile its grid doesn't
hold, or a series block without a tile on a grid (`SeriesError::TileOutside`).

`TapeReader::grid_summaries` finds the summary blocks of one level of the tiles a box meets, as `grid_tiles` finds
their chunks. `ww_tape_summary::region_timeline` reads them and merges, per node, the nodes of every tile into a
region's timeline. A node summarises a whole tile, so a tile partly inside the box counts whole: the timeline is at
tile resolution. Each tile weighs by its cells, so an edge tile with fewer cells weighs less.

#### 12.5.5 Space overviews {#space-overviews}

A timeline zooms time; a field viewer zooms space at one moment. The time pyramid serves the first. **Space
overviews** serve the second: overview level k halves every fixed axis marked `downsample` k times. The other fixed
axes and time stay at full resolution. A one-axis series has no overviews, and its bytes are unchanged.

A cell of level k covers `2^k` cells as written along each downsampled axis, fewer at the axis' far edge. It holds
three statistics per channel and time step: the smallest value of its source cells, the largest and the mean. The
mean of an integer element is rounded to the nearest integer, ties to even; a float's mean is summed in f64.

Float cells, in any order of their source cells:

- A NaN is left out of all three statistics, as a summary leaves it out of its min and max.
- A cell is NaN only where every source cell is NaN.
- A cell stores no NaN count, unlike a series summary: a reader can't tell how many source cells were NaN.
- An infinity is a value: a cell holding one has it as its min or max and as its mean.
- A cell holding both infinities has a NaN mean.
- Of two zeros, -0 is the smaller.

Each tile as written has its own overview tile per level, with the same tile coordinates and tile number. Its tile
extent along a downsampled axis is the tile extent over `2^k` (`TileLayout::overview`). The levels go on as long as
every downsampled tile extent halves evenly (`TileLayout::overview_levels`), so a cell has one size across the whole
grid. A tile extent of 256 gives up to 8 levels, and one of 180 gives 2.

An overview chunk is a series chunk of the same track, of the time steps of the chunk it summarises:

| Where | What |
|---|---|
| head byte 83 | the level k, 1 to `GRID_OVERVIEW_LEVELS_MAX`; 0 in every chunk as written |
| head grid, scale, tile | those of its source chunk |
| payload | one `OverviewMin`, `OverviewMax` and `OverviewMean` column each, coded as a samples column; the flag runs |
| index entry | the level in the summary-level byte; the tile number under flag bit 4 |

It holds no samples column, and no other chunk holds the three statistics columns. An entry a scan made carries the
level without a tile number, so `grid_tiles` reads that chunk's head. A reader of format 0.8 reads an overview chunk
as damaged. Byte 83 was 0 in every series chunk, so its head check refuses it. The golden file `v1-overview.hex` pins
a piece of two tiles and their overviews.

A `GridHandle` builds a tile chunk's overview chunks on the producer's thread when it completes the chunk. Each level
comes from the min, max, sum and count of the level below (`ww_tape_summary::OverviewCells`). The chunk bytes a
handle allows count the overview chunks too: three columns of the level-1 cells.

The tile chunk and its overview chunks are one **unit** (`PieceChain::write_unit`). Each chunk but the last is marked
`FLAG_WITH_NEXT`. The writer places no deferred block or checkpoint inside the unit and cuts no piece in it. The cut
policy counts the unit's bytes and chunks after it, overview chunks among a piece's and a track's chunks.

A unit has at most `UNIT_CHUNKS_MAX` (17) chunks: a tile chunk and one overview chunk per level. A writer refuses a
marked chunk that would make a longer unit, a relayed one too (`InputError::UnitTooLong`).

`TapeWriter::write_chunk_unit` checks each chunk before it writes the first. A chunk that fails after that leaves a
torn unit: the writer stops, so no later chunk joins the unit, and a resume drops it.

A crash inside a unit leaves a run of marked chunks at the end of the piece. The resume drops it whole
([What a resume keeps](#what-a-resume-keeps)), and the handle sends the tile chunk again from its first time step.
So a run with a crash writes the bytes of one without. A tile chunk sent without overviews (`ChunkSender`, a relay of
chunks as written) is no unit and gains none. A relay passes another writer's units on as they are, marks and all.

What a reader shows of a unit:

A unit is a run ([Marking blocks of puts](#marking-blocks-of-puts)), so a reader shows all of its chunks or none:

| Reader | A unit's chunks |
|---|---|
| a scan of a piece | only with the last one: a torn unit is listed in `PieceInfo::dropped_runs`, and `grid_tiles` lacks its tile chunk |
| a follower (`ChainFollower`, `TrackFollower`) | the marked chunks only with the last one: a live view never shows a tile chunk before its overviews |
| the index of a closed piece (`grid_tiles`, `track_entries`) | every chunk of each unit, an entry each: only a resume closes a crashed piece, and it drops the torn unit first |

`ww_tape::Overview` reads the three columns in place, each of the tile's cells at the level times the chunk's time
steps. A viewer picks a level with `TileLayout::level_for`: the highest whose cell is at most a pixel along every
downsampled axis.

An overview saves bytes per tile, not tiles. A zoomed-out view of the whole grid at level k still reads every tile's
overview chunk, each `2^(d·k)` times smaller for d downsampled axes. A grid of a few hundred tiles reads well at any
level. A mosaic of many small tiles gets few levels and many reads.

## 13 Sample codecs {#sample-codecs}

[Series](series.md) explains series, sample codecs and why fixed point comes first.

The column's codec id says how a series chunk's sample columns (`ColumnKind::Samples`) are coded. What a sample is comes
from the track: `ww_tape::SampleShape` names the items' `ElementDesc` and the number of channels interleaved in the column.

The predefined elements are `I8` to `U64`, `F32`, `F64`, `ComplexI16`, `ComplexF32` and `Bool`. An element's code, 0 to
12, is how a summary head names it.

Each part of an element is one **lane**: the scalar it is read into, its stored bits, their order and a float's layout.

`ww_tape::Samples` reads a column in place and decodes any range of samples, and `ww_tape::samples::encode` writes one.
The codecs themselves live in `ww_tape_codecs::samples`, which works on lanes with no notion of a block.

Every codec gives the same bytes for the same values.

| Id | Codec | Elements | Stored as |
|---:|---|---|---|
| 0 | `RAW` | all | each lane at its own bits: a whole scalar little or big endian; other widths packed LSB or MSB first; a float of another layout as its own bits |
| 16 | `BIG_ENDIAN` | integers, floats | big endian |
| 17, 18 | `PACKED10_LSB`, `PACKED10_MSB` | integers of 16 bits and more | 10 bits per value |
| 19, 20 | `PACKED12_LSB`, `PACKED12_MSB` | integers of 16 bits and more | 12 bits per value, 2 values in 3 bytes |
| 21, 22 | `PACKED24_LSB`, `PACKED24_MSB` | integers of 32 bits and more | 24 bits per value |
| 24, 25 | | | reserved: refused as an unknown codec |
| 26 | `ALP` | floats | decimals as integers, frame of reference, bit-packed; other floats by their front bits through a dictionary |
| 32, 33 | `DELTA`, `DELTA2` | integers | differences (or differences of differences), minus their minimum, bit-packed |
| 34, 35 | | | reserved: refused as an unknown codec |
| 48 | `LZ4` | payload columns | a u32 decoded length (at most `LZ4_DECODED_MAX`, 64 MiB), then one lz4 block (`ww_tape::lz4`) |

Codecs 16 to 22 are the raw layouts of fixed widths, read on the same kernel as `RAW`. A writer describes the element
instead and codes it `RAW`; readers take both.

The codecs fall into three families:

- **Raw device formats** (0 to 22) are what a device sends, stored as it is. The reader converts only the samples it
  reads, and any sample reads in place.
  - An LSB-first column holds value `i` at bits `i·w` up of the column read as one little-endian number.
  - An MSB-first column counts bits from the top of byte 0, up to 56 bits per value.
  - A packed value of a signed element is sign-extended.
  - A float of another layout (f16, bfloat16, fp8) converts exactly to and from its scalar. A value its layout doesn't
    hold is refused on write.
- **Delta codecs** (32, 33) code groups of 128 samples on `ww_tape::bitpack`, the bit-pack primitive of the event time
  codec. Per lane, a group holds the first value (for `DELTA2` also the first difference), the minimum difference and a
  width.
  - The differences less that minimum follow at that width. A ramp or a constant therefore costs no bits per sample.
- **ALP** (codec 26) codes groups of 128 samples. It follows the paper by Afroozeh,
  Kuffó and Boncz at the SIGMOD conference, 2024.

### 13.1 The ALP codec {#the-alp-codec}

ALP stores floats that began as decimals as integers: a reading times 0.1, a price, or a value printed and parsed back.

Per group and lane, it picks one exponent `e` and factor `f`. It stores each value as a `d` such that `d × 10^f × 10^-e`
gives its bits back exactly.

It bit-packs the `d`s less their minimum. Values that don't come back bit for bit (NaN payloads, `-0.0`, infinities,
more digits) stay whole as exceptions inside the group.

Other floats take `rd`, ALP's second scheme.

The `rd` scheme codes up to 16 front bits through a dictionary of up to 8 entries. It bit-packs the rest, without the
low zero bits the group shares.

The encoder keeps the smallest form per group, and a group therefore never grows by more than 7 bytes over raw. The
layout is in the `ww_tape::samples::alp` docs.

### 13.2 Groups and the default codec {#groups-and-the-default-codec}

The grouped codecs start the column with a table of where each group ends, and every group restarts its prediction. A
range therefore decodes from its group's start, and damage costs only the groups it touches.

`Samples::group_bytes` names a group's bytes, to check against `Chunk::range_damaged`.

For data a device controls, fixed-point numbers are the default: an exact_units `Fixed` in an integer column with
`DELTA`.

The WireWeaver book's [series](https://ww.vhrd.tech/serdes/series/) page explains them. They take the fewest bits and
decode fastest.

ALP, the one float codec, is for float sources nobody controls.

The writer stores the codec id per chunk. It may therefore try several codecs on a chunk and keep the smallest.

### 13.3 Float codecs measured {#float-codecs-measured}

`ww_tape_codecs_benchmarks` keeps Gorilla, Chimp and delta on ordered float bits (`FLOAT_DELTA`, `FLOAT_DELTA2`) as
bench-only codecs to compare against.

`just bench-sample-codecs` measures every float codec on four kinds of data. Each set's first row is the same quantity
stored as fixed-point integers with `DELTA`:

- **motor_sim**: every variable of the VESC preset of `motor_sim`
  (github.com/vhrdtech/motor_sim at commit 35eab07), for 1 s at 25 kHz. The bench drives it from its rotor angle with a
  fan load, as `f32` and as `f64` (its `f32` variables widened).
  - Fixed point: `i32` at the finest power-of-ten step that keeps 24 bits at each variable's peak.
- **speech**: Roosevelt's Pearl Harbor address (Wikimedia Commons, public domain, `Roosevelt_Pearl_Harbor.ogg`, SHA-1
  `90239988...`), decoded to `f32` by lewton. Fixed point: lewton's 16-bit PCM of the same
  decode.
- **ADC**: 12-bit counts of 4 slow noisy channels, 250 000 samples each. The bench turns
  them into floats as firmware does: `counts × 3.3/4096` and `counts × 0.1`.
  - Fixed point: the counts.

Every value comes back bit for bit. [Sizes and speeds](sizes.md#samples-fixed-point-first) gives the ratio and the
times of every codec on every set. The bench's `--chunk` and `--repeats` change the chunk size and the timed passes.

## 14 Payload codec {#payload-codec}

An events chunk's payloads take two columns, both or neither (`ww_tape::payloads`). `EncodedPayloads::encode` writes
them from the items' bytes, and `Payloads::read` reads them back by item.

- **Lengths** (`ColumnKind::PayloadLengths`, codec 4, `payloads::LENGTHS`): per group of 128 items, a 13-byte record
  holds where the group's first payload starts and the group's shortest length. The record also holds a width of 0 to 32
  bits and where its packed lengths start.
  - Each group's lengths less that minimum follow, bit-packed at its width.
  - Payloads of one length cost the records only, and a record's start places a group's payloads without the groups
    before it.
- **Payloads** (`ColumnKind::Payloads`): the items' bytes back to back, `RAW` and read in place, or a payload codec such
  as `LZ4` (48), decoded once when the chunk is opened.

A group's lengths are stored in one form only:

- its minimum is its shortest length;
- its width is the bits the largest packed value needs;
- the bits after its last value are 0.

A reader refuses any other form. Damage to a width or a minimum therefore can't make a group read as other lengths of
the same total.

`Payloads::salvage` reads a chunk whose payload CRC failed, and checks each group of lengths against its neighbours.

`Payloads::is_suspect` reports as suspect only the items of a group that fails, or raw items with a byte in a bad
segment. A damaged lz4 column makes every item suspect.

### 14.1 Choosing the codec {#choosing-the-payload-codec}

The writer picks the payloads column's codec per track, chunk by chunk (`payloads::PayloadCodec`). The choice is not
stored anywhere but in each column's codec byte, so a reader needs nothing else.

| Choice | Payloads column |
|---|---|
| `Auto`, the default | each codec of `payloads::CODERS` tried in order; the first smallest kept when smaller than raw, else `RAW` |
| `Raw` | `RAW` always; no encoder runs |
| `Named(codec)` | that codec always, smaller or not; a number that is no payload codec is refused (`EncodeError::UnknownCodec`) |

`Raw` suits payloads that don't compress, such as encrypted traffic: it spends no time on an encoder.
`EventsOptions::payload_codec` sets the choice of an events handle. A caller that encodes its own chunks for a `ChunkSender` or a `TapeWriter` passes
it to `EncodedPayloads::encode_with` or `from_parts`.

### 14.2 Adding a codec {#adding-a-payload-codec}

`payloads::CODERS` is the registry of payload codecs past `RAW`: each entry a codec number, an encoder and a decoder.
Adding a codec takes three steps and no format break:

1. Take a free codec number and add its row to the codec ids of [The chunk head](#the-chunk-head).
2. Add its `PayloadCoder` to `CODERS`: encoder, decoder, and a decoded length capped as `LZ4_DECODED_MAX` caps lz4's.
3. Give its decoder a fuzz target and a hostile target, as every decoder has.

Files written before read as they did. A reader without the new entry refuses only that payloads column
(`PayloadsError::UnknownCodec`) and still reads the chunk's times and relations.

`Auto` tries every entry of `CODERS`, so a new entry changes what `Auto` writes for payloads it codes smaller. That is
a change of written bytes: a breaking CHANGELOG entry and new golden files, as any such change.

## 15 Reserved numbers {#reserved-numbers}

Some numbers are reserved: later features then need no format change. Readers of this version refuse each:

| Reserved | For |
|---|---|
| payload codec 49 (`Codec::SERIES`) | a payloads column holding one shrink_wrap `Series<T>` of the chunk's items, alone, at most 65535 items |
| sample codec 36 (`Codec::RUNS`) | run-coded discrete lanes (`U8`, `U16`, `U32`, `Bool`) |
| column kind 7 (`ColumnKind::PAYLOAD_LEAF`) | one leaf of the items' payload struct, the param naming the leaf |
| block kinds 11 and 12 (`BlockKind::POSTINGS`, `DICTIONARY`) | a relation's index per events chunk: each distinct value with its items; the distinct values and each item's index into them. Skipped by readers; writers refuse a definition that asks for one |
| `RelationValues::IndexOnly` | values given with each item only to feed an index, no column; writers refuse it until an index is built |
| summary head bytes 54..56 | the first channel or leaf of the block, 0 today; a grid's tile goes after the scale ([Summaries of tiles](#summaries-of-tiles)) |

## 16 Policies and their defaults {#policies-and-their-defaults}

A [[policy]] is a setting of the writer or the reader. It changes where blocks fall or what a reader spends, never what a
block means. A reader needs none of the writer's policies to read a tape. Track 0's state policy is the exception: the
layout definition states it.

The writer's policies (`WriterOptions`, `PieceChain`'s `ChainOptions`):

| Policy | Default | Sets |
|---|---|---|
| `WriterOptions::sync` | `SyncPolicy::Interval`, 1 s (`DEFAULT_SYNC_INTERVAL`) | when the sink syncs: only at the end, after every N blocks, or once the last sync is an interval ago |
| `WriterOptions::layout` | `Layout::UNALIGNED` | how blocks are placed, stated in the layout definition ([Blank space and gaps](#blank-space-and-gaps)) |
| `IndexPolicy::bytes` | 4 MiB (`DEFAULT_CHECKPOINT_BYTES`) | the bytes of blocks after which the next checkpoint falls; 0: never |
| `IndexPolicy::interval` | None | a checkpoint also once the one before is this long ago; the bytes then depend on the clock |
| `IndexPolicy::state_entries_max` | 1 Mi entries (`STATE_ENTRIES_MAX`) | the state entries one piece lists ([State entries](#state-entries)) |
| `NearPolicy::copy_spacing` | 1 MiB (`COPY_SPACING`) | the bytes from an item's newest record to where a use writes a copy |
| `NearPolicy::memory_max` | 262,144 slots (`MEMORY_MAX`) | the slots whose newest record the writer remembers |
| `NearPolicy::kept_bytes_max` | 64 MiB (`KEPT_BYTES_MAX`) | the bytes of newest state items the writer keeps for a cut to carry |
| `CutPolicy::bytes`, `chunks`, `interval` | none: 0 or None turns each off | when a chain cuts to a new piece: by size, chunk count or age |

A piece holds at most `CUT_SLACK` (1 MiB) past `CutPolicy::bytes`: the chunk that takes it over the limit, the index,
the close and the tail. A memory sink reserves the cut size plus `CUT_SLACK` when a piece starts.

A track's state policy (`StatePolicy`) is part of its definition, and track 0's is in the layout:

| Field | Default (`StatePolicy::DEFAULT`) | Sets |
|---|---|---|
| `near` | 32 MiB | how far behind a use an item lies at most, in the same piece |
| `copies` | 1 | the records an item gets after its first one in a piece |

The reader's policies (`OpenOptions`):

| Policy | Default | Sets |
|---|---|---|
| `medium` | `Medium::File` | read a file from its end, or walk a raw medium forward ([Media](#media)) |
| `start`, `layout` | 0, None | where a raw medium's walk starts, and its gap rules before the definitions state them |
| `scan_back_max` | 256 MiB (`DEFAULT_SCAN_BACK_MAX`) | the bytes looked through backwards from the end of an unclosed piece for its newest checkpoint |
| `state_read_max` | 1 GiB (`STATE_READ_MAX`) | the bytes one state lookup reads, coded and decoded ([State lookups](#state-lookups)) |
| `state_cache_max` | 2 Mi entries (`STATE_CACHE_MAX`) | the state entries the reader keeps sorted over all its pieces |

Past `scan_back_max`, a reader reads the unclosed piece from its start instead.

## 17 Caps on what a reader takes {#caps-on-what-a-reader-takes}

Every count, length, offset or layout value a reader takes from the bytes is bounded before it sizes an allocation, a
slice or a loop. The bound is a length already in hand (the block's, a table's) or one of these [[caps]].

A value past its cap is damage, refused with an error; no file a writer of this crate writes has one.

| Cap | Value | Bounds |
|---|---|---|
| `MAX_BLOCK_LEN` | 64 MiB | a block's length |
| `DECODED_BODY_MAX` | 64 MiB - 36 | a coded body's decoded length |
| `LZ4_DECODED_MAX` | 64 MiB | an lz4 column's decoded length |
| `MAX_BLOCK_CLOCKS` | 16 | the clocks of one block's clock table |
| `MAX_RECORD_HEAD` | 25 | a record head's bytes |
| `MAX_CLOCK_TABLE` | 81 | a clock table's bytes |
| `MAX_COLUMNS` | 255 | a chunk's columns |
| `SEGMENT_SHIFT_MAX` | 24 | a chunk's segment shift (16 MiB segments) |
| `MAX_SEGMENTS` | 16384 | a chunk's segments |
| `SERIES_ITEMS_MAX` | 65535 | the items of a chunk with a series payloads column |
| `FLAG_RUNS_MAX` | 4096 | the flag runs of a series chunk |
| `WIDTH_MAX` | 64 | a bit width (`bitpack::Width`) |
| `KERNEL_WIDTH_MAX` | 32 | a bit width the 32-bit kernels unpack |
| `SLOPE_MAX` | 2^55 | a start group's slope (Q16 ticks per item) |
| `ITEMS_MAX` | 16 Mi | the items of an events chunk |
| `INDEX_LEN_MAX` | 1 MiB | an index block a writer makes, and a reader looks for from the end |
| `CHECKPOINT_ENTRIES_MAX` | 8191 | entries a writer puts in a checkpoint index block |
| `NODE_SHIFT_MAX` | 62 | a summary node's items, as a power of 2 |
| `LEVEL_MAX` | 15 | a summary tree's level |
| `SCALE_LEN_MAX` | 64 | the scale bytes of a series chunk or summary |
| `FACTOR_MAX` | 2^53 | a factor to the common scale (`ww_tape_summary`) |
| `SeriesSummarySpec::BASE_SHIFT_MAX` | 24 | a series summary's base node, as a power of 2 |
| `SERIES_CHANNELS_MAX` | 65535 | a series track's channels |
| `GRID_FIXED_AXES_MAX` | 31 | a grid's fixed axes, and a grid chunk's tile coordinates |
| `AXIS_NAME_LEN_MAX` | 64 | a fixed axis' name bytes |
| `GRID_LISTED_MAX` | 65536 | the listed coordinates of a fixed axis |
| `GRID_TILE_CELLS_MAX` | 64 Mi | the cells of one time step of a grid's tile |
| `GRID_OVERVIEW_LEVELS_MAX` | 16 | a grid chunk's overview level |
| `TILE_NUMBER_BITS` | 128 | the bits of a grid's tile number fields together |
| `GAP_MAX_MAX` | 64 MiB | a layout's gap |
| `GAP_PROBES_MAX` | 8192 | the resume places a reader probes in one gap |
| `GAP_CHECKS_MAX` | 4 | the blocks a reader checks whole past one gap |
| `STATE_READ_MAX` | 1 GiB | the bytes of blocks one state lookup reads |
| `LAYOUT_READ_MAX` | 256 MiB | the bytes of definitions blocks a reader reads to find a piece's layout |

Walks over many blocks and the writer have caps of their own. A walk stops at its cap, and a writer refuses input that
would pass one:

| Walk or writer cap | Value | Bounds |
|---|---|---|
| `RESYNC_AHEAD_MAX` | 128 MiB | the bytes a salvage walk (`Recovery::Resync`) reads ahead |
| `RESYNC_SEQS_MAX` | 65536 | the data blocks a salvage walk remembers by session and seq, for conflicts |
| `DEFERRED_BYTES_MAX` | 1 MiB | the deferred blocks a writer keeps before it writes them out |
| `UNIT_CHUNKS_MAX` | 17 | the chunks of one unit a writer writes or relays: a tile chunk and its overviews |
| `RUN_BLOCKS_MAX` | 4096 | the blocks of one run marked `FLAG_WITH_NEXT` and the block it belongs with, a writer writes or a follower holds |
| `VERIFY_LISTS_MAX` | 466432 | the track lists of a final index one verify takes |
| `TRACKS_PER_PIECE_MAX` | 466430 | the tracks with data in one piece, so its close fits a block |
| `HEAD_TRACKS_MAX` | 699006 | the tracks a piece head carries over the chain, so the head fits a block |
| `CLOSE_SUMMARY_MAX` | 1 MiB | the body of the summary block a close names |
| `REMOVED_PIECES_MAX` | 65536 | the removed pieces one close names |
| `WALK_MISS_BASE_MAX` | 1 MiB | the bytes a content-hash walk's missed blocks claim at its start |
| `WALK_MISS_PER_BYTE_MAX` | 4 | the bytes they claim per byte walked, past that |
| `RESUME_LOOK_MAX` | 128 MiB | the bytes past a reader's stop a resume looks through for a good block |
| `RESUME_PROBES_MAX` | 4 | the places there a resume reads whole |

A cut does not lift the two limits on tracks. A block or chunk that would bring a piece past `TRACKS_PER_PIECE_MAX`
tracks with data is refused with `InputError::TooManyTracks`, and the piece must be cut first. A next piece head that
would carry more than `HEAD_TRACKS_MAX` tracks is refused with `InputError::TooManyCarried`, and a new chain takes the
track. Nothing is written in either case.
