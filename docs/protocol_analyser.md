---
level: advanced
---

# Keeping what matters of a 10 Gbps link

A protocol analyser on a network link sees every frame that passes, in both directions. At 10 Gbps a second of the link
takes gigabytes, more than the disk of a common PC writes. How does an analyser keep up, and what does it keep?

It captures into memory at the line rate, analyses each stretch there while the capture goes on, and writes only what
matters to disk. The case here is an analyser on a tap of a full-duplex Ethernet link. Its traffic is synthetic, made
by `example_ethernet` from fixed numbers, so every run sees the same frames.

## Frames as events

A frame is an [[event]] of its own: it arrives at one instant and carries bytes. The analyser stamps each frame with
the nanosecond its first bit arrived. The frame's bytes, from its destination address to its last payload byte, are
the event's [[payload]]. The track's items are plain bytes with no type, `RawBytes`.

A capture thread takes the frames in batches, as a network card hands them over, and pushes each batch in one call:

```rust
--8<-- "examples/example_ethernet/src/lib.rs:capture"
```

The handle copies the batch into its open [[events chunk]] and completes the chunk at 1 MiB of frames. It codes the
[[payload]] column with the track's payload codec, here `Raw`, and hashes it on the capture thread ([why raw](#compressing-the-payloads)). A tool that already writes the pcap format could store those
records instead, as a `Foreign` item with the media type `application/vnd.tcpdump.pcap` ([Tracks](tracks.md)).

??? example "What the example uses here: `Link`"

    ```rs
    --8<-- "examples/example_ethernet/src/lib.rs:link"
    ```

## Decoded layers as tracks

A decoder reads each frame's IPv4 header and its TCP or UDP header. It keeps a few fields of each. From the IPv4 header
it keeps the addresses, the protocol, the time to live and the length. From the transport header it keeps the ports, the
flags and the payload length.

Each layer goes into a [[track]] of its own, `ipv4` and `l4`. A reader looking for one flow's packets then reads 12 or 8
bytes per frame instead of the whole frame.

A decoded header must lead back to its frame. Each item of `ipv4` and `l4` keeps its frame's [[item number]] in a
relation named `frame`:

```rust
--8<-- "examples/example_ethernet/src/lib.rs:frame_relation"
```

The decoder thread pushes the headers of a batch with their frames' numbers, one value each:

```rust
--8<-- "examples/example_ethernet/src/lib.rs:decoded"
```

The numbers only grow, so the relation is a [[monotone relation]]. The index then keeps each chunk's smallest and
largest frame number, and a reader finds the headers of a given frame without reading other chunks. A frame without
IPv4, one in a thousand here, has no header item, so the two tracks skip its number.

## What the link takes on a tape

The link carries the simple mix of frame lengths that network tests use: 7 frames of 60 bytes, 4 of 590 and 1 of
1514. Three flows in four are TCP and the rest UDP. The payloads are random bytes, as encrypted traffic is, so they do
not compress. Each direction runs at the line rate, 10 Gbps of frames, their preambles and gaps included.

`cargo run --release -p example_ethernet --bin ethernet_sizes` records 10 ms of the link into one chain and reads it
back:

--8<-- "docs/plots/ethernet_sizes.txt"

10 ms of the link hold 65 663 frames of 23 MB in both directions. The chain takes 1.051 bytes per byte of the frames.
The frames' own chunks are raw, so each takes its bytes and a little more. A decoded header
takes about 7 bytes in the chain, against 12 or 8 bytes as the decoder gives it. A second of the link at its line rate
therefore takes about 2.5 GB on a tape.

## A window in memory

Memory takes the link at its line rate where a disk does not. So the analyser records into a [[chain]] of pieces kept
in memory, a [`MemPieces`] that holds each [[piece]] apart. The [[mux]] cuts a piece every 64 MiB, about 27 ms of the
link, as it does in a chain of piece files.

