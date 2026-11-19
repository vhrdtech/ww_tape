# Sessions, pieces and chains

This page names the ways blocks group together in a recording, and shows one file from start to end. It uses the
words of [Tape concepts](concepts.md).

## Sessions, pieces, chains and owners

Four words name four different groupings of blocks. They are easy to confuse, and each gets an example.

### Sessions

A **session** is one writer's unbroken run of appends. It runs from a producer's start to its stop, or from a
device's boot to its power-off.

Every block of a session carries the same 32-bit **session tag** in its header.

Each block also carries a **sequence number**, which counts the session's blocks from 0 upward. A gap in the sequence numbers
says how many blocks are missing.

The session tag tells a reader which blocks belong to the current recording. It also tells which blocks are stale
leftovers of an older recording on the same Secure Digital (SD) card.

### Pieces and chains

A **piece** is a complete file in the tape sense. It starts with a piece head, and holds the data with its definitions
and state near it.

At its end come three blocks: a **final index**, a **close** block and a **tail**. A [later
section](#index-close-and-tail) explains them.

A piece reads alone, because the writer measures [near](concepts.md#state-near-and-copies) inside the piece.

Every definition that a piece's data needs lies in that piece. The writer splits a long recording into several pieces,
so that each piece stays a bounded size.

A **chain** is the pieces of one recording, in order. Each piece head names the piece before it by its content hash.

A reader can therefore check that no piece is missing or swapped.

### Owners

An **owner id** is a 32-byte id in every piece head, close and tail. It names what the pieces belong to.

`ww_tape` doesn't interpret the owner id; the application gives it a meaning. One owner may have several chains, such
as one for events and one for a high-rate stream.

An application may add groupings of its own on top. For example, a recorder application can keep a **project**, which
the user creates, opens and closes.

A project holds many runs of many producers. The recorder can write each stretch from open to close as a new session
in a new piece.

The recorder gives every piece the project's id as its owner id. The project and its runs are the application's
words; `ww_tape` sees only sessions, pieces, chains and an opaque owner id.

| Word | Who decides it | Example |
|---|---|---|
| session | the writer, at each start | one boot of a data logger |
| piece | the writer, by size or by hand | `00000003.wwt`, about 256 MiB of one recording |
| chain | the pieces of one recording | pieces 0 to 7 of one capture |
| owner id | the application | a project's id |

## Cuts

A **cut** ends the current piece and starts the next piece of the chain. The writer writes the piece's final index,
close and tail, then a new piece head.

In the new piece, the writer puts the layout and the producer first. It puts every definition and state item again at
its first use, since nothing in the old piece counts as near.

A chain cuts by its policy (size, chunk count or age) only before a chunk. It also cuts before a write for which the
piece's index has no room left, and wherever the application asks.

A slow input never waits for a cut. Records reach the file in records blocks as they come, and a reader of the
unclosed piece reads them at once.

A **sync policy** bounds what a crash can lose. By default, the writer syncs waiting blocks once a second has passed
since the last sync.

The writer syncs them at the next block, or at a call of `TapeWriter::tick`. A cut only bounds how large one file
grows.

For example, a producer that writes one log line a minute and calls `tick` regularly has each line on disk about a
second later. That holds whether the writer cuts the piece after an hour or after a week.

## Index, close and tail

While the writer writes a piece, it places a **checkpoint**, an index block, about every 4 MiB. Each checkpoint lists
the blocks placed since the one before, and points back at it.

A reader of a file that the writer never closed finds the newest checkpoint near the end. It therefore knows every
block before that checkpoint without reading them.

At the end of a piece, the writer places the final index, which lists every block sorted by track. Then it places
the close record.

The close holds a hash of every byte before it, made with BLAKE3 (a cryptographic hash function). It also holds
per-track totals, and where the final index is.

Last comes the tail, 164 bytes that give the piece's length and where its close is. A closed piece is a multiple
of 4 KiB long, and the writer pads the close record with zeros to make it so.

## One file, start to end

The diagram shows one closed piece, with its start at the top.

```text
 piece start ─► ┌─────────────────────────────┐
                │ piece head                  │ ids, the previous piece's hash
                ├─────────────────────────────┤
                │ puts           (track 0)    │ layout and producer
                ├─────────────────────────────┤
                │ puts           (track 0)    │ clocks, maps, tracks 1-3
                │ events chunk   (track 3)    │ first data block
                │ records        (tracks 1,2) │ small tracks, many per block
                │ puts           (track 0)    │ track 5's definition
                │ series chunk   (track 5)    │
                │ puts           (track 0, 6) │ track 6's definition and state
                │ summary        (track 3)    │
                │ ...                         │
                │ copies         (track 0, 6) │ at a use 1 MiB after the first
                │ index (checkpoint) ◄──┐     │ every 4 MiB: the blocks placed
                │ ...                   │     │ since the previous checkpoint,
                │ index (checkpoint) ───┘     │ and a pointer back to it
                │ ...                         │
                │ puts           (track 0)    │ a definition 32 MiB on, still
                │ records        (track 1)    │ in use
                │ ...                         │
                ├─────────────────────────────┤
                │ final index    ◄──────┐     │ every block again, by track
                │ close         ◄──┐   │     │ BLAKE3 hash of all bytes before it,
                │                   │   │     │ zero padding to the 4 KiB end
                │ tail (164 B) ─────┴───┘     │ piece length, where the close is
   end of file ─► └─────────────────────────────┘ a multiple of 4 KiB from the start
```

A reader of this file starts at the bottom. It reads the tail, then the close, then the final index's part for the
tracks it wants.

Then the reader reads only the blocks it needs. [Reading a tape](reading.md) follows that path.

The figure below draws a small piece of 12 KiB to scale, and shows how its end falls on the 4 KiB grid. The final
index, the close and the tail take the bytes after the data.

The close's zero padding fills the rest, up to the boundary.

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/piece.svg"
<figcaption>A closed piece of a chain, each block to scale on rows of 1 KiB: four records blocks and four events
chunks with the puts they need, two checkpoints, the final index, the close with its padding, and the 164-byte tail
ending on 12 KiB. Drawn by <code>ww_tape_figures</code> from a piece <code>PieceChain</code> wrote.</figcaption>
</figure>

## Addressing one item

A reader addresses an item by its track key and its [item number](concepts.md#records-blocks-and-chunks), never by a
byte offset. The track key names the track in every file of the recording, whatever number the track has there.

The item number counts the track's items over the whole recording. Each piece head says where each track's numbers
continue.

Offsets change when pieces join into one file, or when a copy of a file keeps only some of its tracks. The address
doesn't change.

[Pieces and files](pieces_and_files.md#addressing-one-item) explains how a reader turns the address into bytes.

## Nothing is filled in later

A writer never goes back to change bytes it already wrote. Every block is final once placed, and the file only grows.

This rule gives the format its shape:

- The piece's length and the place of its index are in the tail at the end, not in a header field at the start.
- Checkpoints point back at the checkpoint before them, never forward at one not yet written.
- A tail can't name the next piece's hash, which doesn't exist yet. It names the next piece's id instead, derived from
  its own id and hash, and the next piece head repeats that id.
- A new definition is a new record. The old one stays in the file, and only readers after the new one see it replaced.
- A copy of a state item is written at a use, never scheduled ahead. Nothing is left pending when the writer stops.
- A crash costs only the bytes not yet synced. No header exists that would be wrong after it.

Append-only writing suits every medium the format targets. A NAND (not and) flash page takes one program, and an SD
card is fastest in long sequential writes.

A tool can copy a file that is only appended while the file grows.

## Where to go next

- [Records, tracks and clocks](records_and_clocks.md): how a tape stores time.
- [Pieces and files](pieces_and_files.md): chains, many pieces in one file, copies, and addressing one item.
- [Reading a tape](reading.md): opening a file, finding blocks, state in force at a place, damage.
