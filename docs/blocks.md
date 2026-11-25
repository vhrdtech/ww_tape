---
level: basic
---

# Blocks

A [[reader]] of a [[tape]] may start anywhere in it. It may meet bytes that a crash or a bad sector changed, or a kind
of data that a newer program wrote. It must check what it reads before it trusts it, and skip what it does not
understand. The drive's host, for example, writes the bytes it receives over USB as a tape, and a viewer may open that
tape at any point.

The unit that makes this possible is the [[block]]. Every block starts with a [[block header]] and checks itself, and
two kinds of block hold [[items]]: the [[records block]] and the [[chunk]].

## The block header

A [[tape]] is a sequence of [[blocks]], written one after another with no space between them. A [[live feed]] sends one
block per message, and a file is blocks back to back. Every block has three parts: a 32-byte [[block header]], a body,
and a CRC over everything before it, the [[CRC-32C]].

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/block.svg"
<figcaption>A records block of two records, byte by byte. The header gives the block's kind and length, the body
holds a clock table and each record's track, time delta, length and item, and the CRC-32C ends it. Drawn by
<code>ww_tape_figures</code> from the bytes <code>BlockWriter</code> writes.</figcaption>
</figure>

The header starts with the magic `WWTP` and the format's version. Then come the block's kind, flags, total length, and a
count of the [[records]] or [[items]] it holds. Two more fields tell a [[reader]] where the block belongs:

- The [[sequence number]] counts a [[writer]]'s blocks from 0. A jump in the numbers tells the reader how many blocks it
  lacks.
- The [[session tag]] is the same in every block of one writer's unbroken append. It lets a reader tell current blocks
  from blocks that an earlier recording left on a reused medium.

The header and the CRC-32C are the same in every version of the format. A reader therefore checks any block and finds
its end by its length, whatever its kind or version. A changed byte anywhere in the block fails the CRC, and the reader
knows the block is damaged.

A block of a kind the reader does not know reads as a good block whose body it skips, and the reading goes on. A newer
kind of block thus never looks like damage to an old reader. A block of a later version is different: it ends the
[[piece]] the reader is in. The reader lists the rest as a piece of that version, which it refuses to read
([`ReadError::Version`]).

## Kinds of block

Each kind of [[block]] has one job. They fall into four groups:

| Group | Kinds | Holds |
|---|---|---|
| Description | records on track 0 | what the tape is: the producer, the clocks and maps, the tracks, the API bundles |
| Data | records block, keyed puts, events chunk, series chunk | the items; keyed puts the keys and values of a keyed state track |
| Overview | summary | coarse views of long tracks, rebuilt from the data when lost |
| Structure | piece head, index, close, tail | where things are, how the file ends, how files chain |

[Definitions and state](definitions.md) covers the description, [Anatomy of a piece](anatomy.md) the structure and
[Summaries](summaries.md) the overview.

## Records blocks

A [[records block]] holds records of any [[tracks]], mixed, each with its own time and length. Its body starts with a
table of the [[clocks]] its records count. Each [[record]] follows as its track and clock, a time delta, the [[item]]'s
length and the item's bytes. The three numbers before the item take one byte each when they are small.

The time delta counts from the previous record on the same clock in the same [[block]]. The first one counts from 0.
Every block therefore reads alone, without the block before it. A [[writer]] may compress the whole body with lz4, and a
flag in the header says so.

The drive writes its log lines and temperatures into one records block a second. This test writes such a block with
[`BlockWriter`], reads it back with [`Block::read`], and changes one byte:

```rust
use ww_tape::{Block, BlockId, BlockKind, BlockWriter, ClockTime, SessionTag};
use example_drive::definitions;

// The numbers the drive's definitions give its board clock and its tracks.
let (_, numbers) = definitions(25_000, true, 32 << 20).expect("the drive's definitions");
let (board, log, temp) = (numbers.board_clock, numbers.log, numbers.temp);
let mut buf = [0u8; 256];
let id = BlockId::new(SessionTag::writable(1), 0);
let mut w = BlockWriter::new(&mut buf, BlockKind::Records, id, &[board]).expect("room");
// A log line at 1 s and the temperature, 32.15 °C, 250 µs later.
w.push(log, ClockTime::new(board, 1_000_000), &[b"motor: run"]).expect("room");
w.push(temp, ClockTime::new(board, 1_000_250), &[&3_215i16.to_le_bytes()]).expect("room");
let len = w.finish();
assert_eq!(len, 59);

let block = Block::read(&buf[..len]).expect("a good block");
assert_eq!((block.kind, block.record_count), (BlockKind::Records, 2));
let tracks: Vec<_> = block.records().map(|r| r.expect("reads").track).collect();
assert_eq!(tracks, [log.into(), temp.into()]);

// One changed bit fails the CRC-32C.
buf[40] ^= 1;
assert!(Block::read(&buf[..len]).is_err());
```

A records block suits a track that produces a few items at a time. It is complete as soon as it holds one record, so a
slow track never waits to fill anything. A live [[reader]] thus sees each log line at once.

## Chunks

A [[chunk]] holds a run of consecutive [[items]] of one [[track]], kept [[column]] by column. All start [[ticks]] lie
together, all payload lengths, all [[payloads]]. Each column packs its values with a [[codec]] chosen for it, and a
[[reader]] reaches item `i` of a column without decoding the items before it.

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/records_vs_chunk.svg"
<figcaption>The drive's first 96 CAN frames stored twice. As a records block, each frame carries its own track, time and length. As an events chunk, the frames' times pack into one column and their payloads into another. Drawn by
<code>ww_tape_figures</code> from the two blocks <code>ww_tape</code> writes, to one scale.</figcaption>
</figure>

The difference grows with the number of items. Ninety-six frames take 1574 bytes as [[records]] but 331 bytes as a
chunk. A [[chunk head]] costs more than a [[records block]]'s header, and its columns save the rest. A chunk of `can0`
holds a second of frames, 1000 of them, at about 2.3 bytes a frame over the whole recording.

A chunk head also carries what a reader needs to place it alone. It names the track by its [[track id]], gives the
[[item number]] of its first item and the ticks of its first and last item. It checks its payload in [[segments]] of 64
KiB, each with its own [[CRC]], so damage costs one segment, not the chunk.

There are two kinds of chunk, one per way a track keeps its items:

- An [[events chunk]] holds items with times of their own, such as the drive's CAN frames.
- A [[series chunk]] holds [[samples]] at a fixed period, such as the drive's `phase_a`. Its chunk head carries the
  [[grid]] that times and scales them, and the chunk holds no time per sample.

| | Records block | Chunk |
|---|---|---|
| Tracks | any number, mixed | exactly one |
| Items | records back to back, each with its own time and length | consecutive items, column by column |
| Suits | a few items at a time: a log line, a state change | many items: 1000 frames a second, 25 000 samples a second |
| Finding one item | walk the block from its start | jump to it by its item number |
| The drive's tracks | `log`, `temp`, the puts of `mode` | `can0` in events chunks, `phase_a` in series chunks |

[Event times and their codecs](columns.md) and [Samples and their codecs](sample_codecs.md) show how the columns are
coded, and [Events](events.md) and [Series](series.md) how a
reader reads them.

## Definitions on track 0

A [[reader]] needs to know what each [[track]] is before its [[items]] mean anything. Track 0 holds that knowledge: each
record on it is a [[definition]] of a [[clock]], a [[clock map]], a track or the [[producer]]. Records of track 0 carry
no time, and a [[block]] that holds them holds nothing else. [Definitions and state](definitions.md) explains how a
[[writer]] places them and how a reader finds them.
