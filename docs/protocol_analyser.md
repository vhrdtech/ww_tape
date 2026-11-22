---
level: advanced
---

# Keeping what matters of a 10 Gbps link

A protocol analyser on a network link sees every frame that passes, in both directions. At 10 Gbps a second of the link
takes gigabytes, more than the disk of a common PC writes. How does an analyser keep up, and what does it keep? It
captures into memory at the line rate, analyses each stretch there while the capture goes on, and writes only what
matters to disk. The case here is an analyser on a tap of a full-duplex Ethernet link. Its traffic is synthetic, made
by `ww_tape_example::ethernet` from fixed numbers, so every run sees the same frames.

## Frames as events

A frame is an [[event]] of its own: it arrives at one instant and carries bytes. The analyser stamps each frame with
the nanosecond its first bit arrived. The frame's bytes, from its destination address to its last payload byte, are
the event's [[payload]]. The track's items are plain bytes with no type, `RawBytes`.

A capture thread takes the frames in batches, as a network card hands them over, and pushes each batch in one call:

```rs
--8<-- "ww_tape_example/src/ethernet/mod.rs:capture"
```

The handle copies the batch into its open [[events chunk]] and seals the chunk at 1 MiB of frames. It compresses the
payloads with LZ4 and hashes them on the capture thread. A tool that already writes the pcap format could store those
records instead, as a `Foreign` item with the media type `application/vnd.tcpdump.pcap` ([Tracks](tracks.md)).

??? example "What the example uses here: `Link`"

    ```rs
    --8<-- "ww_tape_example/src/ethernet/mod.rs:link"
    ```

## Decoded layers as tracks

A decoder reads each frame's IPv4 header and its TCP or UDP header. It keeps a few fields of each. From the IPv4 header
it keeps the addresses, the protocol, the time to live and the length. From the transport header it keeps the ports, the
flags and the payload length.

Each layer goes into a [[track]] of its own, `ipv4` and `l4`. A reader looking for one flow's packets then reads 12 or 8
bytes per frame instead of the whole frame.

A decoded header must lead back to its frame. Each item of `ipv4` and `l4` keeps its frame's [[item number]] in a
relation named `frame`:

```rs
--8<-- "ww_tape_example/src/ethernet/mod.rs:frame_relation"
```

The decoder thread pushes the headers of a batch with their frames' numbers, one value each:

```rs
--8<-- "ww_tape_example/src/ethernet/mod.rs:decoded"
```

The numbers only grow, so the relation is a [[monotone relation]]. The index then keeps each chunk's smallest and
largest frame number, and a reader finds the headers of a given frame without reading other chunks. A frame without
IPv4, one in a thousand here, has no header item, so the two tracks skip its number.

## What the link takes on a tape

The link carries the simple mix of frame lengths that network tests use: 7 frames of 60 bytes, 4 of 590 and 1 of
1514. Three flows in four are TCP and the rest UDP. The payloads are random bytes, as encrypted traffic is, so they do
not compress. Each direction runs at the line rate, 10 Gbps of frames, their preambles and gaps included.

`cargo run --release -p ww_tape_example --bin ethernet_sizes` records 10 ms of the link into one chain and reads it
back:

--8<-- "docs/plots/ethernet_sizes.txt"

10 ms of the link hold 65 663 frames of 23 MB in both directions. The chain takes 1.023 bytes per byte of the frames.
The frames' own chunks take a little less than their bytes, since LZ4 finds repeats among the headers. A decoded header
takes about 7 bytes in the chain, against 12 or 8 bytes as the decoder gives it. A second of the link at its line rate
therefore takes about 2.4 GB on a tape.

## A window in memory

Memory takes the link at its line rate where a disk does not. So the analyser records into a [[chain]] of pieces in
memory, a [`MemTapes`] that keeps each [[piece]] as a tape of its own. The [[mux]] cuts a piece every 64 MiB, about
28 ms of the link. The pieces held in memory form the **window**: the newest stretch of the capture.

