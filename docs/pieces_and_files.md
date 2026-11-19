# Pieces and files

A recording that runs for days does not fit one file well. This page explains how `ww_tape` cuts a recording into
pieces and how the pieces stay checked as one chain. It then covers many pieces in one file, tracks of several owners,
each track's hash, what a copy of a file keeps, and how one item is addressed in all of them. It assumes [Tape
concepts](concepts.md) and [Writing a tape](writing.md).

## Why pieces

A piece is a complete tape. It has its own definitions, index, content hash and tail, and it reads without any other
piece. Cutting a recording into pieces gives three things:

- **Bounded loss.** A crash damages only the piece being written. The finished pieces before it are closed and
  hashed.
- **Bounded open time.** A reader opens a piece with a few small reads at its end, however long the recording is.
- **Simple retention.** Deleting the oldest piece frees space without rewriting anything. A chain whose first pieces
  were deleted reads as a "head cut", not as damage.

A `PieceChain` writes a chain of pieces and cuts by a `CutPolicy`: by bytes, by chunks or by age, always before a
chunk. It also cuts before any write that the piece's index has no room left for. The application chooses the size,
for example 256 MiB. On files (`ww_tape_fs::Files`), the pieces are named `00000000.wwt`, `00000001.wwt` and so on, so
that the names sort in chain order.

A piece reads alone because the writer measures near inside the piece. The first block of each track in a new piece
gets the track's definition and its clock put before it, even when the piece before held them a few bytes earlier.
The next put of a state item writes it again for the same reason. A reader of piece 7 alone therefore decodes
everything piece 7 holds.

## Ids and the chain

Every piece carries its ids in its head, its finish and its tail:

| Id | Size | What it names | Who makes it |
|---|---|---|---|
| piece id | 16 bytes | this piece | derived: the first piece's from the owner id and chain number, each next one from the piece before |
| owner id | 32 bytes | what the pieces belong to | the application; opaque to `ww_tape` |
| sequence number | 8 bytes | the piece's place in its chain, from 0 | the writer |

The **owner id** is the application's to choose. A recorder of single captures can use a hash of the capture's
settings. An application that keeps projects can use the project's id, so that every piece of a project names the
project. An owner can have several chains, each with its own number, such as one chain for events and one for a
high-rate stream. `PieceChain::create_on` (`ww_tape_fs::chain::create` for files) takes the owner id and the chain
number, so each chain gets its own piece ids and its own session tag.

The pieces of a chain are checked in both directions:

- **Backward.** Each piece head names the piece before it by its id and by its content hash, the BLAKE3 hash in that
  piece's finish block.
- **Forward.** A tail cannot name the next piece's hash, because that piece is not written yet. It names the next
  piece's id instead. The writer derives that id at the cut from the piece's own id and its content hash
  (`PieceId::after`), and the next piece head repeats it.

`ww_tape::piece::check_chain` checks the links. It takes a `PieceSummary` of each piece at hand; `PieceInfo::summary`
gives one for a piece that a `TapeReader` found. It puts the pieces in order and returns a `ChainReport`, which holds:

- each link between neighbours;
- whether the chain's start was deleted (`head_cut`);
- how the chain ends;
- the pieces that do not belong (`strays`): a second piece with one sequence number, or a piece of another owner;
- the pieces whose head is of an integrity level the reader does not handle (`refused`); every lookup in such a piece
  returns that level's error.

`ChainReport::is_whole` says whether all is well. A piece with a damaged head still has its place in the chain,
because its finish block names the piece as well. Its link to the piece before is then reported as checked from one
side only (`HeadLost`).

Each piece head also says, per track, which item number the piece starts with and the tick before which none of its
items start (`TrackStart`). A reader of one piece therefore knows where its items fall in the whole recording.

## Many pieces in one file

Pieces written back to back are one valid tape. A reader finds the last piece's tail at the end of the file, the
piece's start from the tail, and the previous piece's tail just before that start. It walks back through any number of
pieces with a few small reads each: the tail, the finish and the piece head. An unfinished piece after them, one being
written or one cut off by a crash, is found from its checkpoints or by a scan. The finished pieces before it are still
found by their tails.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/chain.svg"
<figcaption>Three pieces in one file. The reader finds the unfinished last piece from its newest checkpoint (1), then
the tail just before its start, which gives piece 1's length and so its start (2), and so on back to the start of the
file (3). Each piece head names the piece before it by that piece's content hash.</figcaption>
</figure>

