---
level: basic
---

# Deleting old pieces

A recorder that runs for weeks fills its disk unless old data goes. A [[chain]] makes that cheap: each [[piece]] is a
file of its own, and deleting the oldest one frees its space without rewriting anything. How does a [[reader]] tell
pieces deleted on purpose from pieces lost, and how are pieces removed while the chain is still written?

## Retention from the front

Retention by file deletes the oldest piece. Every piece left reads alone, and the chain reads as **head cut**: its
start was deleted, not damaged.

```rust
--8<-- "ww_tape_tests/tests/it/chains.rs:delete"
```

The drive's chain without piece 0 shows it. [`check_chain`] finds 3 pieces from piece 1, and both links between them
hold. The report says head cut, and so not whole. Piece 1 defines every track at its start, the carried ones.

A resume ([`PieceChain::resume_on`], or [`chain::resume`] for files) starts at the lowest piece there, as a reader
does. Retention and a writer that goes on after a restart thus work together.

A chain in memory keeps a bounded window the same way, in one of two shapes:

- A chain of memory pieces drops a closed piece with [`MemPieces::remove`]. A reader that already holds the piece
  reads on unchanged. The piece still being written is not removed ([`MemError::Unclosed`]).
- A chain in one memory tape drops its front with [`MemPieces::drop_front`]. The call frees the oldest pieces whole,
  in blocks of memory of up to 64 MiB, and offsets don't move. The tape then starts at its first kept piece
  ([`ByteSource::start`]). A reader opened on it finds the pieces from there as a head cut. The piece being written
  stays, and a read of a dropped piece fails with [`MemError::Dropped`].

## Pieces removed on purpose

An application may delete pieces from the middle of a chain on purpose, such as a stretch that a user cut out.
Without a note, the hole reads as pieces lost. The note is a list of the removed pieces in a [[close]]. Each row gives
one removed piece's [[piece number]], id and [[content hash]] ([`RemovedPiece`]).

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/removed.svg"
<figcaption>Two ways pieces go. Retention deletes the front and leaves no row, so the chain reads as head cut. A piece
removed from the middle gets a row in a later close, and the chain links over the hole by that row.</figcaption>
</figure>

The writer appends the note to the chain's last piece, after its close:

1. Resume the closed last piece ([`TapeWriter::resume_on`], or [`writer::resume`] for a file). An append would start
   a new piece, which cannot name removed pieces.
2. Name the removed pieces with [`TapeWriter::record_removed`].
3. Close the piece. The new close names the pieces, and every later close of the piece names them again.

[`check_chain`] then links over the hole as [`ChainLink::Removed`] and checks both ends of the hole against the rows.
A row that disagrees with either end reads as broken. A chain whose first pieces were named this way reads as removed,
not as head cut. [The chain report](format.md#the-chain-report) gives the rules the writer enforces on the rows.

A resume walks over such a hole the same way. Any other hole stops it with [`ResumeError::Gap`], which names the
missing [[piece numbers|piece number]]. The writer never goes on after pieces that may have been lost.

## Removing pieces while the chain is written

[`PieceChain::remove`] removes closed pieces of a chain that is being written, in one call. The order of its steps
matters, since a crash can come between any two of them:

1. It names each removed piece that comes after a piece that stays, in the piece being written. It names again the
   rows that a removed piece held for a hole removed before.
2. It closes that piece and syncs it, then writes on after the close ([`TapeWriter::close_and_go_on`]).
3. It takes the pieces out of the store, oldest first ([`PieceStore::remove`]): a file is deleted and its directory
   synced.

```rust
--8<-- "ww_tape_tests/tests/it/chain_removal.rs:remove"
```

A crash before the third step leaves pieces at hand that the rows name. A row for a piece still there plays no part in
a link. A crash part way through the third step leaves a hole that the rows cover. Either way the chain reads whole and
resumes:

```rust
--8<-- "ww_tape_tests/tests/it/chain_removal.rs:resume"
```

The pieces before every piece that stays get no row: they go as retention does, and the chain reads as head cut. Rows
for them would only pile up, while a head cut already says that the start was deleted. In a chain in one memory tape
only those pieces go, as the tape's front ([`MemPieces::drop_front`]). A piece from the middle of a tape is refused, as
is the piece being written.
