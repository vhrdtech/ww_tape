---
level: basic
---

# Series

A **series** is a track of samples at an exact period. Examples are an analog-to-digital converter (ADC) channel, an
audio stream or a motor's phase currents.

A series stores no time per sample. Its samples go into series chunks ([Events and
series](events_and_series.md#chunks)).

This page explains what a sample is, how to store its numbers, and how a reader finds each sample's time. It assumes
[Events and series](events_and_series.md).

The track's definition (`SeriesDef`) says what a sample is:

- the type of one value as the API declares it (`sample`), such as an exact_units `Fixed<Ampere, Milli, i32>`, a `ww_si`
  quantity or a float;
- the storage element (`I16`, `F32`, `ComplexI16` and so on) and the channel count, derived from that type (`shape`);
- the period as an exact fraction of ticks per sample (`period`): 48 kHz on a 1 ns clock is `62500/3`;
- the scale of the stored numbers at the start (`scale`);
- the summaries the writer builds (`summaries`);
- for samples kept in a media file, the stream they are in (`media`).

## Fixed point first

For data a device controls, store fixed-point integers. An exact_units `Fixed<Ampere, Milli, i32>` is an `i32` on the
wire and an `i32` in the series column.

The scale, 10⁻³ A per step, lives once in the track, not in every sample. The numbers stay integers from the device to
the file, and nothing passes through a float.

The writer codes integer columns with the codec `DELTA`. Per group of 128 samples, it stores the differences between
neighbours minus their smallest difference, bit-packed.

A ramp or a constant packs to zero bits per sample. The codec `DELTA2` codes the differences of differences the same
way, which suits signals with a steady slope.

Every group restarts. Any range of samples therefore decodes from its group's start.

### Floats a device didn't choose

The writer codes floats that a device didn't choose with Adaptive Lossless floating-Point compression (**ALP**).
Examples are a sensor that only reports `f32` and a value from another program.

ALP finds floats that began as decimals, such as an ADC count times 0.1. It stores the integers they came from,
bit-packed per group of 128.

Floats with full mantissas take ALP's second scheme, which codes their front bits through a small dictionary. Values
that don't come back bit for bit, such as NaN, infinities and `-0.0`, are kept whole as exceptions.

The encoder never lets a group grow by more than 7 bytes over its raw size. The layout is in the [ALP
docs](https://github.com/vhrdtech/ww_tape/blob/main/ww_tape_codecs/src/samples/alp/mod.rs).

### Why fixed point wins

Fixed point beats every float codec on every data set `just bench-sample-codecs` measures. It is 1.6 to 9.3 times
smaller than the best float codec on each set, and it decodes fastest.

ALP rediscovers the integers only when the scale is a short decimal: 0.1 is one, 3.3/4096 is not. A fixed-point source
hands the writer those integers directly.

Store the ADC counts with the scale in the type, and no float codec is needed.
[Numbers](numbers.md#fixed-point-against-floats) gives the measurements.

## Raw device formats

Some devices send samples in their own packing: two 12-bit values in three bytes, 24-bit values or big-endian words. A
series column may hold them exactly as sent.

The format has codec ids for big-endian and for 10-, 12- and 24-bit packing in either bit order. A reader converts only
the samples it reads, and any sample reads in place.

The recording path then copies the device's bytes and does no arithmetic. The table of sample codecs is in the [samples
module docs](https://github.com/vhrdtech/ww_tape/blob/main/ww_tape_codecs/src/samples/mod.rs).

## Grids: where samples are in time

A series track has no records. Instead, each series chunk's head carries the **grid** its samples are on.

A grid (`SeriesGrid`) holds three values:

- a sample index at or before the chunk's first sample (`grid_index`);
- that sample's tick (`grid_tick`);
- the scale of the chunk's stored numbers.

Sample `i` sits at `grid_tick + (i - grid_index) · period`, rounded down, with the period from the track's definition. A
chunk thus times and scales its samples alone: nothing else has to be found, linked or kept near it.

Consecutive chunks of one stretch of recording carry the same grid. No sample time moves by rounding, however the
samples are cut into chunks.

A change of scale keeps the grid and gives it the new scale. A chunk has one grid and one scale, and its head's first
and last tick are its first and last sample's ticks on that grid.

### Gaps and lost samples

A **gap** is a chunk on another grid than the chunk before it.

A gap or a restart on the same clock starts a new grid. The new grid starts more than a period, rounded up, after the
last tick of the chunk before.

No sample lies between the two chunks, and a viewer draws no line across. A device restart that is a new clock is a new
track on that clock.

Sample indexes missing between two chunks of one grid are **lost samples**: a chunk that is damaged or in another piece.
Their ticks are known, but their values are not.

The chunks are data, not state: each is written once and never repeated. A series track therefore takes the plain policy
(`TrackRepeat::Stream`) like any data.

The index lists each chunk's first sample and first tick. A reader therefore finds the chunk of a sample or of a time
without reading any head.

The figure shows a 48 kHz series on a 1 ns clock, in chunks of six samples. The first four chunks share grid 1.

The third chunk is lost, yet its ticks are known. The fourth holds milliamperes after a prefix change and keeps the
grid.

The fifth chunk starts grid 2 more than a period after the fourth: a gap.

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/grids.svg"
<figcaption>Grids of a 48 kHz series on a 1 ns clock: lost samples, a prefix change and a gap. Drawn by
<code>ww_tape_figures</code>; the ticks are <code>SeriesTimeline</code>'s.</figcaption>
</figure>

### A track's timeline

A track's grids form its **timeline** (`SeriesTimeline`), built from its chunk heads alone.

`TapeReader::series_timeline(i, track_id, period)` reads each chunk's head with `TapeReader::read_chunk_head`. It reads
the header, then the bytes up to the payload, and none of the samples. The head cyclic redundancy check (CRC) checks
those bytes.

Some chunks are left out and count as lost. The reader returns each one's error beside the timeline:

- a chunk whose head is damaged;
- a chunk out of line with its neighbours, such as a new grid too close to the chunk before or samples on two grids.

The writer checks each chunk against its own grid only. The rules between neighbouring chunks are checked when the
timeline is built.

### What a head read costs

In a file whose chunk payloads start on 4 KiB boundaries for direct I/O, the head reaches up to the alignment boundary.
A head read therefore takes about one 4 KiB block.

`just bench-chunks` measures a track of 256 chunks of 1 MiB, read in memory. The bytes are exact.

The times come from two runs on an Advanced Micro Devices (AMD) `EPYC Genoa` virtual machine with 14 vCPUs. The load
averages were 54 and 58, and therefore the times are high and vary between runs:

| Alignment | Read | Bytes read per chunk | ns per chunk |
|---|---|---:|---:|
| none | heads alone | 335 | 637 to 827 |
| none | every whole block | 1 048 895 | 105 331 to 121 345 |
| 4096 | heads alone | 4 084 | 3 485 to 3 761 |
| 4096 | every whole block | 1 052 644 | 120 175 to 318 427 |

A whole block costs its read and its CRC checks, while a head read never touches the samples. The head grows only by its
segment CRCs, 4 bytes per 64 KiB of payload, and therefore its cost hardly grows with the chunk.

## Example: an autoranging channel changes prefix

A current meter reports a `ww_si` quantity whose prefix follows its range. It records 1.234 A as 1234 with prefix milli.

At sample 10 000 it switches to its micro range and reports 980 µA as 980 with prefix micro. On the wire, the `Series`
batch restarts its numeric column at the item where the prefix changed.

The writer does the same in the file. It seals the open chunk after sample 9 999.

The next chunk carries the same grid as before at scale 10⁻⁶, and therefore its integers mean microamperes.
Sample 10 000's tick is the one the grid gives it: no time moves.

A [summary](summaries.md) node that covers samples 8192 to 12 287 holds samples of both scales. It is kept at their
**common scale**, the largest scale both are whole multiples of: 10⁻⁶ here.

Every milliampere number in the node is multiplied by 1000, and 1234 becomes 1 234 000. The node's minimum and maximum
therefore stay exact integers.

Each summary block states its scale, and therefore it reads without the chunks. Two scales without a common scale, whose
ratio isn't a fraction of integers, are refused, never mixed.

## Wire batches and file columns

A device sends samples to the host in `Series<T>` batches, and the file stores them in sample columns. The two layers
code the same numbers for different readers:

| | `Series<T>` on the wire | Sample columns in a tape |
|---|---|---|
| Size | a batch of about 32 items | a chunk of up to megabytes |
| Read | from the start, item by item | any sample range, by group, in place |
| Encoder | sized for a microcontroller | sized for a host, with SIMD kernels |
| Scale | a prefix per item or in the type | one per chunk |

Neither layer contains the other. The host decodes a batch once and codes its numbers again for the file.

The integers pass through unchanged, and a `ww_si` prefix change ends the chunk where the batch restarts its column. The
batch's time column is not stored, because the grid gives every sample's tick.

The host starts a new grid where the device's samples leave the old one.

## Series files

A file that holds series is usually **aligned**. Every chunk's sample payload then starts on a 4 KiB boundary, which is
a memory page and a direct I/O block.

The writer aligns a file when asked (`Layout::ALIGNED`, or `TapeWriter::set_alignment`), and the file's layout
definition states which it is. The chunk head sits just before its payload.

Small blocks waiting for a place, such as summaries and checkpoints, fill the gap before the head. Alignment therefore
costs little space.

Event files are not aligned. [Writing a tape](writing.md#aligned-files) shows how to set it up.

Every chunk carries a CRC-32C per 64 KiB of payload, a **CRC segment**. A flipped bit in a 4 MiB chunk therefore costs
one 64 KiB segment, not the chunk.

[Damage and recovery](recovery.md) continues from there.

## See also

- [`Series<T>`](https://ww.vhrd.tech/serdes/series/): the wire format of the sample batches a device sends.
