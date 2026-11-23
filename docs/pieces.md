---
level: basic
---

# Pieces and chains

A recording that runs for days does not fit one file well. A crash costs only the bytes it touched, in a file of any
size. A file of hundreds of gigabytes, though, is slow to copy, and its old data cannot go without rewriting the rest.
How does a [[writer]] cut a long recording into [[pieces]], how do the pieces find each other, and how do old ones go?
The examples cut the drive's hour every 15 minutes into four pieces.

## Why pieces

A [[piece]] is a complete [[tape]]: it has its own [[definitions]], index, [[content hash]] and [[tail]], and it reads
without any other piece. A [[chain]] is the pieces of one recording in order, each [[piece head]] naming the piece
before it. Cutting a recording into a chain gives three things:

- **Files of a handy size.** A piece of 256 MiB copies, uploads or moves to an archive on its own. A tool that needs
  one stretch of the recording takes only the pieces that hold it.
- **Simple retention.** Deleting the oldest piece frees space without rewriting anything.
- **Closed history.** The pieces before the one being written are closed and hashed. A later crash or a bug in the
  writer cannot touch them.

A crash inside a piece costs only the bytes it touched there, as [When the power fails](crashes.md) shows. A chain is
therefore not what bounds the loss. A [[reader]] also opens a piece of any size with a few small reads at its end.

A piece reads alone because the writer measures [[near]] inside the piece. A new piece carries the definition of every
track the chain has used, even when the piece before held it a few bytes earlier. It also carries each [[state track]]'s
newest [[state item]]. A reader of piece 7 alone therefore decodes everything piece 7 holds.

## Pieces know each other

Each piece says by itself where it belongs. Its [[piece head]] holds its id, the [[owner id]] and its [[piece number]],
which counts from 0 in each chain. The head also names the piece before it by that piece's [[content hash]] and
id. The [[close]] names the next piece's id. The ids follow from the owner id and the chain number, so a reader tells
two chains apart.

A program therefore never trusts a file's name. Files renamed by hand, copied into one directory from several disks or
listed in any order still form their chain. A scan opens each file from its end, a few small reads, and learns which
piece it is and which [[items]] it holds. [`check_chain`] then orders the pieces and checks every link both ways:

```rs
--8<-- "ww_tape_tests/tests/it/chains.rs:renamed"
```

The scan needs no index of the directory and no database beside the files. The close of each piece lists every track it
holds with its first and last [[item number]] and [[tick]]. A viewer thus knows which file to open for a given second
before it reads any data.

## A chain in a directory

A [`PieceChain`] writes a chain and [[cuts|cut]] it by itself. On files, [`ww_tape_fs::chain::create`] makes one file
per piece in a directory, named `00000000.wwt`, `00000001.wwt` and so on, so that the names sort in chain order. The
test below cuts a piece once it holds 16 KiB:

```rs
--8<-- "ww_tape_tests/tests/it/chains.rs:create"
```

