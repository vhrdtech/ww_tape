---
level: basic
---

# Writing from many threads

A recorder rarely has one source. A protocol analyser reads several links, and a test bench samples a current while
a board sends log lines. Each source has a thread or a task of its own, and none of them should wait for the disk. How
do they write one [[tape]] together? They hand their data to a [[mux]], which owns the [[writer]] on a thread of its
own.

## A writing thread and its handles

A [`TapeMux`] owns a [`PieceChain`] and its [[definitions]] on the [[writing thread]], a thread that does nothing but
write. Producers never touch the writer. Each holds a [[handle]], which takes its data, makes finished units of it and
sends them into one queue. The writing thread takes the units from the queue in the order they came and places them.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/mux.svg"
<figcaption>The drive's four sources, each on a thread of its own with its handle. The typed handles do the costly
passes on their own threads. One queue, bounded by bytes, carries every unit to the writing thread, which does every
chore of the chain.</figcaption>
</figure>

Each kind of data has its handle:

| Handle | Takes | Sends |
|---|---|---|
| [`StateHandle`] | a [[state item]] on change, or a key's value on a keyed state track | a put |
| [`GroupHandle`] | puts of several state tracks that belong together | one group of puts |
| [`RecordsHandle`] | a [[records block]] built with a [`BlockWriter`] | the block |
| [`EventsHandle`] | items of one events [[track]], one by one or in batches | an [[events chunk]] it encodes |
| [`GridHandle`] | time steps of one series track: its [[samples]], or the cells of its [[tiles]] | a [[series chunk]] per tile, which it encodes |
| [`ChunkSender`] | a [[chunk]] the caller encoded | the chunk |
| [`BytesHandle`] | a value of any length, written in pieces | its parts, then its seal |
| [`RelayHandle`] | another [[producer]]'s blocks | each block, as a [[relay]] passes it |

The writing thread does every chore the writer needs. It [[puts]] definitions and state where blocks need them,
[[cuts|cut]] by the chain's policy and syncs by the [[sync policy]]. It places [[checkpoints]] and closes the chain.
Handles never pass definitions: the mux holds them, and [`TapeMux::define_track`] gives a track its number.

A shared lock around the writer would serialize the producers instead. A slow sync to the disk would then block every
producer that waits for the lock. With a writing thread, a sync blocks only that thread. A producer waits only when
the queue is full, as [Back-pressure, errors and speed](mux_queue.md) explains.

## A small mux

A mux this small still shows the shape. Two sensors push pings on their own events track, each in one call to
[`EventsHandle::push_all`]. A third pushes levels on a series track with [`GridHandle::push`]. Each thread holds
its own handle into one chain in memory, joins, and the mux closes:

```rust
--8<-- "examples/example_drive/src/producers.rs:threads"
```

Reading the chain back after the close shows that every item of every thread arrived.

Every blocking call has an async twin, named with `_async`. The same three sources, as tasks on tokio instead of
threads, push and close the same way:

```rust
--8<-- "examples/example_drive/tests/producers.rs:tasks"
```

`smol::spawn` schedules the same futures the same way, and neither runtime is a dependency of `ww_tape_mux`.

## The drive with one thread per source

The drive's hour ([What a tape holds](tape.md)) shows the pattern with four threads. The program first starts a mux
on a chain in memory and defines the clocks and tracks on it:

```rust
--8<-- "examples/example_drive/src/handles.rs:create"
```

??? example "What the example uses here: `define_on_mux`"

    ```rs
    --8<-- "examples/example_drive/src/defs.rs:define_on_mux"
    ```

One thread puts the mode each time it changes. The [[state track]] needs no put at each use. The writer carries the
newest item into each new piece and writes copies by its own rule.

```rust
--8<-- "examples/example_drive/src/handles.rs:mode"
```

Another thread pushes the CAN frames of `can0` in batches of 4096 into an [`EventsHandle`]. A third pushes a second of
phase current at a time into a [`GridHandle`]. Each handle ends with `finish`, which completes its open chunk and
sends it:

```rust
--8<-- "examples/example_drive/src/handles.rs:can0"

--8<-- "examples/example_drive/src/handles.rs:phase_a"
```

A fourth thread sends the log lines and temperatures as records blocks through a [`RecordsHandle`]. When every thread
is done, the program closes the mux, which places what was sent and closes the chain:

```rust
--8<-- "examples/example_drive/src/handles.rs:close"
```

