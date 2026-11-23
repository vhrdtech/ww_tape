---
level: advanced
---

# Keeping the bursts of a fast band

A wideband radio receiver digitizes a band at hundreds of millions of complex samples a second. Most of the time the
band holds the same carriers and noise, and what matters are the short bursts between them. How does one PC keep the
bursts of such a band without writing all of it?

It captures into memory, finds the bursts there from the energy in the summaries, and writes only the stretches around
them. The case here builds on the RF capture of [Series](series.md#a-fast-series-radio-samples), at 12.5 times its rate.
The crate `example_wideband` makes it.

## Channels, each a track

A receiver at this rate splits its band into channels of equal width, each a stream at its share of the rate. The
example takes 250 million samples a second in 2 channels of 125 million. Each channel carries the RF capture's signal,
its carriers and its bursts, with noise of its own.

Each channel is a [[series]] track `iq` with a producer thread of its own. The thread pushes the channel's samples into
a series handle. The handle packs them at 12 bits and cuts them into chunks of 1 MiB of samples:

```rs
--8<-- "examples/example_wideband/src/lib.rs:channel"
```

The split into channels is what lets the work spread. A track has one producer, and its handle does the costly work on
that producer's thread. It packs the samples, hashes them and computes the leaves of the [[summaries]]. One thread then
serves one channel's 125 million samples a second, not the whole band.

??? example "What the example uses here: `Wideband`"

    ```rs
    --8<-- "examples/example_wideband/src/lib.rs:wideband"
    ```

## Energy in the summary nodes

A burst raises the power `I² + Q²` of the samples it covers. The leaves of a series' summaries already hold that
power. Each [[summary node]] of level 0 keeps the mean and the largest power of 1 024 samples. An analyser that detects
energy therefore reads the nodes, never the samples: a few values for each 1 024 samples.

The analyser opens a piece, reads the summary blocks of level 0 of each channel, and compares each node's largest power
with a threshold. The carriers of the band add up to 1 050 steps of amplitude, and a burst adds 900 more. A threshold of
1.5 million, [`POWER_MIN`], lies between a quiet node and one that holds a burst:

```rs
--8<-- "examples/example_wideband/src/ram.rs:detect"
```

Consecutive loud nodes make one burst, from the first node's start to the last node's end. A burst may cross from one
[[piece]] into the next: the analyser carries it over and ends it at the next quiet node. The nodes come from one
function, which the overview below uses too:

```rs
--8<-- "examples/example_wideband/src/lib.rs:summary_nodes"
```

## A window in memory

The receiver records into the memory window, as the protocol analyser does in [Keeping what matters of a 10 Gbps
link](protocol_analyser.md#a-window-in-memory). The window is a [[chain]] of pieces kept in memory, the tape of the
capture held in RAM. A [[mux]] writes the chain into a [`MemTapes`], a piece every 64 MiB. Each piece holds the same
tracks, blocks and summaries a piece in a file holds. A triage thread opens each piece as it closes and asks the
analyser whether it holds a burst.

A piece with a burst is copied to a piece file, with one piece before it and one after it. Every other piece leaves
memory unwritten. The band takes fewer bytes a second than the link, and one chain keeps up with it, as the bench below
shows.

```rs
--8<-- "examples/example_wideband/src/ram.rs:rf_capture"
```

The analyser's findings go to a chain of results on disk. Each channel has a track `bursts`, one span per burst with its
largest power. A track `kept` names each kept piece with the span of time it covers. A minute of the band holds five
bursts of 2 ms each, so the disk sees a few pieces a minute.

The test `tests/wideband_ram.rs` records a minute at 1/5 000 of the rate. It checks that each channel's five bursts are
found and that their samples lie in the kept pieces. It also checks that those pieces hold the signal's own samples.

The crate's binary runs the receiver headless: `cargo run --release -p example_wideband -- DIR` captures 10 s of the
band, past its first burst, and keeps the pieces around each burst under `DIR`. Its `main` names each thread and what
it feeds, and [`RfCapture::start`] hands out the memory window while the capture runs, as the analyser's
[pipeline](protocol_analyser.md#the-pipeline) does:

```rs
--8<-- "examples/example_wideband/src/main.rs:main"
```

## What one PC holds and keeps

`just bench-wideband` captures the band into the window at full speed. One run replays chunks made before it, so it
times the recorder and the triage, not the signal's maths; it holds no bursts. A second run makes the signal as it
goes for a minute, its bursts included. It keeps the pieces around the bursts in piece files on the machine's disk.

--8<-- "docs/plots/bench_wideband_window.txt"

--8<-- "docs/plots/bench_cases_machine.txt"

"Capture ms" is the time until every sample was in memory. The column "Of the pace" is the seconds of the band captured
per second of that time: a value of 1 or more keeps up. The live run makes its samples as it goes, and that maths sets
its pace. "Window s" is how many seconds of the band half the machine's memory holds.

--8<-- "docs/plots/bench_wideband_triage.txt"

The triage reads only summary nodes, so it gets through the band many times faster than the band arrives.

"Kept %" is the share of the capture's bytes that reaches the disk, and "To disk MB per s" what the disk writes per
second of the band.

## The overview from the summaries

A viewer that shows a minute of a channel draws from its [[summary nodes]], never from its samples. Each node holds
the smallest and largest I and Q and the mean and largest power `I² + Q²` of its samples. A node of level 3 covers
4 194 304 samples, 34 ms of a channel. The overview opens each [[piece]] of the chain and reads only the summary
blocks of one level:

```rs
--8<-- "examples/example_wideband/src/lib.rs:overview"
```

Each piece has a tree of its own. The nodes at a piece's ends may therefore hold fewer samples. The test finds the five
bursts of a minute in the overview alone. Exactly the nodes that hold a burst stand out by their largest power. The
bench reads channel 0's overview from its chain of files:

--8<-- "docs/plots/bench_wideband_overview.txt"

## Writing the whole band to disk

Some captures must keep every sample. A receiver of a billion samples a second splits its band into 8 channels of
125 million. Packed at 12 bits, a sample takes 3 bytes and a little more for its chunk, its summaries and its share of
the index. At 1 GS/s, a second of the receiver takes about 3.1 GB, and a minute about 183 GB.

A [`TapeMux`] places every chunk of its handles on its one writing thread, into one chain. So channel `k` goes to mux
`k % m`, and each mux writes a chain of its own, on a disk of its own if the machine has several. The test
`tests/wideband.rs` records the channels through two muxes and reads every sample back from its chain.

`just bench-wideband` records the receiver at full speed into a store that keeps no bytes, once per number of channels,
muxes and codec. With the most channels and muxes it then writes chains of piece files on the machine's disk:

--8<-- "docs/plots/bench_wideband_writer.txt"

The column "Of 1 GS/s" is the samples recorded a second against the receiver's rate. The waits count the times a handle
found its mux's queue full. Without a disk, enough channels and muxes come near the full rate. With a disk, a second of
the receiver is about 3.1 GB, more than one disk of a common PC writes in a second. That is why the receiver keeps the
band in memory and writes only the bursts.

The band has no payload column to compress. A channel is a series track, and its samples go through a sample codec:
`RAW` and `PACKED12_LSB` are the two the table compares. "Bytes per sample" is what the chain keeps for each sample,
and "CPU ns per sample" is the CPU time of every thread of the process for it. Packing saves a byte of each four and
costs more CPU time. The bursts and the kept pieces the analyser writes are events of 8 bytes, which no payload codec
shrinks.

