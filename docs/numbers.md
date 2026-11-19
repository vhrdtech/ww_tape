# Numbers

This page answers two practical questions: how to store numbers so that they stay small and fast to read, and what the
format costs in bytes and time. Every figure names the bench that produced it and the machine it ran on. Figures from a
loaded machine are lower than the machine can do, so each table also gives the load. The page assumes [Tape
concepts](concepts.md) and [Events and series](events_and_series.md).

## Fixed point first

Store a value the device measures as a fixed-point integer, not as a float. An exact_units `Fixed<Ampere, Milli, i32>`
is an `i32` on the wire and an `i32` in the series column. Its scale, 10⁻³ A per step, is stated once in the track's
definition, not in every sample. The numbers stay integers from the sensor to the file, and nothing passes through a
float on the way.

Integer columns are coded with `DELTA`: per group of 128 samples, the differences between neighbours minus their
smallest difference, bit-packed. A constant or a ramp costs no bits per sample. `DELTA2` codes differences of
differences the same way, which suits a signal with a steady slope. Every group restarts its prediction, so any range
of samples decodes from its group's start.

## ALP, the one float codec

Some floats come from sources nobody controls, such as a sensor that reports only `f32` or a value computed by another
program. For them the format has one float codec, **ALP** (after Afroozeh, Kuffó and Boncz, SIGMOD 2024). ALP finds
floats that began as decimals, such as an ADC count times 0.1, and stores the integers they came from, bit-packed per
group of 128. Floats with full mantissas take ALP's second scheme, which codes their front bits through a small
dictionary. Values that don't come back bit for bit, such as NaN payloads, infinities and `-0.0`, are kept whole. A
group never grows by more than 7 bytes over its raw size.

### Fixed point against floats

`just bench-sample-codecs` stores the same data in three ways:

- as fixed-point integers with `DELTA`;
- as floats with ALP;
- as floats with the XOR codecs after Gorilla and Chimp, and with `DELTA` on the float bits.

The codecs of the last item are kept in the bench for comparison and are not part of the format. The bench checks that
every value comes back bit for bit. It runs in chunks of 4096 samples, here on one core of an AMD Ryzen AI 9 HX 370.
The ratio is the raw float column's size over the coded size, so larger is better. The decode time is in nanoseconds
per value.

| Data | Fixed point, `DELTA` | `ALP` | Best other float codec |
|---|---|---|---|
| motor simulation, `f32` | 3.10, 0.66 ns | 1.64, 1.17 ns | 1.99, 1.02 ns |
| motor simulation, `f64` | 6.20, 0.65 ns | 2.51, 1.24 ns | 2.51, 2.60 ns |
| speech, `f32` (fixed: 16-bit PCM) | 3.43, 0.71 ns | 1.16, 1.27 ns | 1.23, 5.59 ns |
| 12-bit ADC × 3.3/4096, `f32` | 7.31, 0.65 ns | 1.59, 1.17 ns | 1.84, 0.81 ns |
| 12-bit ADC × 3.3/4096, `f64` | 14.63, 0.65 ns | 1.32, 1.59 ns | 1.57, 7.28 ns |
| 12-bit ADC × 0.1, `f32` | 7.31, 0.65 ns | 4.67, 0.81 ns | 1.84, 0.83 ns |
| 12-bit ADC × 0.1, `f64` | 14.63, 0.65 ns | 9.02, 0.72 ns | 1.59, 7.27 ns |

Three facts follow from the table:

- Fixed point is 1.6 to 9.3 times smaller than the best float codec on every set, and it decodes fastest.
- ALP wins among the float codecs where the floats began as short decimals: on ADC × 0.1 as `f64`, 9.02 against 1.59.
- Elsewhere ALP is level with the best other float codec or behind it by up to 18 %. It decodes at 0.72 to 1.59 ns
  per value, where the XOR codecs take up to 7.3 ns.

