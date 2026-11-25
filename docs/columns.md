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

```rust
--8<-- "examples/example_drive/src/record.rs:frames"
```

??? example "What the example uses here: `DriveNumbers`, `key`, `Act`"

    ```rs
    --8<-- "examples/example_drive/src/defs.rs:ids_item"

    --8<-- "examples/example_drive/src/defs.rs:key_item"

    --8<-- "examples/example_drive/src/record.rs:act_item"
    ```

The two `None`s say that the frames have no ends and no flags. A track of spans passes the ends, and one with a severity
per item passes the flags. A recorder that writes the file itself hands the head and the columns to
[`TapeWriter::write_chunk`] unchanged. The drive's recorder writes through a [`TapeMux`] and sends them with
[`ChunkSender::send`], which places the same chunk.

## Event times

An events chunk stores its start ticks in one of two time modes. A chunk of evenly spaced items needs only the period,
and any other chunk needs a start per item:

| Time mode | The chunk | The column holds |
|---|---|---|
| implicit | evenly spaced items, or one item | the period: item `j` starts at the first tick plus `j` periods |
| linear | any other | per [[time group]] of 128 items, a line through the starts and each start's distance above it |

A start's distance above its group's line is its **residual**. The residuals are bit-packed at the width of the
group's largest one.

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

A chunk of spans stores their ends in one of three end modes, which [Events](events.md#instants-and-spans) draws. The
ends, the flags and the payload lengths are packed per group of 128, as the starts are:

| Column | Each item stores | The group stores beside them |
|---|---|---|
| durations | its length above the group's shortest | the shortest |
| flags, one 32-bit word per item | its flags | the bitwise OR of the group's flags |
| payload lengths | its length above the group's shortest | the shortest |

The shortest duration plus the widest packed value bounds the group's longest span. An overlap query therefore knows
how far back to look. A zoomed-out view reads the ORs of the flags without decoding them. A group without flags costs
its head only, and a chunk whose flags are all 0 has no flags column.

### Monotone relations

A track may declare a per-item value that never decreases, such as a byte offset into a stream. Such a [[monotone
relation]] is stored in its own column with the codec of the starts. A reader reads the column of one relation:

```rust
--8<-- "ww_tape_tests/tests/it/relations.rs:relation_column"
```

It then finds the first item at or above a value by search ([`RelationColumn::lower_bound`]). The items of one value
lie between two such searches:

```rust
--8<-- "ww_tape_tests/tests/it/relations.rs:lower_bound"
```

The writer keeps each relation's newest value and checks the next chunk against it. After a resume over damage, the
caller tells the writer where the relation starts again. [Relation breaks](format.md#relation-breaks) gives the rule.

## Payloads

An events chunk stores the [[payloads]] in two columns: their lengths, and their bytes back to back. The bytes are
stored raw, or compressed with lz4 when that is smaller. A writer may also keep a track's payloads raw always, or name
a codec ([Choosing the codec](format.md#choosing-the-payload-codec)). A reader of a compressed column decodes it once,
whole, when it opens the chunk.

The drive's frames have 12 bytes each: a 4-byte identifier and 8 bytes of data. lz4 was smaller than raw in all 3600
chunks of `can0`. Each column costs, per frame:

| Column of `can0` | Bytes per frame |
|---|---:|
| starts | 1.37 |
| payload lengths | 0.10 |
| payloads | 0.73 |

<span id="samples-into-columns"></span><span id="delta-codecs"></span><span id="which-sample-codec"></span>
<span id="what-a-chunk-costs-the-writer"></span>

## Measured costs

[Sizes and speeds](sizes.md#what-event-times-cost) measures the time codec on four workloads, from an exact period to
bursty traffic, and compares it with delta-of-delta varints and Elias-Fano. The samples of a series, their codecs and
what a chunk costs the writer are on the next page, [Samples and their codecs](sample_codecs.md).