For example, `cat 00000000.wwt 00000001.wwt > all.wwt` gives a file that any reader takes.

### Writing a chain into one file

An application chooses per chain whether each piece gets a file of its own or every piece goes into one file. A chain
of many files suits retention, since old pieces are deleted file by file. One file per recording suits an application
that keeps a recording as one document, such as a project file.

`ChainOptions::tapes` makes the choice:

- `PieceTapes::Each`, the default, writes `00000000.wwt`, `00000001.wwt` and so on.
- `PieceTapes::One` writes every piece into the one file the chain was created with. A cut then ends the piece and
  starts the next one right behind it in the same file (`TapeWriter::cut_in_place`). The bytes are the same as those
  of a chain of many files placed back to back.

In both cases one writer writes a file at a time.

```rust
let options = ChainOptions {
    tapes: PieceTapes::One,
    ..ChainOptions::default()
};
let mut chain = ww_tape_fs::chain::create(path, owner, 0, &[], options.clone())?;
chain.write_chunk(defs, head, columns)?;
let first = chain.finish()?;
// opened again: a new piece behind the finished one
let (mut chain, _) = ww_tape_fs::chain::resume(path, owner, 0, options)?;
chain.write_chunk(defs, head, columns)?;
let second = chain.finish()?;
```

The first session leaves one finished piece in the file. The second session finds it, adds a piece behind it, and
returns both pieces. The repository runs these lines as a test (`ww_tape_tests/tests/it/book.rs`).

