---
level: basic
---

# When bytes go bad

Stored bytes go bad: a failing sector flips a bit, a torn write leaves half a block. Two questions follow: what the
damage costs, and how a [[reader]] says exactly what it lost instead of guessing. A tape's checks confine the damage, a
reader reads past a bad [[block]], and it salvages the good parts of a damaged chunk.

The examples damage the drive's last [[piece]], the one that holds the fault at minute 52, one copy per kind of damage.

## Checks at three levels

A tape finds damage before it can contain it. Every [[block]] ends with a [[CRC-32C]], a CRC
over all its bytes. A [[chunk]] adds two finer checks inside its body, and a closed [[piece]] adds one coarse check over
all its blocks.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/checksums.svg"
<figcaption>The checks of a tape. The block CRC covers a whole block. Inside a chunk, a CRC covers the header and the
chunk head, and the chunk head holds a CRC per segment of the payload. Over a closed piece, the content hash covers
every byte before the close record.</figcaption>
</figure>

The [[chunk head]] carries the chunk's length, track, [[item numbers]] and [[ticks]], and its own CRC. It also holds one
CRC per [[segment]], a fixed stretch of the payload that writers make 64 KiB long. The chunk head's CRC is what keeps
damage small. With a good chunk head and a damaged payload, a reader still trusts the chunk's length. It finds the next
block and knows which segments failed.

| Check | Covers | Damage there costs |
|---|---|---|
| block CRC-32C | each whole block | that block |
| the chunk head's CRC | a chunk's header and chunk head | the chunk; the index still says what it held |
| segment CRC-32C | each segment of a chunk's payload | one segment of one chunk |
| [[content hash]], [[BLAKE3]] | every byte of a closed piece before its close record | nothing; it proves the piece unchanged |

The checks detect damage and never repair it. Correction is the medium's job: the error correction of a disk, of a flash
controller or of a file system. A tape catches only what that correction missed.

Every CRC is CRC-32C. The processor's CRC instructions compute it where they exist. Elsewhere, such as in a browser or
on a microcontroller, a table computes it, with the same result.

### Checks on bytes kept in memory

A tape kept in memory by the process that writes it is read without its payload CRCs by default. No disk or wire
touched those bytes. [`OpenOptions::check_payloads`] decides. [`PayloadCheck::BySource`] checks a file and skips a
[`ByteSource::trusted`] source, such as a memory tape. [`PayloadCheck::Always`] also catches memory faults.

An unchecked read keeps every other check: header, chunk head CRC, lengths and records. [`Block::checked`] says
how far a block was checked. The bench `read_check` times [`TapeReader::read_block`] from memory both ways:

--8<-- "docs/plots/bench_read_check_read.txt"

An unchecked read takes 47 % of the time of a checked one for a chunk, mostly its copy, and 33 % for a records block.

--8<-- "docs/plots/bench_read_check_machine.txt"

## What the flipped bit costs

In the drive's last piece, a flipped bit about 9.7 MB in lands in the chunk of `phase_a` that starts at 50:00. The chunk
holds one second of samples, 25 000 of them, in about 28 KB. That is less than one segment, so the chunk has one
segment, and its CRC fails.

The cost is that segment: one second of `phase_a`, 25 000 of the run's 90 million samples. Every other block of the
piece reads, and so does every other track in the same second. The fault at minute 52 is untouched. `cargo run -p
example_drive` prints these numbers, and [`examples/example_drive/src/damage.rs`] makes the damage.

A [[reader]] meets the flip in two places. [`TapeReader::verify`] checks the piece against its [[content hash]] and
finds that it no longer holds. Reading by the [[final index]], [`TapeReader::read_block`] fails for that one chunk and
succeeds for the other 3444 blocks the index lists. The reader thus knows which track and which items it lost, since the
index entry names them, while the bytes themselves do not read.

A chunk longer than one segment loses less. The sample codecs code samples in groups that decode alone, and
[`Chunk::range_damaged`] says whether a group's bytes lie in a bad segment. A reader then decodes the groups of the good
segments and marks the rest lost. Damage thus costs the samples of the bad segment, not the whole chunk.

## Reading past damage

