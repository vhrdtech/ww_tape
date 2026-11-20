---
level: advanced
---

# Sizes and speeds

The drive's endurance run lasts an hour, and the bench host keeps every run. Before the next campaign the team asks two
questions: how much disk a run takes, and whether the host keeps up. This chapter answers both from measured numbers,
and it shows how to measure them again on another machine.

In short, the drive's hour takes 110 MiB, and the phase current makes up most of it. The host records the hour a
thousand times faster than the drive runs it. Each section below takes one part of that cost, gives its numbers and says
which choice moves them.

## Where the numbers come from

Every number on this page comes from a bench in the repository. `just bench-book` runs them all at release and writes
each report as JSON into `docs/bench/`. `just book-figures` then draws the charts and tables of this page from those
files, and a test fails when a chart differs from its report.

Each report names the processor it ran on and the load average before and after each table. A table here repeats that
line under it. A time measured under load comes out slower than the machine can go, and a size never changes with the
load.

The runs on this page used one core of a virtual machine with 14 logical CPUs. Its processor is a server part of the
Genoa generation from Advanced Micro Devices (AMD). Other work kept the load average near 10 the whole time, so the times are
an upper bound. The page [Numbers](numbers.md), which held the book's numbers before this chapter, points each of its
sections here.

## The drive's hour

The drive's recording holds five [[tracks]], and two of them carry almost every byte. `phase_a` samples the phase
current 25 000 times a second, and `can0` takes about a thousand frames a second from the Controller Area Network (CAN)
bus. `just bench-drive` records the hour at full scale and sorts every byte of its four [[pieces]] by the kind of
[[block]] that holds it.

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/drive_sizes.svg"
<figcaption>Where the 110 MiB of the drive's hour go, on a log scale. Series chunks of the phase current hold
most of it; summaries and indexes take about 3 % each. Drawn by <code>ww_tape_figures</code> from
<code>docs/bench/drive.json</code>.</figcaption>
</figure>

--8<-- "docs/plots/bench_drive_sizes.txt"

The [[series chunks]] of `phase_a` take 86.41 % of the recording, and the [[events chunks]] of `can0` 7.29 %. The
[[summaries]] of `phase_a` add 2.86 %, and the [[checkpoints]] and [[final indexes]] 2.73 %. The slow tracks, the
[[puts]] and the ends of the pieces stay below one percent together.

### Bytes per item

A size grows with the items of the streams that dominate it. The next table gives what one item of each stream costs in
its chunks, against the same item stored raw.

--8<-- "docs/plots/bench_drive_items.txt"

A sample of `phase_a` costs 1.107 bytes against its 2 raw bytes. A frame of `can0` costs 2.334 bytes against its 20 raw
bytes. To predict another recording's size, multiply each stream's rate by its bytes per item. Then add about 3 % for
summaries and 3 % for the index, as the drive's hour shows.

### How long the hour takes to record

The same bench records the hour into memory, with and without the summaries of `phase_a`. The example computes every
sample and frame as it goes, so this time includes making the signals as well as writing them.

--8<-- "docs/plots/bench_drive_write.txt"

The hour takes 3.00 s with summaries and 2.86 s without them. The host thus records the run 1199 times faster than the
drive produces it, summaries included.

## Samples: fixed point first

`phase_a` stores 1.107 bytes a sample because its samples are [[fixed-point]] integers. A writer codes an integer column
with `DELTA`, which bit-packs the differences between neighbours per group of 128 samples. A float column has one codec
in the format, Adaptive Lossless floating-Point compression (ALP). ALP finds floats that began as decimals and stores
the integers they came from.

