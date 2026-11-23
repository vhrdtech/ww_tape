---
level: advanced
---

# Live feeds and relays

A tape need not be finished to be read. Its [[blocks]] can go to a viewer while they are written, over a link to a host,
or onto a raw card. How does a [[reader]] follow a growing tape? How does a program record another [[producer]]'s tape
into a file of its own? The examples relay a tape that the drive's board writes itself into the host's chain.

## Reading while the writer writes

A [[live feed]] is a tape passed on block by block while it is written. Every block checks itself and the writer never
changes a byte it placed, so a reader may take each block as it comes. A reader that joins late finds the
[[definitions]] and [[state]] it needs [[near]] behind, as [Definitions and
state](definitions.md#why-a-reader-needs-state-close-behind) shows.

In one process, a [`MemSink`] holds the tape, and a [`TapeReader`] reads it while the writer appends. A recorder
that writes through a [[mux]] gives its readers a [`ProgressWatch`], which wakes them each time the [[writing thread]]
places more. [Writing from many threads](writing_thread.md#readers-while-the-mux-writes) shows it. A [`MemSink`] of a
[[chain]] reserves the room of a whole piece when the piece starts: the chain's cut size plus 1 MiB. A reader's read
therefore never waits while the bytes move into a bigger buffer.

A piece that was closed and opened again grows after its close. [Reading a growing piece](growing_pieces.md) explains
what a reader sees then, and how it notices the growth.

## Relays

A [[relay]] is a program that takes a live producer's blocks and writes them into a tape of its own. A host that
receives a device's tape over a link and records it to disk is one. A capture tool that keeps a file of a live feed is
another.

The producer's stream carries its own [[puts]], placed by its own [[near]] rule in its own stream. They do not fit the
relay's file, which may start later, be [[cut]] elsewhere, or have lost blocks. [`TapeWriter::relay_block`] therefore
takes the definitions of a block of puts into the relay's own [`struct@Definitions`]. It puts the block's state records by the
relay's own rule ([`TapeWriter::put_block`]). Every other block passes as data, byte for byte.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/relay.svg"
<figcaption>A device's blocks relayed. Its data blocks pass byte for byte. Its puts go into the relay's definitions, and
the relay puts what each of its own pieces needs.</figcaption>
</figure>

The relay needs no knowledge of the producer's tracks. Its file has every definition near its use, like any other file.
The drive's host relays the board's tape into a chain cut every 15 minutes. It writes through a mux, whose
[`RelayHandle`] sends each block to the writing thread. That thread calls [`TapeWriter::relay_block`] and cuts by
the chain's policy:

```rs
--8<-- "examples/example_drive/src/writing.rs:relay"
```

??? example "What the example uses here: `board_tape`, `options`"

    ```rs
    --8<-- "examples/example_drive/src/writing.rs:board_tape_item"

    --8<-- "examples/example_drive/src/record.rs:options_item"
    ```

The board writes its hour as one tape of 116 MB, with about 13700 data blocks. It keeps near at 1 MiB, since a viewer
may join its feed at any time, and so writes 726 blocks of puts, 86 kB. The relayed chain has 4 pieces, 115 MB, with 116
blocks of puts, 11 kB. Its data blocks are the board's, byte for byte.

## Adopting blocks on raw media

On a [[raw medium]], a piece must be one [[session]], as [Where the data ends on a raw
card](raw_media.md#sessions-and-stale-data) explains. A block from another session reads there as [[stale data]]. A
writer for raw media therefore passes a producer's blocks with [`TapeWriter::adopt_block`]. Through a mux, a
[`RelayHandle`] made with [`RelayMode::Adopt`] does the same.

The call renumbers each block into the writer's own session and returns where it landed, or `None` for a block of puts.
A jump in the producer's [[sequence numbers]] stays a jump in the writer's. A reader therefore still sees that blocks
were lost on the way. A block passed with [`TapeWriter::write_block`] keeps its session, which suits a file but not a
card.

## Without the standard library

The writer, the chain and the resume need only an allocator. Firmware or a browser therefore writes tapes on a [[sink]]
of its own. Without the standard library, a new writer starts without a session and without a clock.

A new writer has no session, because it has no random source, and it refuses to write until it gets one.
[`TapeWriter::set_session`] gives it one, such as [`SessionTag::from_seed`] over a device id and a boot counter. A
[[piece head]] gives it one too: [`TapeWriter::begin_piece`], and every piece of a chain.

A new writer also has no clock, so no sync, [[checkpoint]] or cut falls due by time. [`WriterOptions::clock`] gives it
one ([Files and syncing](files.md#the-writers-clock)).
