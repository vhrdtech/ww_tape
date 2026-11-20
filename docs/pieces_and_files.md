# Pieces and files

A recording that runs for days does not fit one file well. This page explains how `ww_tape` cuts a recording into pieces
and how the pieces stay checked as one chain.

It then covers many pieces in one file, the end of a piece, copies of a file, and the address of one item. It assumes
[Tape concepts](concepts.md) and [Writing a tape](writing.md).

[Track owners and hashes](track_hashes.md) covers tracks of several owners and the hash of a track.

## Why pieces

A piece is a complete tape. It has its own definitions, index, content hash and tail, and it reads without any other
piece.

Cutting a recording into pieces gives three things:

- **Bounded loss.** A crash damages only the piece being written. The closed pieces before it are closed and hashed.
- **Bounded open time.** A reader opens a piece with a few small reads at its end, however long the recording is.
- **Simple retention.** Deleting the oldest piece frees space without rewriting anything. A chain whose first pieces
  were deleted reads as a "head cut", not as damage.

A `PieceChain` writes a chain of pieces and cuts by a `CutPolicy`: by bytes, by chunks or by age, always before a chunk.
It also cuts before any write that the piece's index has no room left for.

The application chooses the size, for example 256 MiB. On files (`ww_tape_fs::Files`), the pieces are named
`00000000.wwt`, `00000001.wwt` and so on, so that the names sort in chain order.

A piece reads alone because the writer measures near inside the piece. In a new piece, the writer puts each track's
definition and clock before the track's first block.

It does so even when the piece before held them a few bytes earlier. The next put of a state item writes the item again
for the same reason.

A reader of piece 7 alone therefore decodes everything piece 7 holds.

## Ids and the chain

Every piece carries its ids in its head, its close and its tail:

| Id | Size | What it names | Who makes it |
|---|---|---|---|
| piece id | 16 bytes | this piece | derived: the first piece's from the owner id and chain number, each next one from the piece before |
| owner id | 32 bytes | what the pieces belong to | the application; opaque to `ww_tape` |
| sequence number | 8 bytes | the piece's place in its chain, from 0 | the writer |

The **owner id** is the application's to choose. A recorder of single captures can use a hash of the capture's settings.

An application that keeps projects can use the project's id, so that every piece of a project names the project. An
owner can have several chains, each with its own number, such as one chain for events and one for a high-rate stream.

`PieceChain::create_on` (`ww_tape_fs::chain::create` for files) takes the owner id and the chain number. Each chain
therefore gets its own piece ids and its own session tag.

### Checking the links

`ww_tape` checks the pieces of a chain in both directions:

- **Backward.** Each piece head names the piece before it by its id and by its content hash. The content hash is the
  hash in that piece's close record, made with BLAKE3 (a cryptographic hash function).
- **Forward.** A tail cannot name the next piece's hash, because that piece is not written yet, so it names the next
  piece's id. The writer derives that id at the cut from the piece's own id and content hash (`PieceId::after`), and the
  next piece head repeats it.

### The chain report

`ww_tape::piece::check_chain` checks the links. It takes a `PieceSummary` of each piece at hand; `PieceInfo::summary`
gives one for a piece that a `TapeReader` found.

It puts the pieces in order and returns a `ChainReport`, which holds:

- each link between neighbours;
- whether the chain's start was deleted (`head_cut`), or removed on purpose (`head_removed`, the count of pieces);
- how the chain ends;
- the pieces that do not belong (`strays`): a second piece with one sequence number, or a piece of another owner;
- the pieces whose head is of an integrity level the reader does not handle (`refused`); every lookup in such a piece
  returns that level's error;
- the pieces the closes at hand name as removed on purpose (`removed`).

`ChainReport::is_whole` says whether all is well.

A piece with a damaged head still has its place in the chain, because its close record names the piece as well. Its link
to the piece before is then reported as checked from one side only (`HeadLost`).