The pieces held in memory form the **memory window**: the tape of the capture held in RAM, its newest stretch. Each
piece holds the same tracks, blocks and summaries a piece in a file holds, and the tracks run across the pieces. The
oldest pieces leave memory as the capture goes on.

A mux writes its chain from one thread, which copies every byte of the capture into memory. One such thread for both
ports keeps the link's pace with little to spare, as the bench below shows. So each port goes into a chain of its own,
with a mux of its own and a triage thread of its own ([`RamCapture::chains`]).

A triage thread reads each chain's window while the capture goes on. It waits on the mux's [`ProgressWatch`] and opens
each piece as it closes, a [`TapeReader`] over the bytes in memory without a copy. The analyser decides whether the
piece holds something worth keeping. A kept piece is copied whole to the chain's own directory of piece files, where it
reads alone. Every other piece leaves memory unwritten ([`MemPieces::remove`]).

The chunks of a piece are independent, so the analyser decodes them in parallel on a rayon pool, shared by the chains'
triages. By default the pool has 2 threads a chain ([`RamCapture::triage_threads`]). Nothing keeps the pool off the
capture's cores, so a large pool can take time from the capture. The capture itself keeps its explicit threads, one per
queue and one per mux.

```rust
--8<-- "examples/example_ethernet/src/ram.rs:ram_capture"
```

A verdict often depends on what came before. The window therefore keeps [`Window::before`] pieces after it read them,
until no later verdict can keep them, and [`Window::after`] pieces after a kept one are kept too. Here one piece on each
side goes with each kept piece.

Each chain's window has a size, [`Window::bytes_max`]. An analyser that falls behind lets the window grow until it is
full. The oldest pieces then leave memory unread, and [`Triaged::overrun`] counts them, as a capture card counts the
frames it drops. The test `tests/window.rs` holds the analyser back until the capture ends and checks that every piece
was read or counted.

## The pipeline

The crate's binary runs the analyser headless: `cargo run --release -p example_ethernet -- DIR` captures a second of
the link and keeps the pieces around each reset under `DIR`. Its `main` reads like a block diagram of the threads and
what connects them:

```rust
--8<-- "examples/example_ethernet/src/main.rs:main"
```

[`RamCapture::start`] starts the capture threads, the muxes and a triage thread per chain, and returns at once. The
[`RamRunning`] it gives hands out the memory window while the capture runs: per chain a [`MemChain`], its pieces in
memory and the [`ProgressWatch`] of its mux. A viewer in another thread follows the window from it, as `follow` does
here with a [`Follower`], and reads each closed piece as the triage does. The triage removes a piece once no verdict
can keep it, so a viewer finds the newest pieces only. [`RamRunning::join`] waits for the end.

## The analyser: TCP resets

The analyser here looks for TCP resets, the frames that end a connection abruptly. The link sends one in 4 million
frames of a port. The analyser decodes every frame of each piece and tests its flags:

```rust
--8<-- "examples/example_ethernet/src/ram.rs:look"
```

Its findings go to a second chain, the results, which a mux of its own writes to disk beside the kept pieces. Each
receive queue has a track `resets`, one item per reset with its IPv4 and TCP headers. Like a decoded header, a reset
keeps its frame's number in the relation `frame`. The frame lies in the kept piece the analyser found it in. Each chain
has a track `kept`, which names each kept piece of the chain with the span of time it covers.

The kept pieces of a chain form a chain with holes, where the pieces left memory. Each piece reads alone, and the
results say why it was kept. The test `tests/ethernet_ram.rs` checks that the kept pieces hold the frames the link sent,
and that every reset is found.

## What one PC holds and keeps

`just bench-ethernet` captures 2 s of the link into the window at full speed. The capture threads take the frames as
fast as they can, not at the link's pace. Meanwhile the triage reads each piece and keeps those around a reset.

The bench runs in one chain and in a chain per port, the kept pieces in memory. Each runs with 2 and with 4 receive
queues a port. With a chain per port and 4 queues it runs once more, the kept pieces in piece files on the machine's
disk.

