---
level: advanced
---

# Every frame of a 10 Gbps link

A protocol analyser on a network link keeps every frame that passes, in both directions, and what its decoders find in
them. Can a tape hold a 10 Gbps link at its line rate, and can one PC write it that fast? The case here is an analyser
on a tap of a full-duplex Ethernet link. Its traffic is synthetic, made by `ww_tape_example::ethernet` from fixed
numbers, so every run sees the same frames.

## Frames as events

A frame is an [[event]] of its own: it arrives at one instant and carries bytes. The analyser stamps each frame with
the nanosecond its first bit arrived. The frame's bytes, from its destination address to its last payload byte, are
the event's [[payload]]. The track's items are plain bytes with no type, `RawBytes`.

A capture thread takes the frames in batches, as a network card hands them over, and pushes each batch in one call:

```rs
--8<-- "ww_tape_example/src/ethernet.rs:capture"
```

The handle copies the batch into its open [[events chunk]] and seals the chunk at 1 MiB of frames. It compresses the
payloads with LZ4 and hashes them on the capture thread. A tool that already writes the pcap format could store those
records instead, as a `Foreign` item with the media type `application/vnd.tcpdump.pcap` ([Tracks](tracks.md)).

??? example "What the example uses here: `Link`"

    ```rs
    --8<-- "ww_tape_example/src/ethernet.rs:link"
    ```

## Decoded layers as tracks

A decoder reads each frame's IPv4 header and its TCP or UDP header. It keeps a few fields of each: addresses, protocol,
TTL and length of the IPv4 header, and the ports, flags and payload length of the transport header. Each layer goes
into a [[track]] of its own, `ipv4` and `l4`. A reader looking for one flow's packets then reads 12 or 8 bytes per frame
instead of the whole frame.

A decoded header must lead back to its frame. Each item of `ipv4` and `l4` keeps its frame's [[item number]] in a
relation named `frame`:

```rs
--8<-- "ww_tape_example/src/ethernet.rs:frame_relation"
```

The decoder thread pushes the headers of a batch with their frames' numbers, one value each:

```rs
--8<-- "ww_tape_example/src/ethernet.rs:decoded"
```

The numbers only grow, so the relation is a [[monotone relation]]. The index then keeps each chunk's smallest and
largest frame number, and a reader finds the headers of a given frame without reading other chunks. A frame without
IPv4, one in a thousand here, has no header item, so the two tracks skip its number.

## What the link takes on a tape

The link carries the simple mix of frame lengths that network tests use: 7 frames of 60 bytes, 4 of 590 and 1 of
1514. Three flows in four are TCP and the rest UDP. The payloads are random bytes, as encrypted traffic is, so they do
not compress. Each direction runs at the line rate, 10 Gbps of frames, their preambles and gaps included.

`cargo run --release -p ww_tape_example --bin ethernet` records 10 ms of the link into one chain and reads it back:

--8<-- "docs/plots/ethernet_sizes.txt"

10 ms of the link hold 65 663 frames of 23 MB in both directions. The chain takes 1.023 bytes per byte of the frames:
the frames' own chunks take a little less than their bytes, since LZ4 finds repeats among the headers. A decoded
header takes about 7 bytes in the chain, against 12 or 8 bytes as the decoder gives it. A second of the link at its
line rate therefore takes about 2.4 GB on a tape.

## Spreading a port over cores

One track has one producer: its handle encodes and hashes its chunks on that producer's thread. A port's 1.2 GB of
frames a second on one thread is more than one core encodes, hashes and copies. A network card meets the same limit
by spreading flows over receive queues, each served by a core of its own.

The analyser does the same. Each port spreads its flows over its receive queues, and each queue has a capture thread,
a decoder thread and its own three tracks. The test `tests/ethernet.rs` checks that four queues hold exactly the frames
of one, at the same ticks. A reader merges the queues of a port by time, as it merges any tracks.

All tracks may go through one [`TapeMux`], or each port through a mux of its own, into a chain of its own. A mux places
the chunks of all its handles on one writing thread, so a second mux adds a second writing thread and a second chain.
Each chain can live on a disk of its own.

## How fast it records

`just bench-ethernet` records 2 s of the link at full speed. The threads take the frames as fast as they can, not at
the link's pace, so a run says how much of the link the analyser keeps up with. It runs once per number of queues and
muxes into a store that keeps no bytes, so the encoding and the writing thread are measured, not a disk. It then
writes chains of piece files on the machine's disk.

--8<-- "docs/plots/bench_ethernet_writer.txt"

--8<-- "docs/plots/bench_cases_machine.txt"

The column "Of the link" is the bits recorded a second against both directions at the line rate. A value of 1 or more
keeps up with the link. The waits count the times a handle found the mux's queue full: the writing thread, or the disk
behind it, fell behind the producers.

Without a disk, the limit is the cores: each queue's threads encode, compress and hash their frames, and the queues
spread that work. With a disk, the limit is the disk. A chain at the line rate writes about 2.4 GB a second, which
asks for a mux per port and a fast disk under each chain.

On the machine of the table, four receive queues a port keep up with the link when no disk is behind them, and one queue
a port falls short. Its disk keeps only a small part of the link. Such a machine records every frame into memory for a
short capture, or a part of the traffic to disk through a filter on the capture threads.
