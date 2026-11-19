# Events and series

Records blocks store each item with its own time delta and length, which suits a slow trickle of items. A track with
millions of items needs another form: one that a reader searches and decodes without walking the items before the one it
wants.

`ww_tape` stores such tracks in **chunks**, one track per chunk. This page explains the two kinds of chunk, and how
event times and values are stored.

[Series](series.md) continues with series and their grids, and [Summaries](summaries.md) with how a viewer draws a long
track without reading it. This page assumes the [overview](index.md).

## Chunks

A **chunk** is a block that holds a run of consecutive items of one track. There are two kinds:

- An **events chunk** holds up to a few thousand items of an event track. Each item has a start tick of its own.
- A **series chunk** holds samples of a fixed-rate series. Samples have no time of their own.

An item of an events chunk is an **instant**, or a **span** with an end. It may also carry **flags**, a 32-bit word, and
a **payload**, its WireWeaver value in shrink_wrap bytes.

### The chunk head

The chunk's head places the chunk without any other block. It names the track by its identity hash and number, and gives
these values:

- the first item number and the item count;
- the first and last tick;
- the latest end of any item.

A reader that lost the definitions and the index around a chunk therefore still knows the chunk's track. It also knows
which items and times the chunk holds.

An overlap query asks "which spans cover tick t?". It uses the latest end without decoding the ends.

### Columns and codecs

The **columns** follow the head. An events chunk has columns for starts, ends, flags, payload lengths and payloads; a
series chunk has a column of sample values.

Each column names its kind and its **codec**, the rule its bytes follow. Two chunks of one track can therefore use
different codecs.

A reader that meets a column kind or codec it doesn't know refuses that column. It still reads the other columns.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/events_series.svg"
<figcaption>The two kinds of chunk. An events chunk stores a start per item, beside optional ends, flags and payloads;
a series chunk stores only samples, and its head's grid gives each sample's tick. Both end their payload with 16 zero
bytes and the block CRC.</figcaption>
</figure>

A reader reads columns in place. All numbers are little-endian and need no alignment.

The payload ends with 16 zero bytes, and therefore a 16-byte load at any column byte stays inside the block. The bytes a
reader holds in memory are the bytes in the file: there is no decoded copy to build or cache.

The layout is in the [chunk module docs](https://github.com/vhrdtech/ww_tape/blob/main/ww_tape/src/chunk/mod.rs).

## Event times

An events chunk stores its start ticks in one of two **time modes**:

- **Implicit.** Item `j` starts at `first tick + j × period`, and the column holds only the period. A chunk whose items
  are evenly spaced takes this mode, and so does a chunk of one item.
- **Linear.** The items are cut into **groups** of 128 items. Each item is stored as its distance from a line through
  its group, its **residual**, bit-packed at the group's width.

In linear mode, each group stores a 25-byte record before the residuals. The record holds the group's first tick, a
slope, the smallest residual, a bit width and an offset.

The slope makes periodic data cheap. A 1 kHz event with a little jitter lies close to a line through its group's first
and last item.

Each residual therefore needs only the bits of its jitter, not the bits of the 1 ms period. The encoder keeps the slope
only when it gives a narrower width than no slope, and therefore data without a trend loses nothing.

The encoder uses integers only. The same starts therefore always give the same bytes.

### Reading times in place

A read needs no decode. Item `j` costs one group record and one load.

A search for the first item at or after a tick bisects the groups' first ticks. It then uses the slope to narrow the
items inside the group to the few that the residual width allows.

Whole groups decode with hand-written kernels, chosen at run time. There are kernels for AVX-512, Advanced Vector
Extensions 2 (AVX2) and NEON (the Arm vector extension).

A scalar path beside the kernels gives the same values. The layout is in the [time codec
docs](https://github.com/vhrdtech/ww_tape/blob/main/ww_tape/src/times/mod.rs), and the bit packing in the [bitpack
docs](https://github.com/vhrdtech/ww_tape/blob/main/ww_tape_codecs/src/bitpack/mod.rs).

### What it costs

A bench of an application that records with tapes compares the linear codec with two alternatives. That bench is not
part of this repository.

The bench codes 10 million events per workload in chunks of 4096 items. It runs on a virtual machine with 14 vCPUs of an
Advanced Micro Devices (AMD) `EPYC Genoa` host.

The table gives bytes per event, group records and chunk overhead included. Raw 64-bit ticks take 8.01 bytes per event.

| Workload | Linear groups | Delta-of-delta varints | Elias-Fano |
|---|---:|---:|---:|
| 1 ms period, ±300 ns jitter, 1 ns ticks | 1.48 | 1.88 | 2.79 |
| exact 1 ms period with a few gaps | 0.022 | 0.99 | 2.79 |
| bursty traffic, 1 ns ticks | 3.84 | 2.43 | 3.16 |
| a real system journal, 6.4 M entries, 1 µs ticks | 2.97 | 2.76 | 2.51 |

The bench stores the residual minimum in 8 bytes, a 29-byte group record against the format's 25 bytes. The sizes in the
linear column are therefore an upper bound.

On the same machine, fetching one item by number takes 2 to 25 ns. Finding the first item at or after a tick takes 15 to
220 ns.

That is faster than a binary search over raw ticks of the same items (244 to 280 ns), because the packed track is
smaller. [Numbers](numbers.md#event-times) gives the times per workload.

Bursty nanosecond traffic is the one case where the linear codec loses: varints take 2.43 bytes per event against 3.84.
Varints can't be read in place, though.

With varints, fetching one item costs 190 to 310 ns on the same machine, because the reader must walk to it. On such
data the format pays about 1.4 bytes per event for random access.

A frame clock shows why a clock should match its source. One million video frames at 60000/1001 frames per second cost
0.010 bytes per frame on the stream's own 60 000 Hz clock.

On that clock, the frames form an implicit chunk with period 1001.

On a 1 ns clock the frames cost 0.36 bytes per frame, because rounding leaves a residual of 0 or 1 ns per frame.

### Ends and flags

An instant has no end, and a span has one. Each chunk picks one of three **end modes**:

- **Absent:** every item is an instant, and no column is stored.
- **Next start:** each item ends where the next begins, as in back-to-back states. No column is stored, and the last
  item ends at the chunk head's latest end.
- **Durations:** each item's `end - start`, packed per group of 128 above the group's smallest duration. The smallest
  duration plus the largest value the width holds bounds the group's longest item.

With durations, an overlap query therefore knows how far back to look.

On the same bench, back-to-back spans cost 2.98 bytes per event for starts and ends together, against 16.01 raw. Spans
with gaps between them cost 5.97 bytes.

Flags are optional, one 32-bit word per item, with the severity in its low bits. The writer packs them per group beside
the bitwise OR of the group's flags.

A zoomed-out view therefore colours a range by the ORs without decoding a single item. A group without flags costs its
record only, and a chunk where every item's flags are 0 has no flags column.

### Payloads

An events chunk stores the payloads in two columns:

- **Lengths:** packed per group of 128 above the group's shortest payload. Each group record says where the group's
  first payload starts; item `i`'s payload is found from one record and one group of lengths.
- **Payload bytes:** back to back. They are stored raw and read in place, or coded with lz4 when that is smaller and
  then decoded once when the chunk is opened.

The layout is in the [payload codec docs](https://github.com/vhrdtech/ww_tape/blob/main/ww_tape/src/payloads/mod.rs).