[`FileBlocks`] walks the blocks of a byte stream in order. What it does at damage depends on its [[recovery mode]], a
[`Recovery`] value given to [`FileBlocks::with_recovery`]. The two modes answer different questions, and the figure
walks the drive's last piece both ways.

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/prefix_resync.svg"
<figcaption>One piece with two damaged blocks, walked twice. Prefix reads the blocks before the first damage and stops.
Resync hands out the chunk with its bad segment marked, reports the bad records block, skips to the next block and reads
on. Drawn by <code>ww_tape_figures</code> from the drive's last piece at 1/1000 of its data, a bit flipped in a chunk of
<code>phase_a</code> and one in a records block.</figcaption>
</figure>

[`Recovery::Prefix`], the default, reads the longest valid prefix of the bytes. It stops at the first block with a bad
magic, length or CRC, or at a block cut short. It also stops at a sequence number going backwards, and at a good block
of another [[session]] where none may start. It reports where it stopped and why ([`FileError::Cut`]).

A crash leaves good blocks, then a [[torn block]], which is what Prefix expects. In the flipped piece, Prefix reads 1154
blocks and stops where the damaged chunk starts, about 9.7 MB in.

[`Recovery::Resync`] reads on. At a bad block it reports [`FileError::Bad`] and looks for the next block's magic one
byte after the bad block's start. Any byte may start a block, so Resync takes a block only when its CRCs match. It hands
out a chunk with a good chunk head and a damaged payload instead of skipping it, with [`Block::is_intact`] false and
[`Chunk::bad_segments`] naming the failed segments. In the flipped piece, Resync hands out all 3455 blocks, the damaged
chunk among them, and reports nothing else.

This walk counts what a flipped bit costs, as a test or a salvage tool would:

```rust
use ww_tape::{FileBlocks, Recovery};

let drive = example_drive::Drive::scaled(1000).expect("1000 divides the rates");
let recording = drive.record().expect("the run records");
let (piece, byte) = recording.story_flip().expect("the run reaches minute 50");
let damaged = recording.flip(piece, byte).expect("the byte lies in the piece");

let mut walk = FileBlocks::with_recovery(&damaged.bytes[..], Recovery::Resync);
let (mut good, mut bad_segments, mut reports) = (0, 0, 0);
while let Some(block) = walk.next_block() {
    match block {
        Ok(b) if b.is_intact() => good += 1,
        Ok(b) => bad_segments += b.chunk().map_or(0, |c| c.bad_segments().count()),
        Err(_) => reports += 1,
    }
}
// Every block reads but one chunk, which has one bad segment.
assert_eq!((bad_segments, reports), (1, 0));
assert!(good > 50);
```