`just bench-sample-codecs` stores the same quantities both ways and checks that every value comes back bit for bit. It
uses three kinds of data: the motor simulation of `motor_sim`, a recorded speech, and the counts of a 12-bit
analog-to-digital converter (ADC) turned into floats. The [format reference](format.md#float-codecs-measured) describes
each data set.

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/sample_codecs.svg"
<figcaption>The same quantities as fixed point and as floats, per data set: ratio to the raw floats (left, larger
is better) and decode time per value (right). Fixed point gives the best ratio on every set. Drawn by
<code>ww_tape_figures</code> from <code>docs/bench/sample_codecs.json</code>.</figcaption>
</figure>

Fixed point wins on every set, and by the widest margin where the floats hide integer counts. ADC × 3.3/4096 as `f64`
reaches a ratio of 14.63 in fixed point, against 1.57 for the best float codec. ALP wins among the floats where the
scale is a short decimal: on ADC × 0.1 as `f64` it reaches 9.02.

ALP can't recover integers from a scale like 3.3/4096, which no short decimal writes. A fixed-point source hands the
writer the integers directly, so store the counts and put the scale in the track's type.

??? note "Every codec on every data set"

    The bench keeps Gorilla, Chimp and `DELTA` on the floats' ordered bits as codecs to compare against; the format
    has none of them.

    --8<-- "docs/plots/bench_sample_codecs_codecs.txt"

## Event times

A frame of `can0` costs little because of its start ticks. The writer cuts the starts of an events chunk into
[[time groups]] of 128 events. Each event keeps only its distance from a line through its group. Periodic data thus costs the
bits of its jitter, not of its period.

`just bench-event-times` codes 10 million events per workload in chunks of 4096 items. It compares the linear groups
with three other codecs. The first is raw 64-bit ticks. The second stores varints of the delta of deltas, with a skip
entry every 128 events, and the third is Elias-Fano.

One workload reads this machine's own system journal: 5 057 012 entries of the last 20 days. Each codec's answers match
the raw ticks before the bench times them.

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/event_times.svg"
<figcaption>Event start ticks per workload: bytes per event on a log scale (left), and the time to find the first
event at or after a tick (right). Linear groups are smallest on periodic data, and they find a tick faster than varints
and raw ticks. Drawn by <code>ww_tape_figures</code> from <code>docs/bench/event_times.json</code>.</figcaption>
</figure>

With a 1 ms period and ±300 ns of jitter, an event takes 1.447 bytes. The varints take 2.035 and Elias-Fano 2.789. An
exact period takes 0.014 bytes. Bursty traffic is the one generated case where the linear groups lose: 3.798 bytes
against 2.581 for the varints.

The varints can't read in place, though, and their reader walks to every item it fetches. With the linear groups, the
first event at or after a tick takes 14 to 464 ns to find. With the varints it takes 120 to 802 ns. Elias-Fano finds a
tick faster on three workloads, at a larger size on periodic data. With ten million events these lookups miss the cache,
so a load near 10 slows them most.

??? note "Every codec on every workload"

    --8<-- "docs/plots/bench_event_times_codecs.txt"

### A clock that matches its source

A [[clock]] that matches its source saves the most. Take a million video frames at 60000/1001 frames a second. On the
stream's own 60 000 Hz clock every frame lies on the period, and a frame takes 0.002 bytes. On a 1 ns clock the same
frames take 0.320 bytes, since rounding leaves a residual of 0 or 1 ns per frame.

### Ends beside the starts

Spans need an end per event as well as a start. Spans back to back end where the next one starts, so their ends cost
nothing beyond the starts. Spans with gaps between them store a duration per event.

--8<-- "docs/plots/bench_event_times_ends.txt"

## Summaries

The writer builds the summaries of `phase_a` as the samples arrive, so the summaries must keep up with ingest.
[`SeriesSummaries::push`] keeps every statistic of a [[summary node]] in one pass, through vector kernels with exact
integer sums. `just bench-summaries` measures that pass on noise of each element type, alone and with the `RAW` codec
beside it.

--8<-- "docs/plots/bench_summaries_summaries.txt"

The single pass matters more than the vector code. The next table compares the kernels with a summariser that keeps one
statistic at a time, one pass each, on nodes of 1024 frames.

--8<-- "docs/plots/bench_summaries_kernels.txt"

On `i16` the AVX-512 kernel reaches 10 671 MB/s, against 1275 MB/s one statistic at a time. Even the portable kernel,
without vector extensions, reaches 3408 MB/s.

## What the writer spends

The writer's own cost per chunk is a copy, the cyclic redundancy checks (CRCs) and the [[content hash]]. `just
bench-chunks` measures each part on a series chunk of 1 MiB, which holds 524 288 `i16` samples.

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/chunk_cost.svg"
<figcaption>What a 1 MiB series chunk costs the writer, µs per chunk. The content hash takes most of
<code>TapeWriter::write_chunk</code>, and the copy with its CRCs far less. Drawn by <code>ww_tape_figures</code>
from <code>docs/bench/chunks.json</code>.</figcaption>
</figure>

--8<-- "docs/plots/bench_chunks_writer.txt"

[`TapeWriter::write_chunk`] takes 334 µs per chunk. The [[content hash]] takes 239 µs of that. The copy with its CRCs
takes 60 µs, and a reader checks the CRCs of the whole block in 36 µs. A writer into a growing memory sink spends more,
since the sink copies its bytes each time it grows.

### Reading a track's timeline

A reader finds where a series track's samples lie from its [[chunk heads]] alone. A head read stops before the samples,
so its cost hardly grows with the chunk. `just bench-chunks` reads the timeline of 256 chunks of 1 MiB from memory, in a
file without alignment and in one aligned to 4 KiB.

--8<-- "docs/plots/bench_chunks_timeline-0.txt"

--8<-- "docs/plots/bench_chunks_timeline-4096.txt"

A head reads 335 bytes, or 4084 bytes in an aligned file, where the head reaches to the next 4 KiB boundary. A whole
block reads 1 048 895 bytes and checks its CRCs.

### Definitions and puts

The writer puts a [[definition]] only near the data that needs it, so a new track costs its own record. `just
bench-tracks` adds 10 000 tracks one by one, each with a records block, and then reads the piece back.

--8<-- "docs/plots/bench_tracks_add.txt"

Puts and [[state copies]] cost space in proportion to the tracks in use, not to the tracks defined. The bench writes a
256 MiB piece with 10 000 tracks defined and 4 KiB records blocks on the tracks in use, in turn.

--8<-- "docs/plots/bench_tracks_puts.txt"

A put that finds its item near costs almost nothing. The bench writes a definition 60 KiB long again and again, each
time within its near. It counts what the writer hashes and copies per records block.

--8<-- "docs/plots/bench_tracks_idle.txt"

## Running the measurements again

One recipe reruns every number on this page:

```sh
just jobs=4 bench-book   # every bench at release, into docs/bench/*.json, a few minutes
just book-figures        # the charts and tables of this page, from those files
```

`jobs=4` caps the build's parallel jobs, and each bench then runs on one core. Run it on a quiet machine, and compare
the load average under each table with your own run. A bench also runs alone, such as `just bench-drive`, and `--help`
lists its settings.
