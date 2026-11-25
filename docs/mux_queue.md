---
level: basic
---

# Back-pressure, errors and speed

A producer may outrun the disk, and a disk may fail under a running recorder. A [[mux]] must then decide whether a
producer waits or loses data, and how a producer learns of a failure. What does a [[handle]] do when the queue is
full or the chain has failed? And how fast does a mux write when nothing goes wrong?

## The queue and back-pressure

The queue between the handles and the [[writing thread]] is bounded by bytes, not by units. A put is small and a
chunk of samples may be large, so a count of units would bound nothing. A unit takes its bytes from the budget when
it is sent and gives them back when it is placed. The budget is [`MuxOptions::queue_bytes`], 64 MiB by default.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/queue.svg"
<figcaption>The queue as a budget of bytes. Large chunks take most of it and puts almost none. Once it is spent, each
handle's pressure decides whether it waits, fails or drops.</figcaption>
</figure>

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

```rust
--8<-- "ww_tape_tests/tests/it/mux.rs:pressure"
```

## Errors reach every handle

A producer learns of an error where it can act on it. Three kinds of error surface in three places:

| Kind | Example | Where it surfaces |
|---|---|---|
| refused at the call | an item too long, a chunk of another track | the call returns [`MuxError::Call`], and nothing was sent |
| refused by the writer | a block on a track the definitions don't hold | the handle's next call, flush or sync returns [`MuxError::Refused`] |
| the mux failed | a write or a sync that failed | every call of every handle, and the close, return [`MuxError::Failed`] |

A failed write stops the chain, since the writer never retries. The test below lets the sink fail after one block:

```rust
--8<-- "ww_tape_tests/tests/it/mux.rs:failed"
```

A send returns once the queue holds the unit, not once the unit is placed. A producer that needs to know more asks
its handle, which offers three points to wait for:

- A send returns once the queue holds the unit.
- `flush` returns once every unit of the handle is placed.
- `sync` returns once those units are durable on the [[store]].

## Threads and tasks

Every call that may wait has an async twin, named with `_async`: [`EventsHandle::push_async`],
[`ChunkSender::send_async`], [`TapeMux::close_async`] and the rest. A task awaits room in the queue as a thread blocks
for it, and both wait in one line. The writing thread wakes them, so the crate needs no async runtime and depends on
none. The writing thread itself stays an operating system thread, since its writes and syncs block.

The futures are cancel-safe. A send dropped while it waits leaves the line and sends nothing. A typed handle's push
dropped while it waits keeps the chunk it completed. The handle sends that chunk first on its next call, so no item is
lost. The test below ends an events handle and the mux from a task:

```rust
--8<-- "ww_tape_tests/tests/it/mux_async.rs:push_async"
```

A typed handle's drop sends its open chunk and blocks while it does. Async code therefore ends a handle with
`finish_async`.

## Readers while the mux writes

A reader need not wait for the close. Where it runs decides how it follows the chain:

- A reader in the writer's own process waits on a [`ProgressWatch`] from [`TapeMux::watch`]. The writing thread wakes
  it each time it places more. The watch tells the piece, its bytes written and synced, and the newest chunk of each
  track.
- A reader in another process follows the chain's files with a [[follower]]. A [`ChainFollower`] hands out each block
  once it checks, and a [`TrackFollower`] gives one track's items as their chunks land.

[Reading a growing piece](growing_pieces.md#following-a-chain-while-it-is-written) explains both.

## Several chains, one writer each

One mux writes one [[chain]], and a process may run several. The radio capture of [Series](series.md) writes its
samples and its decoded bursts into two chains, each from a mux of its own. Neither writing thread waits for the
other's writes, and a [`TimeMerge`] gives a reader the items of both in time order.

Each chain has one writer. A file sink locks the file it writes, so a second writer of the same chain is refused. The
lock holds against another process and against the same one.

Data from other processes therefore travels to the writing process over a channel of the program's own, such as a
socket or shared memory. A completed chunk or a block is plain bytes, and the writing process hands each unit to the
handle of its kind:

- a block of another [[producer]] to a [`RelayHandle`];
- a chunk that another process encoded to a [`ChunkSender`].

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
