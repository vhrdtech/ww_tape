# Blocks and definitions

This page explains the parts every tape is built from, and how a tape says what its data means. It assumes the words
of [Tape concepts](concepts.md).

## Blocks

A tape is a sequence of **blocks**. Every block has a 32-byte header, a body, and a cyclic redundancy check (CRC-32C)
over everything before it.

The header starts with the magic `WWTP`. Its other fields are these:

- the block's version, kind and flags;
- the block's total length;
- a count of records, items or entries, depending on the kind;
- a **sequence number**, which counts a writer's blocks from 0;
- a **session tag**, which names one writer's unbroken append, such as a producer's start or a device's boot.

A file is blocks appended back to back, with no padding between them. A live feed sends one block per message.

The header and the trailing CRC-32C are the same in every version of the format. A reader can therefore check any block
and skip it by its length, even a block of a kind or version it doesn't know.

A later version changes only the bodies. An old reader therefore skips a block it doesn't understand, instead of
taking it for damage.

A gap in the sequence numbers says how many blocks are missing. A reader uses the session tag to tell current data
from stale data that an earlier recording left on a medium.

[Sessions and raw media](raw_media.md#sessions-and-stale-data) explains how a reader does that. The block kinds fall
into four groups:

| Group | Kinds | Holds |
|---|---|---|
| Description | records on track 0 | what the tape is: the producer, clocks, maps, tracks, API bundles |
| Data | records, events chunk, series chunk | the items |
| Overview | summary | coarse views of long tracks, rebuilt from the data when lost |
| Structure | piece head, index, finish, tail | where things are, how the file ends, how files chain |

## Definitions on track 0

A reader needs some facts before the data means anything, and **definitions** state them. Definitions describe these
things:

- the producer;
- the **layout**: how the writer places blocks;
- the API bundles;
- every clock, clock map and track;
- typed items, such as a run record.

Each definition is a record on track 0, a track reserved for definitions. A block that holds definitions holds nothing
else.

Each definition fills a **slot** that follows from the definition itself. A newer definition in the same slot replaces
the older one.

- A track's slot is its number.
- A clock's slot is its number.
- A map's slot is the two clocks it links.
- A bundle's slot is its hash.

### The definitions an application holds

The application keeps the definitions it works with in a `Definitions` container. `Definitions::define` holds a
definition in its slot, and checks it against the definitions already held:

- A track or a map must name clocks that the container holds.
- An epoch clock must come with its link in the same `define_all` call.

Defining writes nothing to the tape. `Definitions::close` drops a track, and the clocks, maps and bundles that no held
track names go with it.

The container reports how many definitions it holds (`len`) and their size in bytes (`bytes`). The application
decides how many to keep.

The writer puts each definition before the first data that needs it. Adding a track mid-piece therefore costs one
record, however many tracks the piece already has.

### The producer and the layout

The producer and the layout belong to the writer, which takes them from `WriterOptions::producer` and
`WriterOptions::layout`. The writer puts them in three places:

- after every piece head;
- before the first block of a file;
- when a block starts another session.

The writer refuses a block of definitions that the caller wrote. A relay passes a live producer's blocks on with
`TapeWriter::relay_block`.

`relay_block` holds the definitions those blocks carry. It puts their records by the writer's own rule.

The [describe module docs](https://github.com/vhrdtech/ww_tape/blob/main/ww_tape/src/describe.rs) hold the
definitions, and the [state module docs](https://github.com/vhrdtech/ww_tape/blob/main/ww_tape/src/state.rs) the
slots. [State tracks and near use](index.md#state-tracks-and-near-use) explains how the writer puts them.

## Records blocks

A **records block** holds records of any tracks back to back. Each record has a head, a time delta, the item's length
and the item's bytes.

The head names the track, and which of the block's clocks the record counts. The head, the delta and the length are
variable-length integers, and records on track 0 carry no time.

The delta is against the previous record on the same clock in the same block, and the first one is absolute. Every
block therefore reads alone.

A writer may compress the whole body of a records block with lz4, and the header flag `FLAG_BODY_CODED` says so. The
[`ww_tape` crate docs](https://github.com/vhrdtech/ww_tape/blob/main/ww_tape/src/lib.rs) give the byte layout of the
header and of records blocks.

Records blocks suit tracks that produce a few items at a time, where waiting to fill a chunk would delay a live
reader. Large tracks go into chunks, which [Events and series](events_and_series.md) describes.

## Deferred blocks

`TapeWriter::defer_block` lets a block of summaries or other small data wait for a gap. The writer puts the definitions the block needs at once, when the application defers the block.

The writer places the block later, with no puts of its own. In an aligned file, the block waits for the gap before a
chunk's payload, for at most `DEFER_WINDOW` (1 MiB) of writes.

The block therefore lies at most its tracks' `near` plus that window after their definitions. In a file that isn't
aligned, the writer places the block at once.

A cut or a finish places every deferred block first. A deferred block therefore never leaves its piece.