--8<-- "docs/plots/bench_ethernet_window.txt"

--8<-- "docs/plots/bench_cases_machine.txt"

"Capture ms" is the time until every frame was in memory. The column "Of the pace" is the seconds of the link captured
per second of that time: a value of 1 or more keeps up with the link. "Wall ms" adds the time the triage took to read
the last pieces. "Tape MB per s" is what a second of the link takes in memory. "Window s" is how many seconds of the
link half the machine's memory holds: how long a verdict may wait.

In one chain, the mux's writing thread sets the pace: more receive queues do not raise it. A chain per port splits the
copying over two writing threads, and with 4 queues a port the capture runs ahead of the link.

--8<-- "docs/plots/bench_ethernet_triage.txt"

"Triage threads" is the size of the pool, marked where it is the default.

The largest capture runs again with 1, 2 and 4 threads, and with every core of the machine. One thread decodes the
chunks one after the other, as a triage without a pool does.

"Triage MB/s" is what one chain's triage reads a second. Its "Of the pace" is the seconds of the link the slowest triage
reads per second of its work: 1 or more keeps up. Compare the rows of one capture: "Capture ms" in the first table
shows what each size of the pool costs the capture.

"Kept %" is the share of the capture's bytes that reaches the kept store. "To disk MB per s" is what the disk writes per
second of the link, the results included.

## Writing every frame to disk

Some captures must keep every frame. The analyser then writes its chain straight to piece files, and the disk sets the
pace. A chain at the line rate writes about 2.5 GB a second.

One track has one producer: its handle encodes and hashes its chunks on that producer's thread. A port's 1.2 GB of
frames a second on one thread is more than one core encodes, hashes and copies. So each port spreads its flows over
receive queues, as a network card does. Each queue has a capture thread, a decoder thread and its own three tracks.
The test `tests/ethernet.rs` checks that four queues hold exactly the frames of one, at the same ticks.

`just bench-ethernet` also records 2 s of the link at full speed through one or two muxes. It first writes into a store
that keeps no bytes, which measures the encoding and the writing thread. It then writes chains of piece files on the
disk:

--8<-- "docs/plots/bench_ethernet_writer.txt"

The waits count the times a handle found the mux's queue full. Without a disk, enough receive queues keep up with the
link. With a disk, the disk is the limit, which is why the analyser keeps the link in memory and writes only what it
keeps.

## Compressing the payloads

A [[track]]'s writer chooses how each chunk codes its payload column. `Raw` runs no encoder. `Auto` tries LZ4 and keeps
it when it is smaller ([Choosing the codec](format.md#choosing-the-payload-codec)). The capture above records the
`frames` track `Raw`, because its payloads are random bytes, as encrypted traffic is. The `ipv4` and `l4` tracks keep
`Auto`: their repeated headers do compress.

`just bench-ethernet` records the link with both choices. It also records a second mix of payloads, made by
`PayloadMix::Plain`: plaintext protocols (HTTP, JSON, log lines and SQL) drawn from a small vocabulary. The mix is synthetic. It stands
for the traffic of an unencrypted network, where each frame repeats the headers of its neighbours.

The run writes into a store that keeps no bytes, with 2 receive queues a port in one chain. "CPU ns per frame" counts every thread of the
process, the one that makes the frames included, so the difference between two rows is what the codec costs.

--8<-- "docs/plots/bench_ethernet_payload.txt"

On random payloads, LZ4 saves the few percent that the repeated headers of a chunk give. It costs the encoder's CPU
time on every byte it cannot shrink, so the `Raw` rows run faster. On plaintext, LZ4 cuts the chain to a quarter of its
bytes, at the price of more CPU time per frame.

Which is worth it depends on what the chain is short of: the disk's bytes or the capture's cores. This capture is
limited by its cores and its payloads are random, so it is `Raw`. A link of plaintext protocols recorded to a disk that
is the limit wants `Auto`.