ALP recovers integers from floats only when the scale is a short decimal: 0.1 is one, but 3.3/4096 isn't. A
fixed-point source hands the writer those integers directly. Store the ADC counts, put the scale in the type, and no
float codec is needed. The [format reference](format.md#sample-codecs) describes the data sets and gives the full table
with every codec. To rerun the comparison, run `just bench-sample-codecs`, which takes about 20 s.

## Event times

Event start ticks are stored per group of 128 items as distances from a line through the group, bit-packed. Periodic
data with jitter so costs the bits of its jitter, not of its period. The table gives bytes and nanoseconds per event
for 10 million events per workload (6.4 million for the journal), in chunks of 4096 items. Raw 64-bit ticks take 8.01
bytes per event.

The figures come from a bench of an application that records with tapes, on a virtual machine with 14 vCPUs of an AMD
EPYC Genoa host. That bench is not part of this repository. Here, the bench `starts` of `ww_tape_benchmarks` measures
only reads of a salvaged starts column.

| Workload | Bytes per event | Coding, ns per event | Fetch item `j`, ns | First at or after a tick, ns |
|---|---:|---:|---:|---:|
| exact 1 ms period with a few gaps | 0.022 | 0.5 | 1.8 | 15.3 |
| 1 ms period, ±300 ns jitter, 1 ns ticks | 1.48 | 3.5 | 15.6 | 97.1 |
| a real system journal, 1 µs ticks | 2.97 | 5.2 | 26.4 | 163.8 |
| bursty traffic, 1 ns ticks | 3.84 | 5.5 | 23.3 | 216.6 |

[Events and series](events_and_series.md#what-it-costs) compares the codec with two alternatives on the same
workloads.

A clock that matches its source saves the most. One million video frames at 60000/1001 frames per second cost 0.010
bytes per frame on the stream's own 60 000 Hz clock. On a 1 ns clock they cost 0.36 bytes per frame, because rounding
leaves a residual of 0 or 1 ns per frame.

## Summaries

A writer builds summaries of a series as the samples arrive, so the summaries must keep up with ingest.
`SeriesSummaries::push`, in the crate `ww_tape_summary`, keeps every statistic in one pass over a batch of samples.
Integers up to 32 bits go through vector kernels with exact integer sums, so the order of addition never changes a
result. The kernels come in AVX-512, AVX2 and portable forms, chosen at run time. 64-bit integers and floats take one
sample at a time in sample order, because a float sum depends on the order.

`just bench-summaries` measures the throughput per element type in three cases: the raw sample codec alone, the
summaries alone, and both together. It uses nodes of 1024 samples, 16 384 frames per push and every statistic. This
run used one channel on an AMD EPYC Genoa virtual machine with 14 vCPUs (AVX2 and AVX-512) at a load average of
about 5:

| Element | Summaries | Raw codec and summaries |
|---|---:|---:|
| `i16` | 5123 MB/s | 4731 MB/s |
| complex `i16` | 6423 MB/s | 5907 MB/s |
| `i32` | 7858 MB/s | 7086 MB/s |
| `i64` | 5276 MB/s | 4878 MB/s |
| `f32` | 2428 MB/s | 2463 MB/s |
| `f64` | 4901 MB/s | 5084 MB/s |

MB/s counts sample bytes. The bench prints the CPU's vector extensions and the load average with its table.

The vector kernels are why summaries keep up. The next table compares them with a summariser that keeps one statistic
at a time, in one pass per statistic. Both ran with the same bench settings on one AMD EPYC Genoa core at a load
average of 4:

| Element | One statistic at a time | Single-pass kernels |
|---|---:|---:|
| `i16` | 812 MB/s | 4586 MB/s |
| complex `i16` | 486 MB/s | 5341 MB/s |
| `i32` | 1084 MB/s | 7055 MB/s |

The portable kernel alone, without AVX, summarised complex `i16` at 2108 MB/s. `just bench-summaries` contains neither
the one-statistic summariser nor a switch to the portable kernel. This table therefore can't be rerun with the bench.

## Writing chunks

The writer's own cost per chunk is a copy, the CRCs and the content hash. `just bench-chunks` measures it on series
chunks of 1 MiB, and [Writing a tape](writing.md#writing-data) gives its table. On the AMD EPYC Genoa virtual machine
of the summaries bench, `TapeWriter::write_chunk` took 254 µs per chunk. BLAKE3 took 213 µs of that, and the copy with
its CRCs 53 µs. The CRCs run on the CPU's CRC instructions where it has them (SSE4.2, ARMv8), and checking the CRCs of
a 1 MiB block took 33 µs.

## Definitions and puts

The writer puts a definition only near the data that needs it. A new track so costs its own record, however many
tracks the file already has. `just bench-tracks` added 10 000 tracks one by one, each followed by a records block, in
0.048 s on an AMD Ryzen AI 9 HX 370 at a load of 2.1. Finishing that piece took 0.014 s, and opening it and reading
its definitions back took 0.015 s.

Puts and copies cost space in proportion to the tracks in use, not to the tracks defined. The same bench wrote a
256 MiB piece with 10 000 tracks defined and 4 KiB records blocks on the tracks in use, in turn. It used the default
policies: `near` of 32 MiB, and one copy 1 MiB after a first write.

| Tracks in use | Put and copied | Of the piece |
|---:|---:|---:|
| 10 000 | 4 968 898 bytes | 1.851 % |
| 100 | 71 109 bytes | 0.026 % |

A put that finds its item near costs nothing to write. The bench wrote a definition 60 KiB long again and again
within its `near`, and measured 0 bytes hashed and 0 bytes copied per records block. The writer compares a hash it
computed once, when the application handed it the definition.
