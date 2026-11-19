# Events and series

Record blocks store each item with its own time delta and length, which suits a slow trickle of items. A track with
millions of items needs another form: one that a reader can search and decode without walking every item before the
one it wants. `ww_tape` stores such tracks in **chunks**, one track per chunk, coded column by column. This page
explains the two kinds of chunk, how they store time, and the summaries that let a viewer draw a long track without
reading it. It assumes the [overview](index.md).

## Chunks

A **chunk** is a block that holds a run of consecutive items of one track. There are two kinds:

- An **events chunk** holds up to a few thousand items of an event track. Each item has a start tick and optionally an
  end, a flags word and a payload.
- A **series chunk** holds samples of a fixed-rate series. Samples have no time of their own.

The chunk's head places the chunk without any other block. It names the track by its identity hash and number, and
gives the first item number, the item count, the first and last tick, and the latest end of any item. A reader that
lost the definitions and the index around a chunk therefore still knows which track the chunk belongs to and which
items and times it holds. An overlap query ("which spans cover tick t?") uses the latest end without decoding the
ends.

The **columns** follow the head: starts, ends, flags, payload lengths and payloads for events, or sample values for a
series. Each column names its kind and its **codec**, the rule its bytes follow. Two chunks of one track can so use
different codecs. A reader that meets a column kind or codec it doesn't know refuses that column and still reads the
others.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/events_series.svg"
<figcaption>The two kinds of chunk. An events chunk stores a start per item, beside optional ends, flags and payloads;
a series chunk stores only samples, and its head's grid gives each sample's tick. Both end their payload with 16 zero
bytes and the block CRC.</figcaption>
</figure>

