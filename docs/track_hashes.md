# Track owners and hashes

One tape can hold tracks from several sources, and one track can lie in several files. A reader then has two
questions: which source a track came from, and whether two recordings hold the same data for it. A track's
[[owner|track owner]] answers the first and its [[track hash]] the second. Both build on
[Pieces and chains](pieces.md).

## Tracks of several owners

Each track definition names an owner, [`TrackDef::owner`]: 32 bytes that ww_tape does not interpret. Zeros, the
default, mean the owner of the piece.

The owner is not hashed into the [[track key]] or the [[track id]]. Two owners can therefore have tracks with the
same id, and a reader tells such tracks apart by id and owner together.

Two definitions with the same key and different owners are two tracks. Each has its own item numbers, its own rows in
the piece head and the close, and its own hash.

For example, an application records two devices of the same type into one tape. It defines each device's tracks with
the same keys, and with the device's id as the owner.

## A track's hash

A [`TrackHash`] holds each track's running hash, the [[track hash]]. It starts as 32 zero bytes and takes in each
chunk of the track and each of its records as the writer writes them.

The writer leaves out puts and copies, which repeat what is already on the tape. The piece head states each track's hash
before the piece, and the close record states it after the piece.

[`Closed::tracks`] reports the hash when a piece ends.

The hash depends only on the track's own data, not on where the data lies. A track written alone or among other tracks,
in one piece or many, in one file or many, ends with the same hash.

The hash takes in each chunk whole, with its head and how its columns are coded. Two recordings with the same hash
for a track hold the same chunks, and so the same data. The same items cut into other chunks, or coded with another
codec, hash differently, so a different hash alone does not prove different data. A [subset
copy](pieces.md#copies-of-a-file) that keeps a track's chunks and records unchanged ends with the original's
hash.

[The file format](format.md) lists the bytes that the hash takes in.

### The hash of a range

The hash of a part of a track works the same way. Folded from 32 zero bytes over a run of whole chunks, it depends only
on those chunks.

[`TapeReader::range_hash`] returns it, as a [`RangeHash`], for the chunks that hold any item of an item range. A
track's records are not chunks, so a [[range hash]] leaves them out.

For example, a program that processed items 1 000 to 50 000 of a track can record the range hash of its input. Anyone
can later check that a file still holds exactly that input.

### Folding from the index

The writer stores each chunk's share of the hash, its [[unit hash]], in the chunk's index entry. The fold therefore
reads only the index, never a payload.

A fold from the index gives the hash that the writer stated, checked by the index's CRC codes. It is not a check of
the payloads. [`RangeHash::stated`] counts the chunks folded that way, and `RangeHash::read` those hashed from their
bytes.

The strict check, [`TapeReader::verify`], checks the stated hashes: the content hash, each entry against its block, and
each close row against its track's fold. Every read of a chunk checks them too.

### Without an index

Where the index is damaged or missing, the reader reads the chunks and computes the same hash. A damaged chunk of the
track is an error there, as it is through the index.

Bytes that no block explains are counted in `RangeHash::skipped`, and a chunk written again folds once. The reader
refuses index entries that name one block twice or overlap ([`IndexError::BadEntries`]), from a final index,
checkpoints or a scan alike. It then reads the chunks instead.

### Guards against a wrong index

The fold also guards against a wrong index. An index entry of a kind that the reader does not fold is counted in
`RangeHash::skipped`, never dropped silently. A chunk that the fold reads must be the one its entry names, with the
range the entry states.

This build writes and reads full integrity only. A chunk marked `FLAG_PAYLOAD_UNCHECKED` or an index entry marked
`ENTRY_UNHASHED` belongs to a reserved integrity level without unit hashes, and is refused.
