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
| [`StateHandle`] | a [[state item]], on change | a put |
| [`RecordsHandle`] | a [[records block]] built with a [`BlockWriter`] | the block |
| [`EventsHandle`] | items of one events [[track]], one by one or in batches | an [[events chunk]] it encodes |
| [`SeriesHandle`] | [[samples]] of one series track | a [[series chunk]] it encodes |
| [`ChunkSender`] | a [[chunk]] the caller encoded | the chunk |
| [`RelayHandle`] | another [[producer]]'s blocks | each block, as a [[relay]] passes it |

The writing thread does every chore the writer needs. It [[puts]] definitions and state where blocks need them,
[[cuts|cut]] by the chain's policy and syncs by the [[sync policy]]. It places [[checkpoints]] and closes the chain.
Handles never pass definitions: the mux holds them, and [`TapeMux::define_track`] gives a track its number.

A shared lock around the writer would serialize the producers instead. A slow sync to the disk would then block every
producer that waits for the lock. With a writing thread, a sync blocks only that thread. A producer waits only when
the queue is full, and the thread wakes by itself when a sync or a checkpoint falls due.

## A small mux

A mux this small still shows the shape. Two sensors push pings on their own events track, each in one call to
[`EventsHandle::push_all`]. A third pushes levels on a series track with [`SeriesHandle::push`]. Each thread holds
its own handle into one chain in memory, joins, and the mux closes:

```rs
--8<-- "example_drive/src/producers.rs:threads"
```

Reading the chain back after the close shows every item of every thread arrived.

Every blocking call has an async twin, named with `_async`. The same three sources, as tasks on tokio instead of
threads, push and close the same way:

```rs
--8<-- "example_drive/tests/producers.rs:tasks"
```

`smol::spawn` schedules the same futures the same way; neither runtime is a dependency of `ww_tape_mux`.

## The drive with one thread per source

The drive's hour ([What a tape holds](tape.md)) shows the pattern with four threads. The program first starts a mux on a chain in memory and
defines the clocks and tracks on it:

```rs
--8<-- "example_drive/src/handles.rs:create"
```

??? example "What the example uses here: `define_on_mux`"

    ```rs
    --8<-- "example_drive/src/defs.rs:define_on_mux"
    ```

One thread puts the mode each time it changes. The [[state track]] needs no put at each use. The writer carries the
newest item into each new piece and writes copies by its own rule.

```rs
--8<-- "example_drive/src/handles.rs:mode"
```

Another thread pushes the CAN frames of `can0` in batches of 4096 into an [`EventsHandle`]. A third pushes a second of
phase current at a time into a [`SeriesHandle`]. Each handle ends with `finish`, which seals its open chunk and sends
it:

```rs
--8<-- "example_drive/src/handles.rs:can0"

--8<-- "example_drive/src/handles.rs:phase_a"
```

A fourth thread sends the log lines and temperatures as records blocks through a [`RecordsHandle`]. When every thread
is done, the program closes the mux, which places what was sent and closes the chain:

```rs
--8<-- "example_drive/src/handles.rs:close"
```

Which unit reaches the queue first depends on the threads, so the bytes of the chain differ from run to run. What
each track holds does not. A test decodes every track and compares it with the drive's recording through one thread.
That recording drives the same handles from one thread in a fixed order, so it writes the same bytes every run.

## Typed handles encode on their own thread

An [`EventsHandle`] or a [`SeriesHandle`] keeps an open chunk. A push adds items to it, and the handle seals the chunk
when it is full: 4096 items or 1 MiB of payload by default. Sealing encodes the chunk's [[columns]] and hashes its
payload with [[BLAKE3]], the costliest pass of a write. It also computes the leaves of the track's [[summary]], such
as a series' statistics per group of samples.

All of this runs on the producer's thread, so the work spreads over the producers. The writing thread places the
chunk with its hash given, and builds the summary levels above the leaves. It keeps one summary tree per track and
[[piece]] and ends it before each cut.

A producer may go quiet with a chunk half full. The writing thread then seals the chunk itself once its first item has
waited the handle's latency, 1 second by default ([`EventsOptions::latency`]). A quiet source's items thus reach the
tape within that time, behind every unit the handle sent before.

Pushing items one at a time costs a call per item. [`EventsHandle::push_all`] takes a batch, which the handle encodes
in one pass. The bytes are the same either way.

