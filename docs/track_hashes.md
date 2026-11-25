---
level: nomicon
---

# Track owners and hashes

One tape can hold tracks from several sources, and one track can lie in several files. A reader then has two
questions: which source a track came from, and whether two recordings hold the same data for it. A track's
[[owner|track owner]] answers the first and its [[track hash]] the second. Both build on
[Pieces and chains](pieces.md).

## Tracks of several owners

Each track definition names an owner, [`TrackDef::owner`]: 32 bytes that ww_tape does not interpret. Zeros, the
default, mean the owner of the piece. The owner is not hashed into the [[track key]] or the [[track id]], so two
owners can have tracks with the same id. A reader tells such tracks apart by id and owner together.

Two definitions with the same key and different owners are therefore two tracks. Each has its own:

- item numbers, counted from 0;
- rows in the piece head and in the close;
- track hash.

For example, an application records two devices of the same type into one tape. It defines each device's tracks
with the same keys, and with the device's id as the owner. The test below writes two such tracks, with the owners
`0xaa…` and `0xbb…`, and reads the rows of the last close:

```rust
--8<-- "ww_tape_tests/tests/it/track_hash.rs:two_owners"
```

The two rows share the id and differ in owner, number and hash. Each track holds its own 400 items.

## A track's hash

A [`TrackHash`] holds each track's running hash, the [[track hash]]. It starts as 32 zero bytes and takes in each
chunk and each record of the track as the writer writes them. The piece head states each track's hash before the
piece, the close states it after the piece, and [`Closed::tracks`] reports it when a piece ends.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/track_hash.svg"
<figcaption>Each chunk's unit hash, stored in its index entry, folds into the track's hash in item order. The head and
the close of each piece state the hash before and after it. A range hash folds only the chunks of a range, from the
entries alone.</figcaption>
</figure>

The hash takes in only the track's own data, not where the data lies:

| Takes in | Leaves out |
|---|---|
| each chunk whole: its head and how its columns are coded | puts and state copies, which repeat what the tape holds |
| each record of the track | the piece, file or position a block lies in |
| | the other tracks written beside it |

A track written alone or among other tracks, in one piece or many, in one file or many, ends with the same hash. A
[subset](pieces.md#clones-and-subsets-of-a-file) of a file that keeps a track's chunks and records unchanged ends with
the original's hash too.

### What a hash proves

A hash compares two recordings of a track in one direction only:

- Equal hashes show that both hold the same chunks, and so the same data.
- Unequal hashes do not prove different data: the same items cut into other chunks, or coded with another codec,
  hash differently.

[The format reference](format.md) lists the bytes that the hash takes in.

## The hash of a range

The hash of a part of a track works the same way. Folded from 32 zero bytes over a run of whole chunks, it depends
only on those chunks. [`TapeReader::range_hash`] returns it as a [`RangeHash`], for the chunks that hold any item of
an item range. A track's records are not chunks, so a [[range hash]] leaves them out.

For example, a program that processed items 1 000 to 50 000 of a track can record the range hash of its input.
Anyone can later check that a file still holds exactly that input.

### Folding from the index

The writer stores each chunk's share of the hash, its [[unit hash]], in the chunk's index entry. The fold therefore
reads only the index, never a payload. Such a fold gives the hash that the writer stated, checked by the index's CRC
codes, but it does not check the payloads.

The strict check, [`TapeReader::verify`], checks the stated hashes: the content hash, each entry against its block,
and each close row against its track's fold. Every read of a chunk checks them too.

### What a range hash counts

A [`RangeHash`] says how it got each chunk, so a caller knows how much of the hash rests on stated values:

| Count | The chunks it counts |
|---|---|
| `RangeHash::stated` | folded from their entry's unit hash, as the writer stated it |
| `RangeHash::read` | hashed from their bytes: no usable index, or an entry marked `ENTRY_UNHASHED` |
| `RangeHash::skipped` | bytes no block explains, and entries of a kind the fold does not take |
| `RangeHash::unprotected` | chunks of a piece of `Integrity::None`, whose payload no CRC covered |

A piece below `Integrity::Full` holds no unit hashes, and each of its index entries is marked `ENTRY_UNHASHED`. The
fold then reads each such chunk and hashes its bytes, so the hash is still real.

### Without an index

Where the index is damaged or missing, the reader reads the chunks and computes the same hash. A damaged chunk of the
track is an error there, as it is through the index.

The fold also guards against a wrong index, from a final index, checkpoints or a scan alike:

- An entry of a kind the reader does not fold is counted in `RangeHash::skipped`, never dropped silently.
- Entries that name one block twice or overlap are refused with [`IndexError::BadEntries`]; the reader then reads the
  chunks instead.
- A chunk written again folds once.
- A chunk the fold reads must be the one its entry names, with the range the entry states.