Columns are read in place. All numbers are little-endian and need no alignment. The payload ends with 16 zero bytes,
so a 16-byte load at any column byte stays inside the block. The bytes a reader holds in memory are the bytes in the
file, so there is no decoded copy to build or cache. The layout is in the [chunk module
docs](https://github.com/vhrdtech/ww_tape/blob/main/ww_tape/src/chunk/mod.rs).

## Event times

An events chunk stores its start ticks in one of two **time modes**:

- **Implicit.** Item `j` starts at `first tick + j × period`, and the column holds only the period. A chunk whose items
  are evenly spaced takes this mode, and so does a chunk of one item.
- **Linear.** The items are cut into **groups** of 128. Each group stores a 25-byte record: its first tick, a slope,
  the smallest residual, a bit width and an offset. Then come each item's distances from the line the slope draws,
  the **residuals**, bit-packed at the group's width.

The slope makes periodic data cheap. A 1 kHz event with a little jitter lies close to a line through its group's first
and last item. Each residual so needs only the bits of its jitter, not the bits of the 1 ms period. The encoder keeps
the slope only when it gives a narrower width than no slope, so data without a trend loses nothing. The encoder uses
integers only, so the same starts always give the same bytes.

A read needs no decode. Item `j` costs one group record and one load. A search for the first item at or after a tick
bisects the groups' first ticks. It then uses the slope to narrow the items inside the group to the few that the
residual width allows. Whole groups decode with hand-written kernels for AVX-512, AVX2 and NEON, chosen at run time,
beside a scalar path that gives the same values. The layout is in the [time codec
docs](https://github.com/vhrdtech/ww_tape/blob/main/ww_tape/src/times/mod.rs), and the bit packing in the [bitpack
docs](https://github.com/vhrdtech/ww_tape/blob/main/ww_tape_codecs/src/bitpack/mod.rs).

### What it costs

The bench `eventfull_benchmarks` of the eventfull project compares the linear codec with two alternatives. It codes 10
million events per workload in chunks of 4096 items, on a virtual machine with 14 vCPUs of an AMD EPYC Genoa host. The
table gives bytes per event, group records and chunk overhead included. Raw 64-bit ticks take 8.01 bytes per event.

| Workload | Linear groups | Delta-of-delta varints | Elias-Fano |
|---|---:|---:|---:|
| 1 ms period, ±300 ns jitter, 1 ns ticks | 1.48 | 1.88 | 2.79 |
| exact 1 ms period with a few gaps | 0.022 | 0.99 | 2.79 |
| bursty traffic, 1 ns ticks | 3.84 | 2.43 | 3.16 |
| a real system journal, 6.4 M entries, 1 µs ticks | 2.97 | 2.76 | 2.51 |

The bench stores the residual minimum in 8 bytes, a 29-byte group record against the format's 25. The sizes in the
linear column are therefore an upper bound. On the same machine, fetching one item by number takes 2 to 25 ns, and
finding the first item at or after a tick takes 15 to 220 ns. That is faster than a binary search over raw ticks of
the same items (244 to 280 ns), because the packed track is smaller. [Numbers](numbers.md#event-times) gives the times
per workload.

Bursty nanosecond traffic is the one case where the linear codec loses: varints take 2.43 bytes per event against
3.84. Varints can't be read in place, though. Fetching one item costs 190 to 310 ns on the same machine, because the
reader must walk to it. On such data the format pays about 1.4 bytes per event for random access.

A frame clock shows why a clock should match its source. One million video frames at 60000/1001 frames per second cost
0.010 bytes per frame on the stream's own 60 000 Hz clock, an implicit chunk with period 1001. On a 1 ns clock they
cost 0.36 bytes per frame, because rounding leaves a residual of 0 or 1 ns per frame.

### Ends and flags

An event is either an **instant** or a **span** with an end. Each chunk picks one of three **end modes**:

- **Absent:** every item is an instant, and no column is stored.
- **Next start:** each item ends where the next begins, as in back-to-back states. No column is stored, and the last
  item ends at the chunk head's latest end.
- **Durations:** each item's `end - start`, packed per group of 128 above the group's smallest duration. The smallest
  duration plus the largest value the width holds bounds the group's longest item, so an overlap query knows how far
  back to look.

On the same bench, back-to-back spans cost 2.98 bytes per event for starts and ends together, against 16.01 raw.
Spans with gaps between them cost 5.97 bytes.

**Flags** are an optional 32-bit word per item, with the severity in its low bits. They are packed per group beside the
OR of the group's flags. A zoomed-out view so colours a range by the ORs without decoding a single item. A group
without flags costs its record only, and a chunk where every item's flags are 0 has no flags column.

### Payloads

An item's **payload** is its WireWeaver value in shrink_wrap bytes. An events chunk stores the payloads in two columns:

- The **lengths**, packed per group of 128 above the group's shortest payload. Each group record says where the
  group's first payload starts, so item `i`'s payload is found from one record and one group of lengths.
- The **payload bytes**, back to back. They are stored raw and read in place, or coded with lz4 when that is smaller
  and then decoded once when the chunk is opened.

The layout is in the [payload codec docs](https://github.com/vhrdtech/ww_tape/blob/main/ww_tape/src/payloads/mod.rs).

## Series

A **series** is a track of samples at an exact period, such as an ADC channel, an audio stream or a motor's phase
currents. It stores no time per sample. The track's definition (`SeriesDef`) says what a sample is:

- the type of one value as the API declares it (`sample`), such as an exact_units `Fixed<Ampere, Milli, i32>`, a
  `ww_si` quantity or a float;
- the storage element (`I16`, `F32`, `ComplexI16` and so on) and the channel count, derived from that type (`shape`);
- the period as an exact fraction of ticks per sample (`period`): 48 kHz on a 1 ns clock is `62500/3`;
- the scale of the stored numbers at the start (`scale`);
- the summaries the writer builds (`summaries`);
- for samples kept in a media file, the stream they are in (`media`).

### Fixed point first

For data a device controls, store fixed-point integers. An exact_units `Fixed<Ampere, Milli, i32>` is an `i32` on the
wire and an `i32` in the series column. The scale, 10⁻³ A per step, lives once in the track, not in every sample. The
numbers stay integers from the device to the file, and nothing passes through a float.

Integer columns are coded with **DELTA**. Per group of 128 samples, it stores the differences between neighbours minus
their smallest difference, bit-packed. A ramp or a constant packs to zero bits per sample. **DELTA2** codes the
differences of differences the same way, which suits signals with a steady slope. Every group restarts, so any range
of samples decodes from its group's start.

Floats that a device didn't choose are coded with **ALP**. Examples are a sensor that only reports `f32` and a value
from another program. ALP finds floats that began as decimals, such as an ADC count times 0.1, and stores the integers
they came from, bit-packed per group of 128. Floats with full mantissas take ALP's second scheme, which codes their
front bits through a small dictionary. Values that don't come back bit for bit, such as NaN, infinities and `-0.0`,
are kept whole as exceptions. The encoder never lets a group grow by more than 7 bytes over its raw size. The layout
is in the [ALP docs](https://github.com/vhrdtech/ww_tape/blob/main/ww_tape_codecs/src/samples/alp/mod.rs).

Fixed point beats every float codec on every data set `just bench-sample-codecs` measures. It is 1.6 to 9.3 times
smaller than the best float codec on each set, and it decodes fastest. ALP rediscovers the integers only when the
scale is a short decimal: 0.1 is one, 3.3/4096 is not. A fixed-point source hands the writer those integers directly.
Store the ADC counts with the scale in the type, and no float codec is needed.
[Numbers](numbers.md#fixed-point-against-floats) gives the measurements.

### Raw device formats

Some devices send samples in their own packing: two 12-bit values in three bytes, 24-bit values or big-endian words. A
series column may hold them exactly as sent. The format has codec ids for big-endian and for 10-, 12- and 24-bit
packing in either bit order. A reader converts only the samples it reads, and any sample reads in place. The recording
path then copies the device's bytes and does no arithmetic. The table of sample codecs is in the [samples module
docs](https://github.com/vhrdtech/ww_tape/blob/main/ww_tape_codecs/src/samples/mod.rs).

### Grids: where samples are in time

A series track has no records. Instead, each series chunk's head carries the **grid** its samples are on. A grid
(`SeriesGrid`) is a sample index at or before the chunk's first sample (`grid_index`), that sample's tick
(`grid_tick`), and the scale of the chunk's stored numbers. Sample `i` sits at `grid_tick + (i - grid_index) · period`,
rounded down, with the period from the track's definition. A chunk thus times and scales its samples alone: nothing
else has to be found, linked or kept near it.

Consecutive chunks of one stretch of recording carry the same grid. No sample time moves by rounding, however the
samples are cut into chunks. A change of scale keeps the grid and gives it the new scale. A chunk has one grid and one
scale, and its head's first and last tick are its first and last sample's ticks on that grid.

A **gap** is a chunk on another grid than the chunk before it. A gap or a restart on the same clock starts a new grid
more than a period, rounded up, after the last tick of the chunk before. No sample lies between the two chunks, and a
viewer draws no line across. A device restart that is a new clock is a new track on that clock.

Sample indexes missing between two chunks of one grid are **lost samples**: a chunk that is damaged or in another
piece. Their ticks are known, but their values are not. The chunks are data, not state: each is written once and never
repeated, so a series track takes the plain policy (`TrackRepeat::Stream`) like any data. The index lists each chunk's
first sample and first tick, so a reader finds the chunk of a sample or of a time without reading any head.

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/grids.svg"
<figcaption>A 48 kHz series on a 1 ns clock, in chunks of six samples. The first four chunks share grid 1: the third is
lost, yet its ticks are known; the fourth holds milliamperes after a prefix change and keeps the grid. The fifth
starts grid 2 more than a period after the fourth, a gap. Drawn by <code>ww_tape_figures</code>; the ticks are
<code>SeriesTimeline</code>'s.</figcaption>
</figure>

A track's grids form its **timeline** (`SeriesTimeline`), built from its chunk heads alone.
`TapeReader::series_timeline(i, track_id, period)` reads each chunk's head with `TapeReader::read_chunk_head`: its
header, then its bytes up to the payload, checked by the head CRC, and none of its samples. Some chunks are left out,
each with its error returned beside the timeline, and count as lost:

- a chunk whose head is damaged;
- a chunk out of line with its neighbours, such as a new grid too close to the chunk before or samples on two grids.

The writer checks each chunk against its own grid only. The rules between neighbouring chunks are checked when the
timeline is built. In a file aligned for direct I/O, the head reaches up to the alignment boundary, so a head read
takes about one aligned block. `just bench-chunks` measures a track of 256 chunks of 1 MiB, read in memory, on an AMD
EPYC Genoa virtual machine with 14 vCPUs at a load average of about 58, so the times are high:

| Alignment | Read | Bytes read per chunk | ns per chunk |
|---|---|---:|---:|
| none | heads alone | 335 | 827 |
| none | every whole block | 1 048 895 | 121 345 |
| 4096 | heads alone | 4 084 | 3 761 |
| 4096 | every whole block | 1 052 644 | 318 427 |

A whole block costs its read and its CRC checks, while a head read never touches the samples. The head grows only by
its segment CRCs, 4 bytes per 64 KiB of payload, so its cost hardly grows with the chunk.

### Example: an autoranging channel changes prefix

A current meter reports a `ww_si` quantity whose prefix follows its range. It records 1.234 A as 1234 with prefix
milli. At sample 10 000 it switches to its micro range and reports 980 µA as 980 with prefix micro. On the wire, the
`Series` batch restarts its numeric column at the item where the prefix changed.

The writer does the same in the file. It seals the open chunk after sample 9 999. The next chunk carries the same grid
as before at scale 10⁻⁶, so its integers mean microamperes. Sample 10 000's tick is the one the grid gives it, so no
time moves.

A summary node that covers samples 8192 to 12 287 holds samples of both scales. It is kept at their **common scale**,
the largest scale both are whole multiples of: 10⁻⁶ here. Every milliampere number in the node is multiplied by 1000,
so 1234 becomes 1 234 000, and the node's minimum and maximum stay exact integers. Each summary block states its scale,
so it reads without the chunks. Two scales without a common scale, whose ratio isn't a fraction of integers, are
refused, never mixed.

### Wire batches and file columns

A device sends samples to the host in `Series<T>` batches, and the file stores them in sample columns. The two layers
code the same numbers for different readers:

| | `Series<T>` on the wire | Sample columns in a tape |
|---|---|---|
| Size | a batch of about 32 items | a chunk of up to megabytes |
| Read | from the start, item by item | any sample range, by group, in place |
| Encoder | sized for a microcontroller | sized for a host, with SIMD kernels |
| Scale | a prefix per item or in the type | one per chunk |

Neither layer contains the other. The host decodes a batch once and codes its numbers again for the file. The integers
pass through unchanged, and a `ww_si` prefix change ends the chunk where the batch restarts its column. The batch's
time column is not stored, because the grid gives every sample's tick. The host starts a new grid where the device's
samples leave the old one.

### Series files

A file that holds series is usually **aligned**: every chunk's sample payload starts on a 4 KiB boundary, which is a
memory page and a direct I/O block. The writer aligns a file when asked (`Layout::ALIGNED`, or
`TapeWriter::set_alignment`), and the file's layout definition states which it is. The chunk head sits just before its
payload. Small blocks waiting for a place, such as summaries and checkpoints, fill the gap before the head, so
alignment costs little space. Event files are not aligned. [Writing a tape](writing.md#aligned-files) shows how to set
it up.

Every chunk carries a CRC-32C per 64 KiB of payload, a **CRC segment**. A flipped bit in a 4 MiB chunk so costs one
64 KiB segment, not the chunk. [Damage and recovery](recovery.md) continues from there.

## Summaries

A viewer that draws a track of a billion items over 2000 pixels must not read a billion items. **Summary blocks** hold
a coarse view of a track, built as the track is written, in a tree over item numbers. A **node** of the tree covers a
range of items:

- **Event trees.** A level-0 node covers 2048 items, and a node one level up covers 16 nodes of the level below:
  32 768 items, then 524 288, then 8.4 million. A node holds the first item's start, the last item's start, the OR of
  the items' flags, and for spans the latest end. Counts follow from the item range.
- **Series summaries.** A level-0 node covers a power of two of samples that the track chooses, and each level up
  again covers 16 nodes. A node holds, per channel, the statistics the track declares (`SeriesSummarySpec`): minimum,
  maximum, sum, sum of squares, a count of clipped or NaN samples, a flags OR, and for complex samples the mean and
  maximum of `I² + Q²`. It holds no ticks, since the chunks' grids give them.

Nodes run over item numbers, not time. A sparse track therefore has no empty nodes, and the nodes of consecutive pieces
of one track line up. Every statistic merges: a parent is its 16 children merged in item order. A reader that builds
coarser bins of its own gets exactly what the writer would have stored.

For example, a track of 1 billion events has about 1900 nodes at level 2, roughly one per pixel of a 2000-pixel row.
An overview reads those nodes and no items. Zoomed in, a viewer reads the nodes of the visible range, then the chunks
they point to.

Two crates share the work. The crate `ww_tape_summary` builds the trees as the items arrive (`EventTree`,
`SeriesSummaries`) and reads the nodes of a summary block (`Summary::of_block`). The application hands the blocks it
builds to `TapeWriter::defer_block`. `ww_tape` itself reads and checks only a summary block's head
(`Block::summary_head`), which is enough to list the block in the index.

Summary blocks are small, a few KiB each, and the index lists them like any block. A lost summary block costs nothing
that can't be rebuilt from the data. The block layout is in the [summary module
docs](https://github.com/vhrdtech/ww_tape/blob/main/ww_tape/src/summary/mod.rs), and the nodes in the [ww_tape_summary
crate docs](https://github.com/vhrdtech/ww_tape/blob/main/ww_tape_summary/src/lib.rs).

## See also

- [`Series<T>`](https://ww.vhrd.tech/serdes/series/): the wire format of the sample batches a device sends.