To open the file again, call `ww_tape_fs::chain::resume` with the same options. The chain finds the last piece by the
tails, as a reader does. An unfinished last piece, cut off by a crash, is resumed under the same rules as a piece in a
file of its own ([Resuming after a crash](writing.md#resuming-after-a-crash)). After a last piece that was finished, a
new piece starts. Every session that opens the file and closes it therefore adds at least one piece.

The new piece has a session tag of its own, and its head names the last piece by its content hash and id, so
`check_chain` sees one linked chain. For example, an editor that opens a project, records for a minute and closes it
adds one piece each time.

## Tracks of several owners

Each track definition names an **owner**: `TrackDef::owner`, 32 bytes that `ww_tape` does not interpret. Zeros, the
default, mean the owner of the piece. The owner is not hashed into the track key or the [track
id](concepts.md#items-tracks-and-clocks), so two owners can have tracks with the same id. A reader tells such tracks
apart by id and owner together.

Two definitions with the same key and different owners are therefore two tracks. Each has its own item numbers, its
own rows in the piece head and the finish, and its own hash. For example, an application that records two devices of
the same type into one tape defines each device's tracks with the same keys, and with the device's id as the owner.

## A track's hash

Every track has a running hash, the **track hash** (`TrackHash`). It starts as 32 zero bytes and takes in each chunk
of the track and each of its records as they are written. Puts and copies, which repeat what is already on the tape,
are left out. The piece head states each track's hash before the piece, and the finish states it after the piece;
`Finished::tracks` reports it when a piece ends.

The hash depends only on the track's own data, not on where the data lies. A track written alone or among other
tracks, in one piece or many, in one file or many, ends with the same hash. Two recordings hold the same data for a
track exactly when their hashes match. A subset copy that keeps a track's chunks and records unchanged ends with the
original's hash. [The file format](format.md) lists the bytes that the hash takes in.

### The hash of a range

The hash of a part of a track works the same way. Folded from 32 zero bytes over a run of whole chunks, it depends
only on those chunks. `TapeReader::range_hash` returns it, as a `RangeHash`, for the chunks that hold any item of an
item range. A track's records are not chunks, so a range hash leaves them out. For example, a program that processed
items 1,000 to 50,000 of a track can record the range hash of its input. Anyone can later check that a file still
holds exactly that input.

The writer stores each chunk's share of the hash, its unit hash, in the chunk's index entry. The fold therefore reads
only the index, never a payload. A fold from the index gives the hash that the writer stated, checked by the index's
CRCs; it is not a check of the payloads. `RangeHash::stated` counts the chunks folded that way, and `RangeHash::read`
those hashed from their bytes. The strict check, `TapeReader::verify`, checks the stated hashes: the content hash,
each entry against its block, and each finish row against its track's fold. Every read of a chunk checks them too.

Where the index is damaged or missing, the reader reads the chunks and computes the same hash. A damaged chunk of the
track is an error there, as it is through the index. Bytes that no block explains are counted in
`RangeHash::skipped`, and a chunk written again folds once. The reader refuses index entries that name one block
twice or overlap (`IndexError::BadEntries`), from a final index, checkpoints or a scan alike, and reads the chunks
instead.

The fold also guards against a wrong index. An index entry of a kind that the reader does not fold is counted in
`RangeHash::skipped`, never dropped silently. A chunk that the fold reads must be the one its entry names, with the
range the entry states. This build writes and reads full integrity only: a chunk marked `FLAG_PAYLOAD_UNCHECKED` or an
index entry marked `ENTRY_UNHASHED` belongs to a reserved integrity level without unit hashes, and is refused.

### The 4 KiB end and cloning

A finished piece is a multiple of 4 KiB long. The writer pads the finish block with zeros up to the boundary, so the
tail, still the last bytes of the piece, ends on it. The padding is part of the finish block and its CRC. The content
hash covers the bytes before the finish, as it does without padding.

The boundary makes copies free on file systems that share data between files. On btrfs and XFS a range of one file can
be **cloned** into another, so that the new file points at the same disk blocks and nothing is copied. `cp --reflink`,
the `FICLONERANGE` call and `copy_file_range` clone; `std::io::copy` uses `copy_file_range` between files on Linux.
The kernel clones only ranges that start on a block boundary in both files, 4 KiB on common setups. Every finished
piece ends on a boundary, so the next piece starts on one, and pieces clone back to back into one file with nothing
between them.

Only the end of a piece is aligned. Blocks inside a piece stay where the writer put them, and an unfinished piece ends
where it ends until a resume finishes it on a boundary. A series file is aligned inside too, with every chunk's
payload on 4 KiB ([Writing a tape](writing.md#aligned-files)), so its payloads keep their alignment when the piece is
cloned to a 4 KiB multiple.

## Copies of a file

A copy of a whole file and a copy of some of its tracks differ in what stays the same.

A **whole copy** has the same bytes as the original. Every block, sequence number, index entry and content hash is
unchanged, so `TapeReader::verify` checks the copy against the hash its finish states. Comparing the hashes of two
copies proves them equal. On btrfs and XFS a whole copy can be a clone that shares the original's disk blocks.

A **subset copy** keeps only some tracks, so it is a new file with its own structure:

| Part | Whole copy | Subset copy |
|---|---|---|
| chunk and summary bodies: columns, item numbers, ticks | same | same |
| block headers: session tag, sequence number, CRC | same | new: the copy's own session and count |
| definitions and state records | same | the kept tracks' only, put by the copy's own writer near their use |
| index, finish, tail, content hash | same | new: they list and hash the copy's bytes |
| piece ids | same | new |

The items of a subset copy keep their item numbers and ticks, so an item is addressed the same way in the original and
in the copy. Only byte offsets and block numbers change. `ww_tape` has no copy command. To write a subset copy, an
application holds the kept tracks' definitions in a `Definitions`, puts their state, and writes the chosen blocks
through its own `TapeWriter`. It renumbers each block into the copy's session; `ww_tape::set_id` rewrites a block's id
and its CRCs.

## Addressing one item

An item is addressed by its track and its item number, never by its byte offset:

- The **track key** names the track in any file of the recording, whatever number the track has in that file.
- The **item number** counts the track's items from 0 over the whole recording. Each piece head states where every
  track's numbers continue, so the numbers run on across pieces.

A reader turns that address into bytes in three steps:

1. It finds the track's index entries. Each entry of a chunk gives the chunk's first item number and item count.
2. It reads the chunk whose range holds the item.
3. It reads the item in place inside the chunk, at its position: the item number less the chunk's first item number.

Offsets differ between a recording in many files and the same recording in one file, and between an original and a
subset copy; the address does not. An application that stores a reference to an item, such as a marker a user places,
therefore stores the track key and the item number. The reference stays valid when pieces are joined into one file or
copied.

Records in records blocks carry no item number. A reader addresses them by their track and tick, or within one file by
their block and their place in it. A state item is also known by its hash, which [Reading a
tape](reading.md#state-at-a-place) uses to find it.
