---
level: basic
---

# Series

A sampled signal, such as a current or an audio stream, produces values at a fixed rate for as long as it runs. Its
[[samples]] need no times of their own. The question is how to keep them compact and still find the time and value
of each. A [[series]] stores them in [[fixed point]] on a [[grid]], with a [[scale]] that may change mid-run.
A [[reader]] reads any range of them.

The drive's phase current, for example, is a series: 25 000 samples a second, one every 40 µs, for the whole hour.
The last 20 ms before the fault at 52:00 are 500 samples. They show how fast the current rose past the 31 A limit.

## Samples without times

A [[series]] is a [[track]] of samples at an exact period. An ADC channel, an audio stream
and the drive's phase current are series. A sample needs no time of its own, since the period gives it. A series
therefore stores only values, in [[series chunks]], and the drive's `phase_a` costs about 1.1 bytes per
sample ([Sizes and speeds](sizes.md#bytes-per-item)).

The track's items and its definition, a [`GridDef`], say what a sample is:

| Field | What it says | `phase_a` | `iq`, an RF capture |
|---|---|---|---|
| items: `Numbers` | the stored element and the type of one value, as the producer's API declares it | `i16`, a current in mA | a complex `i16` of the radio's converter |
| `channels` | the elements of one sample | one | one |
| `period` | ticks per sample, as an exact fraction | 40/1 on the board's µs clock | 50/1 on a 1 ns clock |
| `scale` | what one step of a stored number is worth | 10⁻³ A | one step of the converter |
| `summaries` | the coarse view the writer builds | minimum, maximum, sum, sum of squares | minimum, maximum, mean and largest power |

The period is a fraction, so no sample time drifts by rounding. Audio at 48 kHz on a 1 ns clock has the period
62500/3: three samples take exactly 62 500 ns.

## Fixed point first

For data a device controls, store fixed-point integers. The drive's current is an `i16` in steps of 1 mA: an `i16` on
the wire and an `i16` in the column. The scale, 10⁻³ A per step, lives in the track and in each chunk's head, never in a
sample. The numbers stay integers from the device to the file, and nothing passes through a float.

Integers also compress best. The writer stores the differences between neighbouring samples, bit-packed per group of
128 samples, and a smooth signal needs few bits per difference. Fixed point is smaller than the best float codec on
every data set that [Sizes and speeds](sizes.md#why-fixed-point-beats-floats) measures. Floats that a device did not
choose, such as a sensor that reports only `f32`, have codecs of their own. [Samples and their
codecs](sample_codecs.md) explains the codecs and when to pick each.

A device that sends its own packing, such as two 12-bit values in three bytes, can have its bytes stored as sent. A
reader converts only the samples it reads.

## A fast series: radio samples

A series at a high rate is stored the same way as a slow one, and only the numbers grow. The second case of this
chapter is an RF capture. A software-defined radio's front end turns a band of spectrum into 20 million complex samples
a second. Each sample is a pair of values, the in-phase I and the quadrature Q, and the capture is one track,
`iq`.

The track's items are numbers of two parts, a 16-bit signed integer each:

```rs
--8<-- "examples/example_capture/src/rf.rs:element"
```

A sample's I and Q lie side by side in the column. Every codec codes them as two lanes, so neither needs a track of
its own. The converter gives 12 bits in each 16-bit word. The summaries keep the power of each node, `I² + Q²`, which
shows where a signal is on the air:

```rs
--8<-- "examples/example_capture/src/rf.rs:def"
```

??? example "What the example uses here: `Rf`"

    ```rs
    --8<-- "examples/example_capture/src/rf.rs:rf_item"
    ```

The track stores the samples in the converter's packed form, `PACKED12_LSB`: 12 bits per value, as the front end
sends them. A sample then takes 3 bytes instead of 4, and a minute of the capture is a piece of 3.7 GB. [Sizes and
speeds](sizes.md) gives the measured sizes and how fast the writer and the reader go.

<figure class="ww-figure" markdown="1">
![The RF capture's I and Q in eventfull at three zooms.](shots/rf-minute-light.png#only-light){ width="880" loading="lazy" }
![The RF capture's I and Q in eventfull at three zooms.](shots/rf-minute-dark.png#only-dark){ width="880" loading="lazy" }
<figcaption>The capture in a viewer, I and Q in lanes of their own. Over the minute, the summaries show five bursts
above the carriers. At 200 ms the burst at 7.3 s stands out, and at 20 ms the viewer draws its 2 ms of samples. A
screenshot rendered by <code>eventfull_book_shots</code> from the capture at 1/100 of its rate. The signal is the
same, at 200 thousand samples a second.</figcaption>
</figure>

### Samples and what they mean, in two chains

A radio's samples and what a decoder finds in them grow at very different rates. The example's decoder finds bursts
by the mean power of 16 samples, and it measures the band's mean power each second. A minute at 1/100 of the rate
then holds 38 MB of samples and five bursts.

Each kind goes into a [[chain]] of its own, `iq` and `decoded`. A reader of the bursts never walks the samples, and
each chain is cut at its own size. One process writes both, each through a [`TapeMux`] of its own:

```rs
--8<-- "examples/example_rf/src/chains.rs:two_muxes"
```

A radio thread pushes each chunk's samples into the series handle of `iq` and hands them to a decoder thread through
a bounded queue. The decoder pushes a span per burst into the track `bursts` and a power reading per second into the
track `power`. It completes each second's finds, so they reach the tape a second at a time and in time order:

```rs
--8<-- "examples/example_rf/src/chains.rs:threads"
```

Each mux places the units of its own handles on its own writing thread, so neither chain waits for the other's
writes. A crash leaves each chain unclosed at its own byte, and each resumes alone. The test `mux_multi.rs` of
`ww_tape_tests` cuts two such chains at 201 pairs of bytes and resumes both at once.

A reader follows both chains while they are written. A [`ChainFollower`] on each hands out the blocks that check, and
a [`TimeMerge`] gives their items in time order. A chunk of samples counts at the tick of its first sample, and a find
at its start. The two tracks of `decoded` form one source, since the decoder sends them in time order. The merge
holds the chunks of samples back until the decoder's next second shows that no burst comes before them:

```rs
--8<-- "examples/example_rf/src/chains.rs:follow"
```

`cargo run --release -p example_rf DIR` writes the minute at 1/100 of the rate. The chain `iq`
holds 60 pieces of 38 MB together, a piece per second, and `decoded` one piece of 25 KB. The reader merges 6000 chunks
and 65 finds, none of them late. Each burst comes right after the chunk that holds its first sample. The burst at
7.3 s comes after the chunk from sample 1 460 000.

At 50 times the rate, a billion samples a second, the same signal needs its band split into channels. [Keeping the
bursts of a fast band](wideband.md) spreads them over several muxes and measures where that stops.

## Grids: where samples are in time

A series has no records. Each series chunk's head carries the [[grid]] its samples are on instead, a [`SeriesGrid`]:

- `grid_index`, a sample number at or before the chunk's first sample;
- `grid_tick`, that sample's tick;
- `scale`, what a stored number of the chunk is worth.

Sample `i` lies at `grid_tick + (i - grid_index) · period`, rounded down. A chunk thus times and scales its samples
alone: nothing else has to be found, linked or kept near it. Consecutive chunks of one stretch of recording carry the
same grid, so no sample time moves, however the samples are cut into chunks.

The program reads the chunk that holds 51:59.980 and the 500 samples from there. It turns the time into a sample
number with the grid, then decodes from that sample on:

```rs
--8<-- "examples/example_drive/src/fault.rs:current"
```

??? example "What the example uses here: `fault_us`, `the fault's constants`, `series_def`, `Current`"

    ```rs
    --8<-- "examples/example_drive/src/fault.rs:fault_us_item"

    --8<-- "examples/example_drive/src/fault.rs:fault_consts_item"

    --8<-- "examples/example_drive/src/fault.rs:series_def_item"

    --8<-- "examples/example_drive/src/fault.rs:current_type_item"
    ```

The chunk holds 25 000 samples in a block of about 28 KB, at period 40/1 µs and scale 10⁻³. The first sample read is
item 77 999 500 of `phase_a`. The largest in magnitude is -31 873 mA at 51:59.995, past the 31 A limit, 5 ms before
the drive tripped. The step reads about 29 KB in 6 reads: the definitions in force, to learn the period, and the
block.

[`ww_tape::Samples::decode`] starts at the group of 128 samples that holds the first sample asked for. A reader of any
range therefore decodes at most 127 samples it does not need.

### Gaps and lost samples

A grid ends where the samples stop following it. A restart of the device on the same clock, or a stretch the device
did not sample, starts a new grid. The new grid starts more than a period after the last tick of the chunk before,
and a viewer draws no line across that [[gap]].

Sample numbers that are missing between two chunks of one grid are [[lost samples]]. A chunk of them was damaged, or
lies in a piece the reader does not have. Their ticks are known, since the grid gives them; their values are not.

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/grids.svg"
<figcaption>Grids of a 48 kHz series on a 1 ns clock: lost samples, a prefix change and a gap. Drawn by
<code>ww_tape_figures</code>; the ticks are <code>SeriesTimeline</code>'s.</figcaption>
</figure>

[`TapeReader::series_timeline`] builds a track's timeline, a [`SeriesTimeline`], from its chunk heads alone. It
reads each chunk's head with [`TapeReader::read_chunk_head`], up to the payload and none of the samples. Some chunks are
left out as lost: one whose head is damaged, or one that does not line up with its neighbours. Each one's error comes
back beside the timeline.

## Changing scale mid-run

Some instruments change range while they record. A current meter that measures a capacitor's charging current
reports milliamperes at first, as integers at scale 10⁻³. As the current dies away, the meter switches to its micro
range at sample 10 000 and reports microamperes at scale 10⁻⁶.

The writer ends the open chunk with sample 9 999. The next chunk carries the same grid at scale 10⁻⁶: its integers
mean microamperes. Sample 10 000 keeps the tick that the grid gives it. A chunk has one scale, and the change costs
one chunk boundary.

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/autorange.svg"
<figcaption>An autoranging current meter switches from milliamperes to microamperes at sample 10 000. The chunks keep
one grid, and only the scale changes. The summary node across the change holds both at their common scale, 10⁻⁶.
Drawn by <code>ww_tape_figures</code> from the chunks and summaries that ww_tape writes and reads back.</figcaption>
</figure>

A [[summary node]] that covers samples of both scales keeps them at their common scale. That is the largest scale
that both are whole multiples of, 10⁻⁶ here. The third node in the drawing covers the switch, and its maximum reads
6000 at 10⁻⁶, though its chunk stores that sample in milliamperes. The node's minimum and maximum thus stay exact
integers. Two scales without a common scale are refused, never mixed.

## Wire batches and file columns

A device sends samples to the host in `Series<T>` batches of the [WireWeaver wire
format](https://ww.vhrd.tech/serdes/series/). The file stores them in sample columns. The two layers code the same
numbers for different readers:

| | `Series<T>` on the wire | Sample columns in a tape |
|---|---|---|
| Size | a batch of about 32 items | a chunk of up to megabytes |
| Read | from the start, item by item | any range of samples, by group, in place |
| Encoder | sized for a microcontroller | sized for a host, with vector kernels |
| Scale | a prefix per item or in the type | one per chunk |

The host decodes a batch once and codes its numbers again for the file. The integers pass through unchanged. A prefix
change ends the chunk where the batch restarts its column. The file keeps no time column of the batch, since the grid
gives every sample's tick.

A whole hour of 90 million samples is too much to draw by reading them. The next chapter draws it from the
[[summaries]] that the writer kept beside the chunks.