Resync has no sequence number check, since damage can hide any block, and guards against a repeated number instead. It
remembers the latest blocks of data it handed out, as many as the [format reference](format.md#duplicates) states. A
good block with the [[session]] and [[sequence number]] of one of them but other bytes is a conflict
([`FileError::Conflict`]). Resync never hands out the second block of a conflict as a block of its own.

The first block of a conflict went out already when Resync meets the second. An application that keeps what Resync gives
therefore holds blocks back since the last damage, or can take one back.

## Salvaging the items of an events chunk

An [[events chunk]] usually has one segment, so a flipped bit in it fails that segment whole. Its columns still carry
enough structure to [[salvage]] most items: read every item it can and mark the rest [[suspect]].
[`EventTimes::salvage`] and [`Payloads::salvage`] do this.

The chunk stores the time codec and the payload lengths per group of 128 items. Each group's record must agree with its
neighbours: its offsets chain, its ticks are in order and its widths are in range. A group that breaks the chain is
suspect, and so are its items only.

The cost depends on where the damage lands:

- A flipped bit in one time residual damages one item.
- A flipped bit in a group record damages one group or two.
- A payload column coded with lz4 is the exception: it decodes as a whole, so one bad byte makes every item of the chunk
  suspect.

## What a damaged piece still gives

Damage to a piece lands in one of three places: the start, the end or somewhere between. The drive's last piece shows
all three, each on its own copy: the flipped bit, the first 4 KiB lost, the last 4 KiB lost. A lost stretch here reads
as zeros, as sectors that a disk could not read.

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/loss_cases.svg"
<figcaption>The drive's last piece three times, one damage each, and what a reader reads of it. A flipped bit costs one
chunk. The first 4 KiB lost cost the blocks in them. The last 4 KiB lost cost the close and the tail, yet every block
reads from the newest checkpoint. Drawn by <code>ww_tape_figures</code> from the drive's last piece at 1/1000 of its
data, where 4 KiB is a larger share.</figcaption>
</figure>

At full scale, the piece holds 3445 blocks that its index lists. `cargo run -p example_drive` prints what each damage
leaves:

| Damage | The reader finds the piece by | Content hash | Blocks that read | Definitions found |
|---|---|---|---:|---:|
| a flipped bit | its tail | does not hold | 3444 | 10 of 10 |
| the first 4 KiB lost | its tail | does not hold | 3441 | 10 of 10 |
| the last 4 KiB lost | its final index, with no scan after it | no close to check | 3445 | 10 of 10 |

### The first 4 KiB lost

These bytes hold the [[piece head]], the first [[definitions]] and the first blocks of data. The reader still opens the
piece from its [[tail]]. By the [[final index]] it reads every block outside the lost bytes, 3441 of the 3445. Four
blocks are lost:

- the [[puts]] of the layout and the producer;
- the puts of the definitions the piece carries for the tracks its head carries, with the mode's newest item;
- the chunk of frames of minute 45's first second;
- the chunk of samples that the lost bytes reach into.

Each definition comes from its newest record that reads. The writer copies a definition at a use 1 MiB or more after its
first record. Every call that puts a definition or an item uses the [[layout]] and the producer's name too, so the
writer copies them the same way. Every definition of the drive is thus found again, the layout and the producer among
them. A [[reader]] scanning past the newest [[checkpoint]] takes its gap rules from the layout's copy.

The mode is a [[state track]], and a cut carries its newest item beside its definition. That item lay in the lost
bytes. A reader thus finds no mode in force over the piece's first 35 seconds, 45:00 to 45:34. The mode's copy at 45:35
gives it again from there.

A raw-media walk that starts past the lost bytes knows no gap rules until it reaches that copy. Over the first MiB or so
of the piece, only the rules a medium layer gives ([`OpenOptions::layout`]) read past a gap. Without them a blank
stretch there ends the data.

The piece head named the piece before it, so the chain's link back from this piece can no longer be checked. The last
piece of a chain names no next piece, so it has no link forward to lose.

### The last 4 KiB lost

These bytes hold the [[tail]], the [[close]] and the padding up to a multiple of 4 KiB. The [[final index]] before
them is whole. The reader finds no tail and reads the piece as unclosed. It takes the final index as the newest index
block, with no byte to scan after it. Every one of the 3445 blocks of data reads, and so does every definition,
but the piece loses its [[content hash]].

Lost bytes that reach into the final index make the reader take the newest [[checkpoint]] and scan the stretch after it.

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
and a Resync walk reads what follows.

## What the checks cannot do

The checks bound what damage costs, but some damage costs more than one segment. These limits follow from the design.

The checks detect damage and never repair it. The block kind [`BlockKind::Parity`] is reserved for parity blocks: no
writer writes one, and a reader skips one as a kind it does not know.

A damaged chunk head costs the whole chunk. Its CRC finds the damage, but no second copy of the chunk head exists to
read instead.

Salvage can miss small shifts. A flipped bit in a group record can shift a tick or a slope without breaking the order.
The group then reads shifted and is not marked suspect.

The content hash needs a close, so an unclosed piece has no hash to check it by.

A reader scans an unclosed piece without a checkpoint yet, under about 4 MiB, from its start. The scan stops at the
first bad block, and only a Resync walk reads past it.

An unclosed piece shorter than 1 MiB holds each definition once, so one bad sector can take a definition. Chunks then
still place themselves by track id, item number and tick. Without clock rates and item types, a reader cannot turn ticks
into seconds or bytes into values.

The layout and the producer have the same limit, and one more. Their copy takes two state entries of the piece's
[index policy](carried_definitions.md#when-they-dont-fit). In a piece whose state entries are full, the writer skips the
copy rather than refuse a call. Such a piece names the layout and the producer once, after its head.

The state lookups go by index entries. A damaged block where a scan stopped has no known kind and no entries. The
lookups therefore take the newest record before it and report the stretch after it as uncovered by the index.

A [[resume]] cuts only a [[torn end]]. Damage with good blocks behind it therefore stops a resume and leaves the piece
unclosed, and a Resync walk reads it.

Session tags have 32 bits. Data of an earlier recording thus matches the current [[session]] by chance with a
probability of 2⁻³² per earlier session.

The [format reference](format.md#sessions-media-and-damage) states the walks and their reports as rules. Damage on a raw
card brings one more question, where the data ends, which [Raw media](raw_media.md) answers.
