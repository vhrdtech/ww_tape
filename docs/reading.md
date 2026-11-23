---
level: basic
---

# Reading a tape

A recording can run to gigabytes, 3.7 GB for a minute of a radio capture. The question asked of it is often about one
second. A [[reader]]
answers it with a few small reads. It opens a file or a [[chain]], finds the [[blocks]] of one [[track]] in a time
range, and looks up the [[state]] in force there. Each step uses what a closed [[piece]] says about itself at its end
and in its index.

The example is a query on the drive's recording for the second around the fault at minute 52 of its hour. The recording
is a chain of four [[pieces]], about 115 MB in all. Every number below comes from [`src/fault.rs`]. This program
runs each step on that recording and counts the bytes it reads. `cargo run -p example_drive` prints them.

## Byte sources

A [[reader]], [`TapeReader`], reads a [[tape]] through a [[byte source]]: anything with a length that reads bytes at an
offset. The trait is [`ByteSource`], and the crate provides it for these:

- a byte slice or a `Vec<u8>`, such as a whole file read into memory;
- a file on disk, through [`FileSource::open`], which reads only what the reader asks for;
- a tape still being written into memory, through [`MemSink::tape`].

An HTTP resource read by range requests can be a byte source too. The reader itself needs no file system and no threads,
so it builds for WebAssembly and runs in a browser or a tool. It needs an allocator, like the writer, and it is not
meant for firmware. A small device reads its own log with [logring](related_books.md#logring-a-log-store-on-the-device).

In the example, the drive's four pieces lie back to back in one file. The program wraps that file in a byte source
that counts each read, and so measures what each step costs.

## Opening from the tails

A [[closed]] [[piece]] describes itself at its end. Its last 164 bytes are the [[tail]], which gives the piece's length
and the offset of its [[close record|close]]. [`TapeReader::open`] therefore starts at the end of the bytes and works
backwards:

1. It reads the tail of the last piece, and from it the piece's start.
2. It reads that piece's close, which lists every track the piece holds with its items and ticks, and its
   [[piece head]].
3. The tail of the piece before lies just before that start, so the reader steps back piece by piece.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/seek.svg"
<figcaption>Above, a closed piece: the reader reads the tail (1), the close (2), the final index's directory (3) and
one track's list, then (4) only the needed blocks. Below, an unclosed piece: the newest checkpoint and its back
pointers list every earlier block, and the reader scans the blocks after it.</figcaption>
</figure>

For the drive's chain, opening reads about 17 KB in 28 reads, and [`TapeReader::pieces`] lists four pieces. Each
[`PieceInfo`] holds the piece's start and end, its head and its close with a row per track. The row of `phase_a` says
which ticks the track's [[chunks]] cover in that piece. The program picks the piece whose row reaches 52:00, without
another read:

```rs
--8<-- "examples/example_drive/src/fault.rs:open"
```

??? example "What the example uses here: `fault_us`, `key`, `Opened`"

    ```rs
    --8<-- "examples/example_drive/src/fault.rs:fault_us_item"

    --8<-- "examples/example_drive/src/defs.rs:key_item"

    --8<-- "examples/example_drive/src/fault.rs:opened_item"
    ```

The fault lies in piece 3, the last one. A piece that a crash left without a tail is found from its newest
[[checkpoint]] instead, as [When the power fails](crashes.md#how-a-reader-finds-the-end) shows. A piece still being
written, or closed more than once, reads as [Reading a growing piece](growing_pieces.md) explains. A [[raw medium]] such
as a card is read forwards from its start, since an older recording's tail there may pass for the newest one. Open it
with [`TapeReader::open_with`] and [`Medium::Raw`]; [Where the data ends on a raw card](raw_media.md) explains why.

## Finding blocks through the index

The [[final index]] of a closed piece lists every [[block]] with an [[index entry]], and the entries of one track lie
together. An entry says where the block is and what it holds, so a reader picks blocks before it reads any:

| Field of [`IndexEntry`] | What it says |
|---|---|
| `offset`, `len` | where the block lies in the piece |
| `kind`, `track_id` | a records block, a chunk or a summary block, and of which track |
| `first_item`, `count` | the [[item numbers]] the block holds |
| `first_tick`, `last_tick`, `latest_end` | the ticks of its first and last item, and the latest end of any item |

[`TapeReader::track_entries`] gives the entries of one track in file order. In a closed piece it reads the final
index's directory and that track's list, each checked by its own CRC. It never walks the
file. The example asks for `phase_a` in the second either side of the fault:

```rs
--8<-- "examples/example_drive/src/fault.rs:find"
```

??? example "What the example uses here: `fault_us`, `key`, `Found`"

    ```rs
    --8<-- "examples/example_drive/src/fault.rs:fault_us_item"

    --8<-- "examples/example_drive/src/defs.rs:key_item"

    --8<-- "examples/example_drive/src/fault.rs:found_item"
    ```

`phase_a`'s list in the last piece holds 1 638 entries, its chunks and its summary blocks. The query keeps two of
them, the chunks from 51:59.000 to 51:59.999 and from 52:00.000 to 52:00.999. It reads about 197 KB in 3 reads,
almost all of them the track's list. A track with a chunk every second has a long list, and a reader that queries it
often keeps the list in memory.

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/index_select.svg"
<figcaption>The index entries of <code>phase_a</code> and <code>can0</code> in the drive's last piece, each over
the ticks it covers, and the entries a query for the stretch around the fault reads. Drawn by
<code>ww_tape_figures</code> from the drive's recording at 1/1000 of its data, where a chunk covers a minute instead of
a second.</figcaption>
</figure>

<figure class="ww-figure" markdown="1">
![The fault in eventfull.](shots/fault-light.png#only-light){ width="880" loading="lazy" }
![The fault in eventfull.](shots/fault-dark.png#only-dark){ width="880" loading="lazy" }
<figcaption>What the query's two chunks hold, with the mode and the frames, as a viewer draws them. Above, 40 ms up to
the fault: the current rises past its limit, and the mode turns to fault at 52:00.000. Below, 6 ms of it, where each
frame carries its identifier: the emergency frame <code>081</code> follows the fault. A screenshot rendered by
<code>eventfull_book_shots</code> from the recording at full rate.</figcaption>
</figure>

An entry also carries what a query by item number needs. A reader that wants item 3 120 000 of `can0` takes the
entry whose `first_item` and `count` hold it. Item `j` of a chunk is item `first_item + j` of its track. An item number
counts a track's items over the whole recording, across pieces. It therefore names the same item in the recording
however its pieces are stored, in one file or in many.

## State at a place

Data often needs a state to mean something. The items of a [[state track]] stay in force until the next one replaces
them. A tape therefore has no "current state" of a whole file: what is in force depends on the place. A reader asks
for the state in force where its data starts. The two chunks of the example give the drive's current, for instance,
and the `mode` track says what the controller was doing.

[`TapeReader::state_at`] finds the newest record of a [[slot]] whose block starts before a place. Every state record
has an index entry of its own, which holds its slot and the hash of its item. The lookup therefore goes through the
index to the one block it needs:

```rs
--8<-- "examples/example_drive/src/fault.rs:mode"
```

??? example "What the example uses here: `number_of`, `InForce`"

    ```rs
    --8<-- "examples/example_drive/src/fault.rs:number_of_item"

    --8<-- "examples/example_drive/src/fault.rs:in_force_item"
    ```

At the chunk before the fault, the mode is `run, 31 A limit`, and its record from 45:35 lies about 12 MB behind. That
record is the [[state copy]] which the [[writer]] adds after the record the cut carried into the last [[piece]]. At the chunk
from 52:00, the mode is `fault: overcurrent`, from 52:00, and lies about 2 KB behind.

The number of `mode` is only the file's name for the track, so the reader first finds it by its key among the
definitions in force. With that, both lookups together read about 4 KB in 10 reads. Each record lies within [[near]],
32 MiB, so a reader of the last piece alone finds both.

[`TapeReader::definitions_at`] answers the same question for every [[definition]] at once: the clocks, the clock map
and each track's definition in force at a place. The program uses it to learn `phase_a`'s period before it reads the
samples. [`TapeReader::item_by_hash`] finds a state item by the hash of its bytes, for an application that refers to
an item that way.

### When a lookup meets damage

Damage is a reason to look further back, never an error. When the newest record of a slot does not read, the lookup
lists it as lost and takes the next older record. It also names the stretches the index does not cover, because a
newer record may lie there. Such a stretch is a [[gap]] a scan read past, or the bytes after a [[torn end]].

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/state_lookup.svg"
<figcaption>A state lookup walks back from its place through the slot's index entries. It passes a record whose block
is damaged, notes a stretch the index does not cover, and takes the next record that reads. The answer names
both.</figcaption>
</figure>

A [`Lookup`] carries the record, the lost blocks, the uncovered stretches and whether its read budget ran out.
[`Lookup::is_whole`] says that none of these happened, so no newer record can hide. Both lookups of the example are
whole. A viewer that shows a state from a lookup that is not whole marks it as uncertain.

## Reading the items of a block

Once the reader has an entry, [`TapeReader::read_block`] reads the block and checks its [[CRC-32C]].
[`TapeReader::read_chunk_head`] reads only a chunk's head, up to its payload, which gives a [[series chunk]]'s
[[grid]] without its samples. What comes next depends on the kind of block:

| Block | Read with | Chapter |
|---|---|---|
| a [[records block]] | [`Block::records`]: each record's track, time and item | [Blocks](blocks.md) |
| an [[events chunk]] | its time columns and payloads, in place | [Events](events.md) |
| a [[series chunk]] | its samples, from any sample on | [Series](series.md) |
| a [[summary block|summary]] | its nodes, a coarse view of a track | [Summaries](summaries.md) |

A block that fails its CRC is damaged, and a reader skips it. Its index entry still says what it held: which track,
which items and which ticks. [When bytes go bad](damage.md) shows how much of a damaged chunk still reads. The next
chapter reads an events chunk in place, such as the frames on `can0` around the fault.