The chain takes an [[owner id]] and a chain number. The owner id is a 32-byte id that the application chooses and
`ww_tape` does not interpret, such as a project's id. One owner may have several chains, such as one for events and one
for a high-rate stream. Each chain then gets its own piece ids and [[session tag]]. Tracks of several owners can share
one chain ([Track owners and hashes](track_hashes.md#tracks-of-several-owners)).

### When to cut

A [`CutPolicy`] has three limits, and the first one reached cuts:

| Limit | Cuts when the piece holds |
|---|---|
| `bytes` | that many bytes, such as 256 MiB |
| `chunks` | that many [[chunks]] |
| `interval` | data for that long, counted from its start or its resume |

A limit of 0 or `None` is off. The chain checks the policy before each chunk, so a cut never comes after the last chunk
and never leaves a piece without data. The chain also cuts before a write that the piece's index has no room left for.
[`PieceChain::cut_now`] cuts at once.

A cut by age makes the file's bytes depend on the clock. The drive's recorder counts chunks instead, as many as 15
minutes of the run make:

```rs
--8<-- "examples/example_drive/src/record.rs:options"
```

??? example "What the example uses here: `Drive`"

    ```rs
    --8<-- "examples/example_drive/src/lib.rs:drive_item"
    ```

A recorder that writes a [`PieceChain`] itself and must know in which piece a chunk lands asks [`PieceChain::cut_due`]
and cuts first with [`PieceChain::cut_now`]. A [`TapeMux`] does so on its [[writing thread]] before it places each
chunk. A chunk thus lies whole in one piece, and its [[handle]] never notices the cut. [`TapeMux::cut`] cuts at once.
The drive's recorder only starts the mux with these options, and hands it the acts through its handles:

```rs
--8<-- "examples/example_drive/src/record.rs:create"
```

??? example "What the example uses here: `Act`, `Recorder`"

    ```rs
    --8<-- "examples/example_drive/src/record.rs:act_item"

    --8<-- "examples/example_drive/src/record.rs:recorder_item"
    ```

### Nothing waits for a cut

Durability comes from the [[sync policy]] and the [[checkpoints]], which work inside a piece. A cut only bounds how
large one file grows. A slow input of one log line a minute goes into a [[records block]] as it comes. The writer syncs
it within the sync interval, and a reader of the unclosed piece reads it at once.

## What a cut writes

A [[cut]] ends the current piece and starts the next one. [`TapeWriter::cut_with`] does it in five steps:

1. It places every [[deferred block]] still waiting.
2. It writes the [[final index]], the [[close]] and the [[tail]].
3. It closes the sink; a file takes its final name.
4. It starts the next piece in a new sink, which the [[store]] makes under the next key.
5. It puts the definitions of every track that the new head carries, from its caller's [`struct@Definitions`].

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/cut.svg"
<figcaption>A cut. Piece 0 ends with its final index, its close padded to the 4 KiB end, and its tail. Piece 1 starts
with its head and two blocks of puts that carry the definitions, before its first data. Drawn by
<code>ww_tape_figures</code> from the drive's chain at 1/1000 of its data; each row has its own scale.</figcaption>
</figure>

At full rate, piece 0 of the drive ends with a final index of 415 kB. The close takes 1198 bytes, 322 of them padding,
and the tail 164 bytes. Piece 1 starts with a head of 636 bytes, then 2 blocks of puts of 623 bytes in all.

The head lists every track the chain has used so far, a [[state track]] included. The puts after it carry their
definitions, and the mode's newest item. [The definitions a new piece carries](carried_definitions.md) says which
definitions a piece carries and what they cost.

The four pieces of the hour end on 4 KiB:

| Piece | Bytes | Close padding |
|---|---:|---:|
| 0 | 29343744 | 322 |
| 1 | 29560832 | 374 |
| 2 | 29556736 | 3849 |
| 3 | 26836992 | 3565 |

A [`TapeWriter`] without a chain cuts the same way. [`TapeWriter::cut_on`] takes a sink the caller made, and
[`writer::cut`] the path of the next file. Only a piece of a chain can be cut: such a piece starts with a piece head,
which [`TapeWriter::begin_piece`] writes. A piece takes data of a bounded number of tracks, and a head carries a bounded
number of rows ([Tape pieces](format.md#tape-pieces)).

## Deleting old pieces

Retention by file deletes the oldest piece. Every piece left reads alone, and the chain reads as **head cut**: its start
was deleted, not damaged.

```rs
--8<-- "ww_tape_tests/tests/it/chains.rs:delete"
```

The drive's chain without piece 0 shows it. [`check_chain`] finds 3 pieces from piece 1, and both links between them
hold. The report says head cut, and so not whole. Piece 1 defines every track at its start, the carried ones. A chain
with deleted pieces cannot be resumed, since a resume starts from the first piece.

A chain in memory keeps a bounded window the same way. [`MemTapes::remove`] takes a closed piece out of the store. A
reader that already holds the piece reads on unchanged. The piece still being written is not removed
([`MemError::Unclosed`]).

A chain in one memory tape keeps its window with [`MemTapes::drop_front`]. The call frees the oldest pieces whole, in blocks of
memory of up to 64 MiB. Offsets don't move: the tape now starts at its first kept piece
([`ByteSource::start`]). A reader opened on it finds the pieces from there, as a head cut. The piece being
written stays, and a read of a dropped piece fails with [`MemError::Dropped`].

### Pieces removed on purpose

An application may delete pieces from the middle of a chain on purpose, such as a stretch a user cut out. Without a
note, the hole reads as pieces lost. The note is a list of the removed pieces in a close: each row gives one removed piece's
[[piece number]], id and content hash ([`RemovedPiece`]).

The writer appends the note to the chain's last piece, after its close:

1. Resume the closed last piece ([`TapeWriter::resume_on`], or [`writer::resume`] for a file). An append would start a
new piece, which cannot name removed pieces.
2. Name the removed pieces with [`TapeWriter::record_removed`].
3. Close it. The new close names the pieces, and every later close of the piece names them again.

[`check_chain`] then links over the hole as [`ChainLink::Removed`], and checks both ends of the hole by the rows. A row
that disagrees with either end reads as broken. A chain whose first pieces were named this way reads as removed, not as
head cut. [The chain report](format.md#the-chain-report) gives the rules the writer enforces on the rows.

## Writing a chain into one file

A chain of many files suits retention, since old pieces go file by file. One file per recording suits an application
that keeps a recording as one document, such as a project file. [`ChainOptions::tapes`] makes the choice:

- [`PieceTapes::Each`], the default, writes a file per piece.
- [`PieceTapes::One`] writes every piece into the one file the chain was created with. A cut then starts the next piece
  right behind the closed one ([`TapeWriter::cut_in_place`]).

The bytes are the same as those of a chain of many files placed back to back. A reader walks such a file from its end.
The last tail gives the last piece's start, and the tail just before that start gives the piece before.

```rs
--8<-- "ww_tape_tests/tests/it/book.rs:one_file"
```

The first session leaves one closed piece in the file. The second finds it by the tails, writes on after its close and
closes it again. The file still holds one piece, closed twice, and gains a piece only where the chain cuts. An unclosed
last piece, cut off by a crash, resumes as [When the power fails](crashes.md#resuming-a-chain) describes.

## The 4 KiB end and cloning

A closed piece is a multiple of 4 KiB long: the writer pads the close with zeros up to the boundary. The boundary makes
cloning free on file systems that share data between files. The Linux file systems Btrfs and XFS can **clone** a range
of one file into another. The new file then points at the same disk blocks.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/reflink.svg"
<figcaption>Three piece files cloned back to back into one file. Each piece ends on 4 KiB, so the next starts on a
boundary, and the new file shares the disk blocks of the three.</figcaption>
</figure>

The kernel clones only ranges that start on a block boundary in both files, 4 KiB on common setups. `cp --reflink`, the
`FICLONERANGE` call and `copy_file_range` clone, and `std::io::copy` uses `copy_file_range` between files on Linux.
Every closed piece ends on a boundary, so pieces clone back to back with nothing between them. In an [aligned
file](writing.md#aligned-files), the payloads keep their alignment too.

## Clones and subsets of a file

A **clone** of a file has the same bytes as the original: every block, index entry and content hash.
[`TapeReader::verify`] checks it against the hash its close states. A **subset** keeps only some tracks, so it is a new
file:

| Part | Clone | Subset |
|---|---|---|
| chunk and summary bodies: columns, item numbers, ticks | same | same |
| block headers: session tag, sequence number, CRC | same | the subset's own |
| definitions and state records | same | the kept tracks' only, put by the subset's writer |
| index, close, tail, content hash | same | new |
| piece ids | same | new |

The items of a subset keep their [[item numbers]] and [[ticks]], so a reference to an item stays valid. To write one, an
application holds the kept tracks' definitions, puts their state and writes the chosen blocks through its own writer. It
renumbers each block into the subset's session with [`ww_tape::set_id`].
