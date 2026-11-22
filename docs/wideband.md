---
level: advanced
---

# A billion samples a second

A wideband radio receiver can digitize a band at a billion complex samples a second. As 16-bit I and Q that is 4 GB
of samples every second. What does it take to keep such a stream on a tape, on one PC, and where is the limit? The
case here builds on the RF capture of [Series](series.md#a-fast-series-radio-samples), at 50 times its rate. The crate
`ww_tape_example` makes it in the module `wideband`.

## Channels, each a track

A receiver at this rate splits its band into channels of equal width, each a stream at its share of the rate. The
example splits it into 8 channels of 125 million samples a second. Each channel carries the RF capture's signal, its
carriers and its bursts, with noise of its own.

Each channel is a [[series]] track `iq` with a producer thread of its own. The thread pushes the channel's samples into
a series handle. The handle packs them at 12 bits and cuts them into chunks of 1 MiB of samples:

```rs
--8<-- "ww_tape_example/src/wideband.rs:channel"
```

The split into channels is what lets the work spread. A track has one producer, and its handle does the costly work on
that producer's thread. It packs the samples, hashes them and computes the leaves of the [[summaries]]. One thread then
serves one channel's 125 million samples a second, not the whole billion.

??? example "What the example uses here: `Wideband`"

    ```rs
    --8<-- "ww_tape_example/src/wideband.rs:wideband"
    ```

## Several muxes, several chains

A [`TapeMux`] places every chunk of its handles on its one writing thread, into one [[chain]]. At this rate one writing
thread and one disk become the limit. So channel `k` goes to mux `k % m`, and each mux writes a chain of its own in a
directory of its own. One process holds them all, as the RF capture's two chains in [Series](series.md) do.

Each chain then holds whole channels, and each can sit on a disk of its own. A crash leaves each chain unclosed at its
own byte, and each resumes alone. A reader of one channel opens only that channel's chain.

## What a second takes

Packed at 12 bits, a sample takes 3 bytes and a little more for its chunk, its summaries and its share of the index. A
second of the receiver takes about 3.1 GB, and a minute about 183 GB. The test `tests/wideband.rs` records the
channels through two muxes and reads every sample back from its chain.

## How fast it records

`just bench-wideband` records the receiver at full speed into a store that keeps no bytes, once per number of channels,
muxes and codec. Each channel's thread pushes chunks made before the run, again and again, so the run times the
recorder and not the maths of the signal. With the most channels and muxes it then writes chains of piece files on the
machine's disk.

--8<-- "docs/plots/bench_wideband_writer.txt"

--8<-- "docs/plots/bench_cases_machine.txt"

The column "Of 1 GS/s" is the samples recorded a second against the receiver's rate. A
value of 1 or more keeps up. The waits count the times a handle found its mux's queue full: its writing thread, or the
disk behind it, fell behind.

Three limits stand in turn. Each channel's thread packs, hashes and summarizes its own samples, so more channels spread
that work over more cores. Each mux places its chunks on one writing thread, so more muxes spread the placing.

Past both, the disk decides: a second of the receiver is about 3.1 GB. That is more than one disk of a common PC writes
in a second. A machine that keeps the whole band needs a chain per fast disk, or it keeps fewer channels.

The machine of the table sits at the edge. Without a disk, raw samples pass the full rate once two muxes or more share
the channels. Packed 12-bit samples come near it with eight channels or more, with every core busy. Its disk keeps only
a small part of the band. On such a machine the receiver records the whole band into memory for a short capture, or a
few of its channels to disk.

## The overview from the summaries

A viewer that shows a minute of a channel draws from its [[summary nodes]], never from its samples. Each node holds
the smallest and largest I and Q and the mean and largest power `I² + Q²` of its samples. A node of level 3 covers
4 194 304 samples, 34 ms of a channel. The overview opens each [[piece]] of the chain and reads only the summary
blocks of one level:

```rs
--8<-- "ww_tape_example/src/wideband.rs:overview"
```

Each piece has a tree of its own. The nodes at a piece's ends may therefore hold fewer samples. The test finds the five
bursts of a minute in the overview alone. Exactly the nodes that hold a burst stand out by their largest power. The
bench reads channel 0's overview from its chain of files:

--8<-- "docs/plots/bench_wideband_overview.txt"
