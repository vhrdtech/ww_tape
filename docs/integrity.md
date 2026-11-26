---
level: basic
---

# Fewer checks, sealed later

A tape's checks cost a writer time. A writer that must keep up with a fast source, such as a radio receiver, can leave
some of them out. A seal adds them later. [When bytes go bad](damage.md) says what each check catches.

## Three integrity levels

A writer can skip the hashes, or the payload CRCs too. Each [[piece]] names its [[integrity level]] in its
[[piece head]]. [`WriterOptions::integrity`] sets the level of the pieces a writer starts.

| Level | The writer skips | A reader still checks |
|---|---|---|
| `Full` | nothing | every CRC and the [[content hash]] |
| `CrcOnly` | the content hash, the unit hashes in the index and the [[track hash|track hashes]] | every CRC |
| `None` | also the CRCs over chunk payloads | block CRCs, chunk head CRCs and every structure block |

Below `Full`, the fields of the hashes stay in place and hold zero, and the piece head's level says that they are
absent. A chunk of `None` sets header bit 3, which says that its CRC covers the head alone. A reader reports such a
chunk as [`Checked::Unprotected`], and [`ChainLink::Unproven`] marks a link between pieces that no content hash
proves. The bench `levels` writes chunks into a memory tape at each level:

--8<-- "docs/plots/bench_levels_write.txt"

A 1 MiB series chunk takes 35 % of the time of `Full` at `CrcOnly` and 26 % at `None`. What is left at `None` is mostly
the copy into memory.

## The seal

A seal raises a closed piece to `Full` later, at save time or whenever there is time. It computes what the writer
skipped and writes every block in its place, so the piece keeps its length and its offsets. The sealed piece holds the
bytes a writer at `Full` writes from the same calls.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/seal.svg"
<figcaption>A piece written at <code>None</code> and the same piece sealed. The seal fills the fields the writer left
at zero and changes no offset, so the sealed piece reads as one written at <code>Full</code>.</figcaption>
</figure>

- [`MemPieces::seal`] swaps a sealed copy in, while a reader holding the old tape reads on.
- [`ww_tape_fs::writer::seal`] seals a file in place and writes each piece head last. A crash before that leaves the
  piece at its old level, and a seal again finishes it.
- A chain is sealed from its first piece on, since each piece's id comes from the content hash of the piece before.

--8<-- "docs/plots/bench_levels_seal.txt"

Sealing a 1 MiB series chunk of `None` takes about as long as writing it at `Full`: 110 % of that time. The seal reads
the piece, checks it and hashes it, and copies it into memory again.

--8<-- "docs/plots/bench_levels_machine.txt"
