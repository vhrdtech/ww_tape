---
level: basic
---

# When bytes go bad

Stored bytes go bad: a failing sector flips a bit, a torn write leaves half a block. Two questions follow: what the
damage costs, and how a [[reader]] says exactly what it lost instead of guessing. This chapter answers them in three
pages:

- this page: the checks that find damage, and what one flipped bit costs;
- [Reading past damage](reading_past_damage.md): the walks that read on after a bad [[block]], and the salvage of a
  damaged chunk;
- [What a damaged piece gives](damaged_pieces.md): a [[piece]] that lost its start, its end or a block between.

The examples damage the drive's last piece, the one that holds the fault at minute 52, one copy per kind of damage.

## Checks at three levels

A tape finds damage before it can contain it. Every [[block]] ends with a [[CRC-32C]], a CRC over all its bytes. A
[[chunk]] adds two finer checks inside its body, and a closed [[piece]] adds one coarse check over all its blocks.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/checksums.svg"
<figcaption>The checks of a tape. The block CRC covers a whole block. Inside a chunk, a CRC covers the header and the
chunk head, and the chunk head holds a CRC per segment of the payload. Over a closed piece, the content hash covers
every byte before the close record.</figcaption>
</figure>

The [[chunk head]] carries the chunk's length, track, [[item numbers]] and [[ticks]], and its own CRC. It also holds
one CRC per [[segment]], a fixed stretch of the payload that writers make 64 KiB long. The chunk head's CRC keeps damage
small: with a good chunk head and a damaged payload, a reader still trusts the chunk's length. It finds the next block
and knows which segments failed.

| Check | Covers | Damage there costs |
|---|---|---|
| block CRC-32C | each whole block | that block |
| the chunk head's CRC | a chunk's header and chunk head | the chunk; the index still says what it held |
| segment CRC-32C | each segment of a chunk's payload | one segment of one chunk |
| [[content hash]], [[BLAKE3]] | every byte of a closed piece before its close record | nothing; it proves the piece unchanged |

The checks detect damage and never repair it. Correction is the medium's job, done by one of these:

- the error correction of a disk;
- the error correction of a flash controller;
- a file system that keeps its own checksums and copies.

A tape catches only what that correction missed.

Every CRC is CRC-32C. The processor's CRC instructions compute it where they exist. Elsewhere, such as in a browser or
on a microcontroller, a table computes it, with the same result.

### Checks on bytes kept in memory

A tape kept in memory by the process that writes it is read without its payload CRCs by default. No disk or wire
touched those bytes. [`OpenOptions::check_payloads`] decides:

| Value | Checks payload CRCs of |
|---|---|
| [`PayloadCheck::BySource`], the default | a file, but not a [`ByteSource::trusted`] source such as a memory tape |
| [`PayloadCheck::Always`] | every source, so it also catches memory faults |

An unchecked read keeps every other check: header, chunk head CRC, lengths and records. [`Block::checked`] says how
far a block was checked. The bench `read_check` times [`TapeReader::read_block`] from memory both ways:

--8<-- "docs/plots/bench_read_check_read.txt"

An unchecked read takes 47 % of the time of a checked one for a chunk, mostly its copy, and 33 % for a records block.

--8<-- "docs/plots/bench_read_check_machine.txt"

A writer may skip checks too, and a seal adds them later: [Fewer checks, sealed later](integrity.md).

## What the flipped bit costs

In the drive's last piece, a flipped bit about 9.7 MB in lands in the chunk of `phase_a` that starts at 50:00. The
chunk holds one second of samples, 25 000 of them, in about 28 KB. That is less than one segment, so the chunk has one
segment, and its CRC fails.

The cost is that segment: one second of `phase_a`, 25 000 of the run's 90 million samples. Every other block of the
piece reads, and so does every other track in the same second. The fault at minute 52 is untouched. `cargo run -p
example_drive` prints these numbers, and [`examples/example_drive/src/damage.rs`] makes the damage.

A [[reader]] meets the flip in two places:

| Call | What it finds |
|---|---|
| [`TapeReader::verify`] | the piece no longer matches its [[content hash]] |
| [`TapeReader::read_block`], by the [[final index]] | that one chunk fails; the other 3445 blocks the index lists read |

The reader thus knows which track and which items it lost, since the index entry names them, while the bytes themselves
do not read.

A chunk longer than one segment loses less. The sample codecs code samples in groups that decode alone, and
[`Chunk::range_damaged`] says whether a group's bytes lie in a bad segment. A reader then decodes the groups of the good
segments and marks the rest lost. Damage thus costs the samples of the bad segment, not the whole chunk.

The next page walks this piece block by block: [Reading past damage](reading_past_damage.md).