A triage thread reads the window while the capture goes on. It waits on the mux's [`ProgressWatch`] and opens each piece
as it closes, a [`TapeReader`] over the bytes in memory without a copy. The analyser decides whether the piece holds
something worth keeping. A kept piece is copied whole to a directory of piece files, where it reads alone. Every other
piece leaves memory unwritten ([`MemTapes::remove`]).

```rs
--8<-- "ww_tape_example/src/ethernet/ram.rs:ram_capture"
```

A verdict often depends on what came before. The window therefore keeps [`Window::before`] pieces after it read them,
until no later verdict can keep them, and [`Window::after`] pieces after a kept one are kept too. Here one piece on each
side goes with each kept piece.

The window has a size, [`Window::bytes_max`]. An analyser that falls behind lets the window grow until it is full. The
oldest pieces then leave memory unread, and [`Triaged::overrun`] counts them, as a capture card counts the frames it
drops. The test `tests/window.rs` holds the analyser back until the capture ends and checks that every piece was read
or counted.

## The analyser: TCP resets

The analyser here looks for TCP resets, the frames that end a connection abruptly. The link sends one in 4 million
frames of a port. The analyser decodes every frame of each piece and tests its flags:

```rs
--8<-- "ww_tape_example/src/ethernet/ram.rs:look"
```

Its findings go to a second chain, the results, which a mux of its own writes to disk beside the kept pieces. Each
receive queue has a track `resets`, one item per reset with its IPv4 and TCP headers. Like a decoded header, a reset
keeps its frame's number in the relation `frame`. The frame lies in the kept piece the analyser found it in. A track
`kept` names each kept piece with the span of time it covers.

The kept pieces form a chain with holes, where the pieces left memory. Each piece reads alone, and the results say why
it was kept. The test `tests/ethernet_ram.rs` checks that the kept pieces hold the frames the link sent, and that every
reset is found.

## What one PC holds and keeps

`just bench-ethernet` captures 2 s of the link into the window at full speed. The capture threads take the frames as
fast as they can, not at the link's pace, while the triage reads each piece and keeps those around a reset. It runs
once with the kept pieces in memory and once with piece files on the machine's disk.

--8<-- "docs/plots/bench_ethernet_window.txt"

--8<-- "docs/plots/bench_cases_machine.txt"

The column "Of the pace" is the seconds of the link captured per second of the run: a value of 1 or more keeps up with
the link. "Tape MB per s" is what a second of the link takes in memory. "Window s" is how many seconds of the link half
the machine's memory holds: how long a verdict may wait.

--8<-- "docs/plots/bench_ethernet_triage.txt"

The triage reads the pieces on one thread. Its "Of the pace" is the seconds of the link it gets through per second of
its own work: 1 or more keeps up. "Kept %" is the share of the capture's bytes that reaches the kept store, and "To disk
MB per s" is what the disk writes per second of the link, the results included.

## Writing every frame to disk

Some captures must keep every frame. The analyser then writes its chain straight to piece files, and the disk sets the
pace. A chain at the line rate writes about 2.4 GB a second.

One track has one producer: its handle encodes and hashes its chunks on that producer's thread. A port's 1.2 GB of
frames a second on one thread is more than one core encodes, hashes and copies. So each port spreads its flows over
receive queues, as a network card does, each queue with a capture thread, a decoder thread and its own three tracks.
The test `tests/ethernet.rs` checks that four queues hold exactly the frames of one, at the same ticks.

`just bench-ethernet` also records 2 s of the link at full speed through one or two muxes. It writes into a store that
keeps no bytes, which measures the encoding and the writing thread, and then into chains of piece files on the disk:

--8<-- "docs/plots/bench_ethernet_writer.txt"

The waits count the times a handle found the mux's queue full. Without a disk, enough receive queues keep up with the
link. With a disk, the disk is the limit, which is why the analyser keeps the link in memory and writes only what it
keeps.
