---
level: advanced
---

# Event times and their codecs

A [[chunk]] keeps many [[items]] of one [[track]] field by field, and the [[writer]] takes those fields already encoded.
For [[events]], most of those bytes are times: every event stores its own start [[tick]]. How does an application
encode the times, ends and [[payloads]] of its events, and what does each cost? The examples encode a second of the
drive's `can0`: 1000 CAN frames. [Samples and their codecs](sample_codecs.md) covers the samples of a [[series]].

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

<span id="samples-into-columns"></span><span id="delta-codecs"></span><span id="which-sample-codec"></span>
<span id="what-a-chunk-costs-the-writer"></span>

## Measured costs

[Numbers](numbers.md#event-times) measures the time codec on four workloads, from an exact period to bursty traffic,
and compares it with delta-of-delta varints and Elias-Fano. The samples of a series, their codecs and what a chunk costs
the writer are on the next page, [Samples and their codecs](sample_codecs.md).