Each piece head also says, per track, which [item number](concepts.md#items-tracks-and-clocks) the piece starts with. It
also gives the tick before which none of the piece's items start (`TrackStart`).

A reader of one piece therefore knows where its items fall in the whole recording.

### Pieces removed on purpose

An application may delete pieces from the middle of a chain on purpose, for example to drop a stretch a user cut out.
Without a note, the hole reads as a gap: pieces missing, as after a lost file.

The note is a list of the removed pieces in a close record. Each row gives the piece's sequence number, its id and the
content hash of its close in force (`RemovedPiece`).

The writer appends the note to the chain's last piece, after its close, as any [append](format.md#closing-more-than-once)
does:

1. Resume the closed last piece (`TapeWriter::resume_on`, or `ww_tape_fs::writer::append` for a file).
2. Name the removed pieces with `TapeWriter::record_removed`.
3. Close it. The new close names the pieces, and every later close of the piece names them again.

`check_chain` then links over the hole: `ChainLink::Removed { pieces }`. It checks both ends of the hole by the rows:

- the piece before the hole names the first removed piece's id;
- the piece after the hole names the last removed piece's content hash and id.

A row that disagrees with either end reads as `Broken`. A hole that the rows do not cover whole reads as a gap.

Only the ends are checked: no piece at hand names the rows between them, or the first row's hash. A hole of two
pieces or more therefore links whatever those rows say.

A chain whose first pieces were removed and named reads as removed (`head_removed`), not as head cut. The rows must
then name every piece from sequence number 0 on.

`ChainReport::is_whole` takes removed pieces as part of a whole chain.

The rows live in the piece that holds them. To remove that piece later too, name its rows again on the new last piece.
`ChainReport::removed` lists every row the closes at hand name.

Rows go on the last piece and name only pieces before it. The last pieces of a chain therefore cannot be named as
removed.

The writer refuses:

- a row for a piece that is not before its own;
- a second row for one piece with another id or hash;
- more than `REMOVED_PIECES_MAX` (65536) rows in one close.

## Many pieces in one file

Pieces written back to back are one valid tape. A reader walks such a file from its end:

1. It finds the last piece's tail at the end of the file.
2. It finds the piece's start from the tail.
3. It finds the previous piece's tail just before that start, and goes on.

Each piece costs a few small reads: the tail, the close and the piece head. The walk therefore goes back through any
number of pieces.

An unclosed last piece, still being written or cut off by a crash, is found from its checkpoints or by a scan. The
closed pieces before it are still found by their tails.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/chain.svg"
<figcaption>Three pieces in one file. The reader finds the unclosed last piece from its newest checkpoint (1), then
the tail just before its start (2). That tail gives piece 1's length and so its start, and the walk goes on back to
the start of the file (3).</figcaption>
</figure>

For example, `cat 00000000.wwt 00000001.wwt > all.wwt` gives a file that any reader takes.

### Writing a chain into one file

An application chooses per chain whether each piece gets a file of its own or every piece goes into one file. A chain of
many files suits retention, since old pieces are deleted file by file.

One file per recording suits an application that keeps a recording as one document, such as a project file.

`ChainOptions::tapes` makes the choice:

- `PieceTapes::Each`, the default, writes `00000000.wwt`, `00000001.wwt` and so on.
- `PieceTapes::One` writes every piece into the one file the chain was created with. A cut then ends the piece and
  starts the next one right behind it in the same file (`TapeWriter::cut_in_place`).

With `PieceTapes::One`, the bytes are the same as those of a chain of many files placed back to back. In both cases one
writer writes a file at a time.

```rust
let options = ChainOptions {
    tapes: PieceTapes::One,
    ..ChainOptions::default()
};
let mut chain = ww_tape_fs::chain::create(path, owner, 0, &[], options.clone())?;
chain.write_chunk(defs, head, columns)?;
let first = chain.close()?;
// opened again: written on after the close
let (mut chain, _) = ww_tape_fs::chain::resume(path, owner, 0, options)?;
chain.write_chunk(defs, head, columns)?;
let second = chain.close()?;
```

The first session leaves one closed piece in the file. The second session finds it, writes on after its close and
closes it again.

The file still holds one piece, closed twice: `first` and `second` each report that piece as closed at the time.

The repository runs these lines as a test (`ww_tape_tests/tests/it/book.rs`).

To open the file again, call `ww_tape_fs::chain::resume` with the same options. The chain finds the last piece by the
tails, as a reader does.

The chain resumes an unclosed last piece, cut off by a crash, under the rules for a piece in a file of its own.
[When the power fails](crashes.md#resuming-the-file) gives those rules.

A last piece that was closed is written on after its close, behind an append head, in the session that wrote it. Nothing
before the append head changes, and the new close names the one before it ([Closing more than
once](format.md#closing-more-than-once)).

For example, an editor that opens a project, records for a minute and closes it adds a close to the last piece each
time. The file gains a piece only where the chain cuts.

## The 4 KiB end and cloning

A closed piece is a multiple of 4 KiB long. The writer pads the close record with zeros up to the boundary, so the
tail, still the last bytes of the piece, ends on it.

The padding is part of the close record and its cyclic redundancy check (CRC). The content hash covers the bytes before
the close, as it does without padding.

The boundary makes copies free on file systems that share data between files. On btrfs and XFS (a Linux file system), a
range of one file can be **cloned** into another.

The new file then points at the same disk blocks, and nothing is copied. `cp --reflink`, the `FICLONERANGE` call and
`copy_file_range` clone; `std::io::copy` uses `copy_file_range` between files on Linux.

The kernel clones only ranges that start on a block boundary in both files, 4 KiB on common setups. Every closed piece
ends on a boundary, so the next piece starts on one.

Pieces therefore clone back to back into one file with nothing between them.

Only the end of a piece is aligned. Blocks inside a piece stay where the writer put them.

An unclosed piece ends where it ends, until a resume closes it on a boundary.

A series file is aligned inside too, with every chunk's payload on 4 KiB ([Writing a tape](writing.md#aligned-files)).

The payloads of such a file therefore keep their alignment when the piece is cloned to a 4 KiB multiple.

## Copies of a file

A copy of a whole file and a copy of some of its tracks differ in what stays the same.

A **whole copy** has the same bytes as the original. Every block, sequence number, index entry and content hash is
unchanged.

`TapeReader::verify` therefore checks the copy against the hash its close record states. Comparing the hashes of two copies
proves them equal.

On btrfs and XFS, a whole copy can be a clone that shares the original's disk blocks.

A **subset copy** keeps only some tracks. It is therefore a new file with its own structure:

| Part | Whole copy | Subset copy |
|---|---|---|
| chunk and summary bodies: columns, item numbers, ticks | same | same |
| block headers: session tag, sequence number, CRC | same | new: the copy's own session and count |
| definitions and state records | same | the kept tracks' only, put by the copy's own writer near their use |
| index, close, tail, content hash | same | new: they list and hash the copy's bytes |
| piece ids | same | new |

The items of a subset copy keep their item numbers and ticks. An item is therefore addressed the same way in the
original and in the copy; only byte offsets and block numbers change.

`ww_tape` has no copy command. To write a subset copy, an application works through its own `TapeWriter`:

1. It holds the kept tracks' definitions in a `Definitions`.
2. It puts their state.
3. It writes the chosen blocks through the writer.

The application renumbers each block into the copy's session. `ww_tape::set_id` rewrites a block's id and its CRCs.

## Addressing one item

An item is addressed by its track and its item number, never by its byte offset:

- The **track key** names the track in any file of the recording, whatever number the track has in that file.
- The item number counts the track's items from 0 over the whole recording. Each piece head states where every track's
  numbers continue, so the numbers run on across pieces.

A reader turns that address into bytes in three steps:

1. It finds the track's index entries. Each entry of a chunk gives the chunk's first item number and item count.
2. It reads the chunk whose range holds the item.
3. It reads the item in place inside the chunk, at its position: the item number less the chunk's first item number.

Offsets differ between a recording in many files and the same recording in one file, and between an original and a
subset copy. The address does not differ.

An application that stores a reference to an item, such as a marker a user places, therefore stores the track key and
the item number. The reference stays valid when pieces are joined into one file or copied.

Records in records blocks carry no item number. A reader addresses them by their track and tick, or within one file by
their block and their place in it.

A state item is also known by its hash, which [Reading a tape](reading.md#state-at-a-place) uses to find it.
