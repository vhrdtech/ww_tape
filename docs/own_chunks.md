---
level: advanced
---

# A producer's own chunks

Some producers keep their own open chunk. An application may encode its items itself and show the items not yet
placed in its own views. It may also keep its own event tree for them. Such a producer sends each finished chunk
through a [`ChunkSender`] and still wants the summary tree a typed handle writes. This page says how it gets that
tree, where its chunks land, and how its typed items reach every piece.

## Leaves, not summary blocks

A summary tree must end in the piece its items are in, and only the writing thread knows where it cuts. So the
producer sends the chunk's summary leaves, never finished summary blocks. A sender from
[`TapeMux::summarized_chunks`] takes each chunk with its [`Leaves`] through [`ChunkSender::send_placed`].

The producer computes the leaves on its own thread. For an events track they come from the runs
[`ChunkSender::event_runs`] gives, fed the chunk's items. For a series they come from [`ChunkSender::sample_leaves`].
The writing thread joins them into the track's tree and ends it before each cut and at the close. A resumed mux
rebuilds it from the chunks the piece kept.

An application that wrote the finished blocks of its own tree into the piece now feeds the same items into these
runs or leaves instead. It may keep its tree for its own views. Given the same items, such a sender writes the
bytes an [`EventsHandle`] or a [`GridHandle`] writes. A test checks it, also after a crash at every block boundary:

```rs
--8<-- "ww_tape_tests/tests/it/mux_leaves.rs:send_leaves"
```

## Where a chunk landed

The receipt that [`ChunkSender::send_placed`] gives is a [`Placing`]. It resolves to the chunk's [`Placement`] once
the writing thread placed the chunk. The placement names the piece and where the piece starts in its file. It also
gives the chunk's offset and length in the piece, and its unit hash.

The block is in the piece's file by then. A reader in the same process reads the chunk back at once, before any
sync. The producer can then drop its own copy of the chunk's items.

## Typed items in every piece

A cut carries the definitions of the tracks, but not typed items such as a run's description.
[`TapeMux::put_item`] puts one on track 0 and holds it for later pieces as its [`ItemCarry`] says. An item held for
an owner goes into each new piece that carries a track of that owner. A piece whose head carries none of them gets it
at the owner's first data there. A piece read alone thus names the run of every track it holds.

[`TapeMux::release_item`] ends the carry. Carrying never cuts. An item a new piece has no room for (its state entries are full) is refused there and counted
in [`MuxStats::refused`]; the next piece gets it. The mux holds at most [`HELD_ITEMS_MAX`] items and
[`HELD_ITEM_BYTES_MAX`] bytes of them: one more is refused before anything is written.

## Tracks without data

The writer puts a track's definition before the track's first block of data. A track that never gets data is then
in no piece. [`TapeMux::put_definitions`] puts the definitions of such tracks now, behind the units sent before: each
track's clock chain, maps, bundles and its own definition.

A definition that lies near behind is not written again, and a track defined twice gets its copy. The mux holds the
tracks and puts them again in each piece a later cut starts, never cutting for them. A resumed mux holds none: put
them again after the resume.

In a chain that several owners write, [`ChainFollower::track_of`] follows one owner's track. It passes over the chunks
the other owners wrote on a track of the same id.
