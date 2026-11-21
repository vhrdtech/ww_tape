---
level: advanced
---

# Samples and their codecs

A [[series]] stores only values, and nearly all of its bytes are [[samples]]. How the samples are coded therefore
decides the size of a recording and how fast a [[reader]] decodes it. Which numbers should a device send, and which
[[codec]] suits them? The drive's `phase_a` shows the usual case: 25 000 samples a second of a current in steps of
1 mA.

## Samples into columns

A [[series chunk]] of the drive holds a second of `phase_a`: 25 000 samples as `i16`. The recorder encodes them with
[`ww_tape::samples::encode`] into one samples [[column]], and builds the head from the track's [[grid]]:

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

A codec works on groups of 128 samples. Every group restarts its prediction, so a reader decodes any range from the
start of its group, and damage costs only the groups it touches. A chunk of several channels, such as I and Q, codes
each channel as a lane of its own. [Sample codecs](format.md#sample-codecs) lists every codec by number.

## Fixed point

A [[fixed-point]] number is an integer count of a step that the type states. The drive's current is an `i16` in steps
of 1 mA, and the device sends the count. The step lives in the track and in each chunk's head, never in a sample. An
exact_units `Fixed<Ampere, Milli, i16>` is such a type, an `i16` on the wire and in the column.

Fixed point keeps the numbers exact from the device to the file, and nothing passes through a float on the way. It is
also the smallest form of the data and the fastest to decode. A converter's counts are integers already, so a device
that controls its data sends them as they are.

## Delta codecs

A smooth signal changes little from one sample to the next. The delta codecs store those differences instead of the
values, in groups of 128. Each group stores its first value and its smallest difference. The differences less that
minimum are bit-packed at the group's width, so a constant or a ramp costs no bits per sample.

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

A device that sends its own packing, such as two 12-bit values in three bytes, can have its bytes stored as sent. A
reader converts only the samples it reads. [A fast series](series.md#a-fast-series-radio-samples) stores a radio's
samples that way.

## Floats and ALP

Some floats come from sources nobody controls, such as a sensor that reports only `f32` or a value that another
program computed. For them the format has one float codec, [`Codec::ALP`], after the paper by Afroozeh, Kuffó and Boncz
at the SIGMOD conference of 2024.

ALP looks for floats that began as decimals and stores the integers they came from. A converter's count times 0.1, for
example, becomes the count again, and ALP bit-packs those integers in groups. Floats with full mantissas take ALP's
second scheme, which codes their front bits through a small dictionary. Values that do not come back bit for bit stay
whole, such as NaN payloads, infinities and `-0.0`. A group never grows by more than 7 bytes over its raw size.

ALP can only recover integers that a short decimal scale hid. The test below codes 4096 counts of a 12-bit converter in
three ways. They are `i16` counts with `DELTA`, volts in tenths with ALP, and volts of a 3.3 V range with ALP:

```rs
--8<-- "ww_tape_tests/tests/it/sample_codecs.rs:coded"

--8<-- "ww_tape_tests/tests/it/sample_codecs.rs:fixed_against_alp"
```

The counts take about 2.3 KB. ALP undoes the scale of 0.1 and stores the same signal in about 5.2 KB, still more than
twice the size. The scale 3.3/4096 is no short decimal, so ALP stores about 27 KB of the 33 KB raw.

### Fixed point against floats

`just bench-sample-codecs` compares the same choices on real and simulated data. It stores each set as fixed-point
integers with `DELTA` and as floats with ALP. For comparison, it also tries the XOR codecs after Gorilla and Chimp, and
`DELTA` on the float bits. The bench keeps those other float codecs for comparison only; they are not part of the
format. Every value comes back bit for bit.

The bench runs in chunks of 4096 samples. This run used one core of an AMD processor, the `Ryzen AI 9 HX 370`. The
ratio is the raw float column's size over the coded size, so larger is better, and the time is the decode time per
value.

| Data | Fixed point, `DELTA` | `ALP` | Best other float codec |
|---|---|---|---|
| motor simulation, `f32` | 3.10, 0.66 ns | 1.64, 1.17 ns | 1.99, 1.02 ns |
| motor simulation, `f64` | 6.20, 0.65 ns | 2.51, 1.24 ns | 2.51, 2.60 ns |
| speech, `f32` (fixed: 16-bit PCM) | 3.43, 0.71 ns | 1.16, 1.27 ns | 1.23, 5.59 ns |
| 12-bit ADC × 3.3/4096, `f32` | 7.31, 0.65 ns | 1.59, 1.17 ns | 1.84, 0.81 ns |
| 12-bit ADC × 3.3/4096, `f64` | 14.63, 0.65 ns | 1.32, 1.59 ns | 1.57, 7.28 ns |
| 12-bit ADC × 0.1, `f32` | 7.31, 0.65 ns | 4.67, 0.81 ns | 1.84, 0.83 ns |
| 12-bit ADC × 0.1, `f64` | 14.63, 0.65 ns | 9.02, 0.72 ns | 1.59, 7.27 ns |

Fixed point is 1.6 to 9.3 times smaller than the best float codec on every set, and it decodes fastest. ALP wins among
the float codecs where the floats began as short decimals: on ADC × 0.1 as `f64`, 9.02 against 1.59. Elsewhere ALP is
level with the best other float codec or behind it by up to 18 %. It decodes at 0.72 to 1.59 ns per value, where the
XOR codecs take up to 7.3 ns.

[Float codecs measured](format.md#float-codecs-measured) describes the data sets and gives the full table with every
codec. The bench takes about 20 s.

## Which sample codec

The choice follows from who controls the numbers:

| The samples are | Store them as | Codec |
|---|---|---|
| counts or steps a device controls | fixed-point integers, the step in the type | [`Codec::DELTA`], or [`Codec::DELTA2`] for steady slopes |
| a device's own packing, such as 12-bit pairs | the device's bytes as sent | the packed element, decoded on read |
| floats nobody controls | the floats | [`Codec::ALP`] |

[Series](series.md) shows how a reader finds and decodes a range of samples.

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