Which unit reaches the queue first depends on the threads, so the bytes of the chain differ from run to run. What
each track holds does not. A test decodes every track and compares it with the drive's recording through one thread.
That recording drives the same handles from one thread in a fixed order, so it writes the same bytes every run.

## Typed handles encode on their own thread

An [`EventsHandle`] or a [`GridHandle`] keeps an open chunk. A push adds items to it, and the handle completes the
chunk when it is full: 4096 items or 1 MiB of payload by default. Completing encodes the chunk's [[columns]] and hashes
its payload with [[BLAKE3]], the costliest pass of a write. It also computes the leaves of the track's [[summary]], such
as a series' statistics per group of samples.

All of this runs on the producer's thread, so the work spreads over the producers. The writing thread places the
chunk with its hash given, and builds the summary levels above the leaves. It keeps one summary tree per track and
[[piece]] and ends it before each cut.

A series track with [[fixed axes]] keeps a chunk per [[tile]]. Its [`GridHandle`] splits each time step into the open
chunk of each tile, by these rules:

- It takes a time step of the whole grid, or of a box of tiles ([`TapeMux::grid_box`]).
- The tiles' chunks hold the same time steps and are completed together.
- Several producers may each write a box of their own, and a tile that no box holds is never written.
- A tile's time step must fit a chunk and a block. A handle refuses a larger one with [`CallError::StepTooLarge`] and
  never splits it.

A producer may go quiet with a chunk half full. The writing thread then completes the chunk itself once its first item
has waited the handle's latency, 1 second by default ([`EventsOptions::latency`]). A quiet source's items thus reach the
tape within that time, behind every unit the handle sent before.

Pushing items one at a time costs a call per item. [`EventsHandle::push_all`] takes a batch, which the handle encodes
in one pass. The bytes are the same either way.

The other handles take units that are already whole. A [`ChunkSender`] takes a chunk that its caller encoded, as the
page [Event times and their codecs](columns.md) shows. A [`RecordsHandle`] takes a records block, and the writing
thread numbers it as the writer's next block. A [`RelayHandle`] takes another producer's blocks, as [Live feeds and
relays](live_feeds.md#relays) explains.

A producer that encodes its own chunks may still want a typed handle's summaries and to know where each chunk lands.
[A producer's own chunks](own_chunks.md) shows how.

## A value of any length

Some data is one value made elsewhere and read whole, such as a firmware image or a design file. A bytes track holds
such values. Its [`BytesHandle`] streams a value into parts as the bytes come, so the value never sits whole in
memory. The last part is the seal, which holds the value's length and its [[BLAKE3]] hash:

```rust
--8<-- "ww_tape_tests/tests/it/bytes.rs:value"
```

A value may span a cut, as the one above does, and a reader still reads it whole with its hash checked. A value torn
by a crash, or one that lacks its seal, reads as absent. A bytes track takes one handle at a time, so the parts of two
values never mix.

## The order units land in

One handle's units land in the order it sent them: its chunks, its puts and its blocks. Units of different handles
land in the order they reached the queue, which need not be the order of their times. A chunk of samples may land
after a records block whose times are later.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/mux_order.svg"
<figcaption>Three handles send into one queue. The writing thread places the units in the order they came, and a cut
falls between two units, never inside one.</figcaption>
</figure>

The format does not need time order across tracks. Each track is indexed on its own, a state is looked up by its
[[tick]], and a reader merges tracks by time. One order is fixed: a definition lands before any data that names it,
since a handle is made only once the mux holds its track.

## Cuts and closes under live handles

The writing thread cuts between units, never inside one. A chunk therefore lies whole in one piece, and a handle never
notices a cut. The chain cuts by its [`CutPolicy`] before it places a chunk, and [`TapeMux::cut`] cuts once every unit
sent before is placed. Item numbers go on across the cut, as [Pieces and chains](pieces.md#what-a-cut-writes) shows.

[`TapeMux::close`] refuses everything sent after it: every handle's next call returns [`MuxError::Closed`]. The units
sent before are placed, the summary trees are ended, and the chain is closed. The close does not wait for the handles
to be dropped, so one forgotten handle cannot hold it up.

A mux dropped without a close places what was sent, syncs, and leaves the last piece unclosed, as a crash would.
[`TapeMux::resume`] goes on with the chain and rebuilds the summary tree of the piece from the chunks it kept. [When the
power fails](crashes.md) explains the resume.

A producer may run faster than the disk, and a write may fail. [Back-pressure, errors and speed](mux_queue.md) shows
what the queue and the handles do then.

