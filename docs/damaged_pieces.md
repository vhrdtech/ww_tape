---
level: basic
---

# What a damaged piece gives

Damage to a [[piece]] lands in one of three places: the start, the end or somewhere between. Each place holds different
blocks, so each costs a [[reader]] something different. This page shows all three on the drive's last piece, each on its
own copy. The copies hold the flipped bit of [When bytes go bad](damage.md), the first 4 KiB lost and the last 4 KiB
lost. A lost
stretch here reads as zeros, as sectors that a disk could not read.

## Three damages, one piece each

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/loss_cases.svg"
<figcaption>The drive's last piece three times, one damage each, and what a reader reads of it. A flipped bit costs one
chunk. The first 4 KiB lost cost the blocks in them. The last 4 KiB lost cost the close and the tail, yet every block
reads from the newest checkpoint. Drawn by <code>ww_tape_figures</code> from the drive's last piece at 1/1000 of its
data, where 4 KiB is a larger share.</figcaption>
</figure>

At full scale, the piece holds 3449 blocks that its index lists. `cargo run -p example_drive` prints what each damage
leaves:

| Damage | The reader finds the piece by | Content hash | Blocks that read | Definitions found |
|---|---|---|---:|---:|
| a flipped bit | its tail | does not hold | 3448 | 10 of 10 |
| the first 4 KiB lost | its tail | does not hold | 3444 | 10 of 10 |
| the last 4 KiB lost | its final index, with no byte to scan after it | no close to check | 3449 | 10 of 10 |

### The first 4 KiB lost

These bytes hold the [[piece head]], the first [[definitions]] and the first blocks of data. The reader still opens the
piece from its [[tail]]. By the [[final index]] it reads every block outside the lost bytes, 3444 of the 3449. Five
blocks are lost:

- the [[puts]] of the [[layout]] and the producer;
- the puts of the definitions the piece carries for the tracks its head carries, with the mode's newest item;
- the records block of minute 45's first second, its log lines and temperatures;
- the chunk of frames of that second;
- the chunk of samples that the lost bytes reach into.

Each definition comes from its newest record that reads. The writer gives every definition and every plain [[state
item]] a first [[state copy]] 4 KiB after its first record, without waiting for a use ([`NearPolicy::copy_first`]). The
layout and the producer's name get theirs the same way. These copies lie past the lost bytes, so every definition of the
drive is found again, the layout and the producer among them. A [[reader]] scanning past the newest [[checkpoint]] takes
its gap rules from the layout's copy.

The drive's hour spends 8.3 kB of its 116 MB on all its puts and copies ([What the writer adds](writer_adds.md)).

The mode is a [[state track]], and a cut carries its newest item beside its definition. That item lay in the lost
bytes. A reader thus finds no mode in force over the piece's first second, 45:00. The mode's first copy, written before
the chunks of the next second, gives it again from there.

| Stretch of the piece | What a reader lacks there |
|---|---|
| before the mode's copy | the mode in force |
| before the layout's first copy, on a raw-media walk | the gap rules, unless a medium layer gives them ([`OpenOptions::layout`]) |
| the whole piece | the check of the chain's link back, which the piece head named |

The last piece of a chain names no next piece, so it has no link forward to lose.

### The last 4 KiB lost

These bytes hold the [[tail]], the [[close]] and the padding up to a multiple of 4 KiB. The reader finds no tail and
reads the piece as unclosed. The [[final index]] ends before the lost bytes and reads whole. The reader takes it as the
newest index block, with no byte to scan after it. Every one of the 3449 blocks of data reads, and so does every
definition, but the piece loses its [[content hash]].

| Lost bytes | The reader reads the blocks by |
|---|---|
| stop short of the final index, as here | the final index, taken as the newest index block, with no byte to scan after it |
| reach into the final index | the newest [[checkpoint]] and a scan of the blocks after it, which costs time but no block |

A piece in the middle of a chain would also lose its close's link to the next piece.

### One block in the middle lost

In a closed piece the reader reads blocks by the [[final index]], so a lost block in the middle costs that block alone.
What that block held decides the cost:

| Lost block | Cost |
|---|---|
| series chunk, chunk head good | one segment per hit, the samples of the groups in it |
| series or events chunk, chunk head damaged | the chunk; the index still says which track, items and ticks it held |
| events chunk, payload damaged | the items of the groups it hits, marked suspect |
| records block | its records; the index says how many, the sequence numbers how many blocks |
| summary | nothing: a summary derives from its track's data, and `ww_tape_summary` builds it again |
| definitions or state records | nothing once a copy or a later put of each item reads |
| checkpoint | in an unclosed piece, the stretch to the next good checkpoint is scanned instead |
| final index | in a closed piece, the list of all blocks: [`TapeReader::entries`] fails, while [`TapeReader::track_entries`] still reads each track whose own list reads |

In an unclosed piece the same holds before the newest checkpoint. After it, the reader's scan stops at the lost block,
and a Resync walk ([Reading past damage](reading_past_damage.md)) reads what follows.

## What the checks cannot do

Some damage costs more than one [[segment]]. These limits follow from the design:

| Limit | Why |
|---|---|
| damage is found, never repaired | the block kind [`BlockKind::Parity`] is reserved for parity blocks: no writer writes one, and a reader skips one as a kind it does not know |
| a damaged chunk head costs the whole chunk | its CRC finds the damage, but no second copy of the head exists |
| salvage can miss small shifts | a flipped bit in a group record can shift a tick or a slope without breaking the order; the group reads shifted and is not marked suspect |
| an unclosed piece has no content hash | the hash needs a close |
| an unclosed piece under about 4 MiB is scanned from its start | it has no checkpoint yet; the scan stops at the first bad block, and only a Resync walk reads past it |
| a [[resume]] cuts only a [[torn end]] | damage with good blocks behind it stops a resume and leaves the piece unclosed, for a Resync walk to read |
| a stale [[session]] may match by chance | session tags have 32 bits, so data of an earlier recording matches with a probability of 2⁻³² per earlier session |

### Definitions held once

A piece that ends less than 4 KiB after a definition's first record holds that definition once. One bad sector there can
take it. Chunks then still place themselves by track id, item number and tick. Without clock rates and item types, a
reader cannot turn ticks into seconds or bytes into values.

An item of a keyed [[track]] and a record of a group of [[puts]] get copies only at uses ([Puts and
copies](format.md#puts-and-copies)). Copies take state entries of the piece's [index policy](carried_room.md). In a piece
whose state entries are full, the writer skips the copy rather than refuse a call. Such a piece may name a definition,
the [[layout]] or the producer once, after its head.

### Lookups after a stopped scan

The state lookups go by index entries. A damaged block where a scan stopped has no known kind and no entries. The
lookups therefore take the newest record before it and report the stretch after it as uncovered by the index.

The [format reference](format.md#sessions-media-and-damage) states the walks and their reports as rules. Damage on a raw
card brings one more question, where the data ends, which [Raw media](raw_media.md) answers.
