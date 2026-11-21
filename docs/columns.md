---
level: advanced
---

# Chunks from columns

A [[chunk]] keeps many [[items]] of one [[track]] field by field, and the [[writer]] takes those fields already encoded.
How does an application encode its times, [[payloads]] and [[samples]], and which [[codec]] suits each? The examples
encode a second of the drive: 1000 CAN frames and 25 000 current samples.

## A chunk is a head and its columns

A [[chunk]] holds a run of consecutive [[items]] of one [[track]], one [[column]] per field. The head places the chunk
alone. It names the track, the first [[item number]] and the count, the first and last [[tick]], and the latest end of
any item. [Blocks](blocks.md#chunks) shows what that buys a [[reader]].

Each column names its kind and its [[codec]], the rule its bytes follow. Two chunks of one track can therefore use
different codecs, and a writer may try several on a chunk and keep the smallest. A reader that meets a codec it does not
know refuses that column and still reads the others. [Sample codecs](format.md#sample-codecs) lists the codecs of
samples by number.

A reader reads columns in place. All numbers are little-endian and need no alignment. The payload ends with 16 zero
bytes, so a 16-byte load at any column byte stays inside the block.

## Frames into columns

An [[events chunk]] of the drive holds a second of `can0`: each frame's start tick and its bytes. The recorder encodes
the starts with [`EncodedTimes::encode`] and the frames with [`EncodedPayloads::encode`]. The encoded times also make
the chunk head, since they know the first and last tick:

```rs
--8<-- "ww_tape_example/src/record.rs:frames"
```

??? example "What the example uses here: `the clock and track numbers`, `key`, `Act`"

    ```rs
    --8<-- "ww_tape_example/src/defs.rs:ids_item"

    --8<-- "ww_tape_example/src/defs.rs:key_item"

    --8<-- "ww_tape_example/src/record.rs:act_item"
    ```

The two `None`s say that the frames have no ends and no flags. A track of spans passes the ends, and one with a severity
per item passes the flags. The recorder hands the head and the columns to [`TapeWriter::write_chunk`] unchanged.

## Event times

An events chunk stores its start ticks in one of two time modes. In the **implicit** mode, item `j` starts at the first
tick plus `j` periods, and the column holds only the period. A chunk of evenly spaced items takes it, and so does a
chunk of one item.

Any other chunk takes the **linear** mode. The items are cut into [[time groups]] of 128. Each group stores a line
through its starts. Each start is stored as its distance above that line, its **residual**. The residuals are bit-packed
at the width of the group's largest one.

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/linear_group.svg"
<figcaption>128 CAN frames of the drive from second 60 on, about 1 ms apart and up to 0.4 ms late. The group's line runs
1002.126 µs per frame, and each start is stored as its distance above it, in 10 bits instead of 64. Drawn by
<code>ww_tape_figures</code> from the first group of the chunk of <code>can0</code> at second 60, full rate, checked
against the encoder.</figcaption>
</figure>

Each group stores a 25-byte head before its residuals: its first tick, the slope of its line, the smallest residual,
the width and an offset. The slope makes periodic data cheap. A residual needs only the bits of the jitter, not those of
the 1 ms period. The encoder keeps a slope only when it narrows the width, so data without a trend loses nothing.

The drive's frames show the gain. All 3600 chunks of `can0` take the linear mode, at 9.335 bits per residual. With the
group heads, a start costs 1.37 bytes against 8 bytes raw. The encoder uses integers only, so the same starts always
give the same bytes.

### Ends and flags

A chunk of spans stores their ends in one of three end modes, which [Events](events.md#instants-and-spans) draws. With
**durations**, each item's length is packed per group of 128 above the group's shortest. The shortest plus the widest
value bounds the group's longest span, so an overlap query knows how far back to look.

Flags are one 32-bit word per item. The writer packs them per group beside the bitwise OR of the group's flags. A group
without flags costs its head only, and a chunk whose flags are all 0 has no flags column.

### Monotone relations

A track may declare a per-item value that never decreases, such as a byte offset into a stream. Such a [[monotone
relation]] is stored in its own column with the codec of the starts. A reader then finds the first item at or above a
value by search ([`RelationColumn::lower_bound`]).

The writer keeps each relation's newest value and checks the next chunk against it. After a resume over damage, the
caller tells the writer where the relation starts again. [Relation breaks](format.md#relation-breaks) gives the rule.

## Payloads

An events chunk stores the [[payloads]] in two columns. The lengths are packed per group of 128 above the group's
shortest payload. The payload bytes follow back to back, stored raw or compressed with lz4 when that is smaller. A
reader of a compressed column decodes it once, whole, when it opens the chunk.

The drive's frames have 12 bytes each: a 4-byte identifier and 8 bytes of data. Their lengths cost 0.10 bytes per frame.
Their payloads cost 0.73 bytes per frame, and lz4 was smaller than raw in all 3600 chunks.

## Samples into columns

A [[series chunk]] of the drive holds a second of `phase_a`: 25 000 [[samples]] as `i16`. The recorder encodes them with
[`ww_tape::samples::encode`] into one samples column, and builds the head from the track's [[grid]]:

```rs
--8<-- "ww_tape_example/src/record.rs:samples"
```

??? example "What the example uses here: `the clock and track numbers`, `key`, `phase_def, phase_shape`, `Act`"

    ```rs
    --8<-- "ww_tape_example/src/defs.rs:ids_item"

    --8<-- "ww_tape_example/src/defs.rs:key_item"

    --8<-- "ww_tape_example/src/defs.rs:phase_def_item"

    --8<-- "ww_tape_example/src/record.rs:act_item"
    ```

The head's first and last tick come from the grid, by [`SeriesGrid::chunk_ticks`], and every sample's tick follows from
them. The chunk stores no time per sample. The column's codec is the recorder's choice, here [`Codec::DELTA`].

### Delta codecs

A smooth signal changes little from one sample to the next. The delta codecs store those differences instead of the
values, in groups of 128. Each group stores its first value and its smallest difference. The differences less that
minimum are bit-packed at the group's width.

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/delta_group.svg"
<figcaption>128 samples of the drive's phase current from second 60 on. Their differences run from -43 to 117 mA, and
below them are the differences less the minimum. Each is stored in 8 bits instead of 16. Drawn by
<code>ww_tape_figures</code> from the first group of the chunk of <code>phase_a</code> at second 60, full
rate.</figcaption>
</figure>

[`Codec::DELTA2`] stores the differences of the differences, which suits a signal that changes by steady steps. The
drive's hour shows what each codec costs `phase_a`:

| `phase_a` codec | Bytes | Bytes per sample |
|---|---:|---:|
| [`Codec::RAW`] | 180000000 | 2.000 |
| [`Codec::DELTA`] | 99131283 | 1.101 |
| [`Codec::DELTA2`] | 96350188 | 1.071 |

Every group restarts its prediction. A reader therefore decodes a range from its group's start, and damage costs only
the groups it touches.

### Which sample codec

For data a device controls, store [[fixed-point]] integers with a delta codec, as the drive does
([Series](series.md#fixed-point-first)). They take the fewest bits and decode fastest.

A raw device format stores what the device sends, such as 12-bit values two in three bytes. A reader converts only the
samples it reads. Floats that nobody controls take [`Codec::ALP`], the one float codec.
[Numbers](numbers.md#fixed-point-first) measures each family on real data.

## What a chunk costs the writer

The writer adds little to the encoding. `just bench-chunks` measures what it spends on a series chunk of 1 MiB, which
holds 524 288 `i16` samples. The table shows the best of 5 rounds of 256 chunks.

The bench ran on a virtual machine with 14 virtual CPUs on an AMD EPYC processor of the Genoa generation. The load
average was about 5.

| What | µs per chunk | GB/s |
|---|---:|---:|
| `chunk::write_chunk`: copy and CRCs | 53 | 19.82 |
| `Block::read`: the CRCs checked | 33 | 31.32 |
| `ItemHash::of`: BLAKE3 | 213 | 4.92 |
| `TapeWriter::write_chunk` from columns | 254 | 4.12 |
| `chunk::write_chunk`, then `TapeWriter::write_block` | 281 | 3.73 |

The [[content hash]] is most of the cost.
