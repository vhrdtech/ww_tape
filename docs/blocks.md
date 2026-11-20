---
level: basic
---

# Blocks

The drive's host receives a steady stream of bytes over USB and writes them out as a [[tape]]. A reader of that tape may
start anywhere. It may meet bytes that a crash or a bad sector changed, or a kind of data that a newer program wrote. It
must check each piece of data before it trusts it, and skip what it does not understand.

This chapter shows the unit that makes this possible, the [[block]], and the two kinds of block that hold items. After
it you can read a block header and tell a records block from a chunk. You can also say why a reader skips what it does
not know.

## The block header

A tape is a sequence of blocks, written one after another with no space between them. A live feed sends one block per
message, and a file is blocks back to back. Every block has three parts: a 32-byte [[block header]], a body, and a
cyclic redundancy check (CRC) over everything before it, the [[CRC-32C]].

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/block.svg"
<figcaption>A records block of two records, byte by byte. The header gives the block's kind and length, the body
holds a clock table and each record's head, time delta, length and item, and the CRC-32C ends it. Drawn by
<code>ww_tape_figures</code> from the bytes <code>BlockWriter</code> writes.</figcaption>
</figure>

The header starts with the magic `WWTP` and the format's version. Then come the block's kind, flags, total length, and a
count of the records or items it holds. Two more fields tell a reader where the block belongs:

- The [[sequence number]] counts a writer's blocks from 0. A gap in the numbers tells the reader how many blocks it
  lacks.
- The [[session tag]] is the same in every block of one writer's unbroken append. It lets a reader tell current blocks
  from blocks that an earlier recording left on a reused medium.

The header and the CRC-32C are the same in every version of the format. A reader therefore checks any block and skips it
by its length, even a block of a kind or a version it does not know. A later version changes only the bodies, so an old
reader skips a new block instead of taking it for damage. A changed byte anywhere in the block fails the CRC, and the
reader knows the block is damaged.

## Kinds of block

Each kind of block has one job. They fall into four groups:

| Group | Kinds | Holds |
|---|---|---|
| Description | records on track 0 | what the tape is: the producer, the clocks and maps, the tracks, the API bundles |
| Data | records block, events chunk, series chunk | the items |
| Overview | summary | coarse views of long tracks, rebuilt from the data when lost |
| Structure | piece head, index, close, tail | where things are, how the file ends, how files chain |

This chapter covers the data blocks. [Definitions and state](definitions.md) covers the description, and [Anatomy of a
piece](anatomy.md) the structure. [Summaries](summaries.md) covers the overview.

## Records blocks

A [[records block]] holds records of any tracks, mixed, each with its own time and length. Its body starts with a table
of the clocks its records count. Each [[record]] follows as a head, a time delta, the item's length and the item's
bytes. The head names the track and the record's clock, and the three numbers before the item take one byte each when
they are small.

The time delta counts from the previous record on the same clock in the same block. The first one counts from 0. Every
block therefore reads alone, without the block before it. A writer may compress the whole body with lz4, and a flag in
the header says so.

The drive writes its log lines and temperatures into one records block a second. This test writes such a block with
[`BlockWriter`], reads it back with [`Block::read`], and changes one byte:

```rust
use ww_tape::{Block, BlockId, BlockKind, BlockWriter, ClockTime, SessionTag};
use ww_tape_example::{BOARD_CLOCK, LOG, TEMP};

let mut buf = [0u8; 256];
let id = BlockId::new(SessionTag::writable(1), 0);
let mut w = BlockWriter::new(&mut buf, BlockKind::Records, id, &[BOARD_CLOCK]).expect("room");
// A log line at 1 s and the temperature, 32.15 °C, 250 µs later.
w.push(LOG, ClockTime::new(BOARD_CLOCK, 1_000_000), &[b"motor: run"]).expect("room");
w.push(TEMP, ClockTime::new(BOARD_CLOCK, 1_000_250), &[&3_215i16.to_le_bytes()]).expect("room");
let len = w.finish();
assert_eq!(len, 59);

let block = Block::read(&buf[..len]).expect("a good block");
assert_eq!((block.kind, block.record_count), (BlockKind::Records, 2));
let tracks: Vec<u32> = block.records().map(|r| r.expect("reads").track).collect();
assert_eq!(tracks, [LOG, TEMP]);

// One changed bit fails the CRC-32C.
buf[40] ^= 1;
assert!(Block::read(&buf[..len]).is_err());
```

A records block suits a track that produces a few items at a time. It is complete as soon as it holds one record, so a
slow track never waits to fill anything. A live reader thus sees each log line at once.

## Chunks

A [[chunk]] holds a run of consecutive items of one track, stored column by column. All start ticks lie together, all
payload lengths, all payloads. Each column packs its values with a codec chosen for it, and a reader reaches item `i` of
a column without decoding the items before it.

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/records_vs_chunk.svg"
<figcaption>The drive's first 96 CAN frames stored twice. As a records block, each frame carries its own head, time
and length. As an events chunk, the frames' times pack into one column and their payloads into another. Drawn by
<code>ww_tape_figures</code> from the two blocks <code>ww_tape</code> writes, to one scale.</figcaption>
</figure>

The difference grows with the number of items. Ninety-six frames take 1574 bytes as records but 331 bytes as a chunk. A
chunk's head costs more than a records block's header, and its columns save the rest. A chunk of `can0` holds a second
of frames, 1000 of them, at 2.33 bytes a frame over the whole recording.

A chunk's head also carries what a reader needs to place it alone. It names the track by its [[track id]], gives the
[[item number]] of its first item and the ticks of its first and last item. It checks its payload in [[segments]] of 64
KiB, each with its own CRC, so damage costs one segment, not the chunk.

There are two kinds of chunk, one per way a track stores its items:

- An [[events chunk]] holds items with times of their own, such as the drive's Controller Area Network (CAN) frames.
- A [[series chunk]] holds samples at a fixed period, such as the drive's `phase_a`. Its head carries the [[grid]] that
  times and scales them, and the chunk stores no time per sample.

| | Records block | Chunk |
|---|---|---|
| Tracks | any number, mixed | exactly one |
| Items | records back to back, each with its own time and length | consecutive items, column by column |
| Suits | a few items at a time: a log line, a state change | many items: 1000 frames a second, 25 000 samples a second |
| Finding one item | walk the block from its start | jump to it by its item number |
| The drive's tracks | `log`, `temp`, the puts of `mode` | `can0` in events chunks, `phase_a` in series chunks |

[Chunks from columns](columns.md) shows how the columns are coded, and [Events](events.md) and [Series](series.md) how a
reader reads them.

## Definitions on track 0

A reader needs to know what each track is before its items mean anything. Track 0 holds that knowledge: each record on
it is a [[definition]] of a clock, a clock map, a track or the producer. Records of track 0 carry no time, and a block
that holds them holds nothing else. [Definitions and state](definitions.md) explains how a writer places them and how a
reader finds them.