The other handles take units that are already whole. A [`ChunkSender`] takes a chunk that its caller encoded, as the
page [Event times and their codecs](columns.md) shows. A [`RecordsHandle`] takes a records block, and the writing
thread numbers it as the writer's next block. A [`RelayHandle`] takes another producer's blocks, as [Live feeds and
relays](live_feeds.md#relays) explains.

## The queue and back-pressure

The queue between the handles and the writing thread is bounded by bytes, not by units. A put is small and a chunk
of samples may be large, so a count of units would bound nothing. A unit takes its bytes from the
budget when it is sent and gives them back when it is placed. The budget is [`MuxOptions::queue_bytes`], 64 MiB by
default.

When the budget is spent, a handle meets [[back-pressure]]: it cannot send until the writing thread catches up. Each
handle chooses what to do then with its [`Pressure`]:

| Pressure | A full queue | Suits |
|---|---|---|
| `Wait`, the default | blocks the call until there is room; nothing is lost | a recorder that keeps every item |
| `Fail` | returns [`MuxError::Full`] with the unit | a producer that decides itself, such as one that buffers elsewhere |
| `Drop` | drops the unit and counts it | a live monitor that must never block |

Handles that wait are served in the order they started to wait, so a fast producer cannot starve the others. A unit
larger than the whole budget goes alone once the queue is empty.

A dropped chunk leaves a gap in its track's [[item numbers]]. The next chunk starts past the dropped items, so a
[[reader]] sees that items were lost and how many. [`MuxStats`] counts the drops of each track. The test below lets one
handle drop and another fail while the sink stalls:

```rs
--8<-- "ww_tape_tests/tests/it/mux.rs:pressure"
```

## The order units land in

One handle's units land in the order it sent them: its chunks, its puts and its blocks. Units of different handles
land in the order they reached the queue, which need not be the order of their times. A chunk of samples may land
after a records block whose times are later.

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

## Errors reach every handle

A producer learns of an error where it can act on it. Three kinds of error surface in three places:

| Kind | Example | Where it surfaces |
|---|---|---|
| refused at the call | an item too long, a chunk of another track | the call returns [`MuxError::Call`], and nothing was sent |
| refused by the writer | a block on a track the definitions don't hold | the handle's next call, flush or sync returns [`MuxError::Refused`] |
| the mux failed | a write or a sync that failed | every call of every handle, and the close, return [`MuxError::Failed`] |

A failed write stops the chain, since the writer never retries. The test below lets the sink fail after one block:

```rs
--8<-- "ww_tape_tests/tests/it/mux.rs:failed"
```

A send returns once the queue holds the unit, not once the unit is placed. A producer that needs to know more asks
its handle. Its `flush` returns once every unit of the handle is placed, and its `sync` once they are durable on the
[[store]].

## Threads and tasks

Every call that may wait has an async twin, named with `_async`: [`EventsHandle::push_async`],
[`ChunkSender::send_async`], [`TapeMux::close_async`] and the rest. A task awaits room in the queue as a thread blocks
for it, and both wait in one line. The writing thread wakes them, so the crate needs no async runtime and depends on
none. The writing thread itself stays an operating system thread, since its writes and syncs block.

The futures are cancel-safe. A send dropped while it waits leaves the line and sends nothing. A typed handle's push
dropped while it waits keeps the chunk it sealed. The handle sends that chunk first on its next call, so no item is
lost. The test
below ends an events handle and the mux from a task:

```rs
--8<-- "ww_tape_tests/tests/it/mux_async.rs:push_async"
```

A typed handle's drop sends its open chunk and blocks while it does. Async code therefore ends a handle with
`finish_async`.

## Readers while the mux writes

A reader in the writer's own process waits on a [`ProgressWatch`] from [`TapeMux::watch`]. The writing thread wakes it
each time it places more. The watch tells the piece, its bytes written and synced, and the newest chunk of each track.

A reader in another process follows the chain's files with a [[follower]]. A [`ChainFollower`] hands out each block
once it checks, and a [`TrackFollower`] gives one track's items as their chunks land. [Reading a growing
piece](growing_pieces.md#following-a-chain-while-it-is-written) explains both.

## Several chains, one writer each

One mux writes one [[chain]], and a process may run several. The radio capture of [Series](series.md) writes its
samples and its decoded bursts into two chains, each from a mux of its own. Neither writing thread waits for the
other's writes, and a [`TimeMerge`] gives a reader the items of both in time order.

Each chain has one writer. A file sink locks the file it writes, so a second writer of the same chain is refused. The
lock holds against another process and against the same one.

Data from other processes therefore travels to the writing process over a channel of the program's own, such as a
socket or shared memory. A sealed chunk or a block is plain bytes. The writing process hands each unit to a
[`RelayHandle`] or a [`ChunkSender`].

## How fast the mux writes

The bench `handles` writes the same data through a mux and through one thread writing a chain directly. It splits the
data across 1, 4 and 16 tracks, each track a producer thread on the mux. The events are instants with a 16-byte
payload and their summaries:

--8<-- "docs/plots/bench_handles_events.txt"

With one track, the mux reaches 88 % of the direct writer's rate: the queue and the second thread cost a little. A
producer that pushes events one by one reaches only 23 %, which is why a batch is worth making. With 16 tracks, the
producers encode in parallel, and the mux writes 4.6 times as many events a second as the direct writer.

The second table writes a radio capture, 20 million IQ samples a second, packed to 12 bits with summaries:

--8<-- "docs/plots/bench_handles_iq.txt"

With 16 tracks, the mux writes 4.1 times as many samples as the direct writer, 3.2 GB/s. There the writing thread
becomes the limit: 1 400 sends waited for room in the queue. A source faster than one writing thread gets a chain of
its own, and the chains are merged when they are read.

--8<-- "docs/plots/bench_handles_machine.txt"

`just bench-handles --help` lists the bench's settings, and [Sizes and speeds](sizes.md) says how to run the benches
on your own machine.
