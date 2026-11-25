---
level: advanced
---

# A producer's own chunks

Some [[producers|producer]] keep their own open [[chunk]]. An application may encode its [[items]] itself and show the
items not yet placed in its own views, with an event tree of its own. Such a producer sends each finished chunk
through a [`ChunkSender`]. It still wants the [[summary]] tree that a typed handle writes, and it wants to know where
each chunk landed. For example, a scope application keeps the samples of its screen in a chunk of its own and sends
the chunk when the screen fills.

## Leaves, not summary blocks

A summary tree must end in the [[piece]] that holds its items, and only the [[writing thread]] knows where a piece
ends. So the producer sends the chunk's summary leaves, never finished summary blocks. The leaves summarise the chunk's
items, and the writing thread joins them into the track's tree. It ends the tree before each [[cut]] and at the
close.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/own_chunks.svg"
<figcaption>A producer's own chunks. Each chunk goes with its leaves to the writing thread, which places it, builds
the summary tree and ends the tree before the cut. The receipt names where the chunk landed.</figcaption>
</figure>

A sender from [`TapeMux::summarized_chunks`] takes each chunk with its [`Leaves`] through
[`ChunkSender::send_placed`]. The producer computes the leaves on its own thread, from the chunk's items:

- for an events track, from the runs that [`ChunkSender::event_runs`] gives;
- for a series, from [`ChunkSender::sample_leaves`].

Given the same items, such a sender writes the bytes an [`EventsHandle`] or a [`GridHandle`] writes. A test checks
it, also after a crash at every block boundary:

```rust
--8<-- "ww_tape_tests/tests/it/mux_leaves.rs:send_leaves"
```

A resumed mux rebuilds the tree from the chunks the piece kept. The producer may keep its own tree for its views.

## Where a chunk landed

The receipt that [`ChunkSender::send_placed`] gives is a [`Placing`]. It resolves to the chunk's [`Placement`] once
the writing thread has placed the chunk. The placement says:

- the chunk's track, its first item and its count of items;
- which piece holds the chunk, and where the piece starts in its file;
- the chunk's offset and length in the piece;
- the chunk's unit hash.

The block is in the piece's file by then. A [[reader]] in the same process reads the chunk back at once, before any
sync. The producer can then drop its own copy of the items:

```rust
--8<-- "ww_tape_tests/tests/it/mux_placing.rs:read_placement"
```

## Typed items in every piece

A cut carries the [[definitions]] of the tracks, but not a typed item such as a run's description. A piece read
alone should still name the run of every track it holds. [`TapeMux::put_item`] therefore puts the item on track 0
and holds it for later pieces, as its [`ItemCarry`] says:

| `ItemCarry` | The item goes into |
|---|---|
| `Once` | the piece it is put in |
| `Every` | that piece and each piece a later cut starts |
| `Owner(owner)` | that piece and each later piece that carries or writes a track of that owner |

A piece whose head carries none of the owner's tracks gets the item at the owner's first data there.
[`TapeMux::release_item`] ends the carry, and carrying never cuts.

An item that a new piece has no room for is refused there and counted in [`MuxStats::refused`]. The next piece gets
it. The mux holds at most [`HELD_ITEMS_MAX`] items and [`HELD_ITEM_BYTES_MAX`] bytes of them, and refuses one more
before it writes anything.

In a chain that several owners write, [`ChainFollower::track_of`] follows one owner's track. It passes over the chunks
that the other owners wrote on a track of the same id.

## Tracks without data

The writer puts a track's definition before the track's first block of data. A track that never gets data is then
in no piece. [`TapeMux::put_definitions`] puts the definitions of such tracks now, behind the units sent before:
each track's clock chain, maps, bundles and its own definition.

```rust
--8<-- "ww_tape_tests/tests/it/mux_definitions.rs:put_definitions"
```

A definition that lies [[near]] behind is not written again, and a track defined twice gets its copy. The mux holds
the tracks and puts them again in each piece a later cut starts, never cutting for them. A resumed mux holds none, so
put them again after the resume.
