---
level: nomicon
---

# The definitions a new piece carries

A chain is read from its oldest piece left. Dropping the pieces before it must not lose what a track is. So a new piece
carries the definitions of the tracks it holds no data for. This chapter has two pages:

- this page: which definitions a new piece carries, in which blocks, the state it carries beside them and what that
  costs;
- [When carried definitions don't fit](carried_room.md): the room the index policy leaves them, and what a cut leaves
  out past it.

A piece head carries a row for every track that gave data or a state in the chain so far. The writer puts the
definition of each of these tracks right after the head, the layout and the producer. The clocks, maps and bundles it
needs come with it. A quiet track, one with no data in the new piece, so keeps its kind, its clock and its label there.

Older pieces may be dropped or removed. A reader of the chain then finds, in the oldest piece left, the definition of
each track that `defs` held at the cut:

- a track that gave data before that piece: right after its head;
- a track that gives data first in a later piece: beside its first data, as before.

Some tracks have no definition there:

- A track that never gave data or a state has no row and no definition in any piece.
- A track closed in `defs` (`Definitions::close`) keeps its row but no definition, and its row is not marked: dropping
  the pieces before loses it.
- A row whose number `defs` holds under another id or owner is passed over, since that definition is another track's.

## The blocks they come in

The carried definitions come in blocks of at most `CARRIED_BLOCK_LEN` (4 KiB):

- First the bundles, each once. A bundle longer than a block has a block of its own.
- Then the tracks, grouped by the clocks and maps they count through. Each block holds its group's clocks and maps
  again, then as many tracks as fit.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/carried_blocks.svg"
<figcaption>The start of a new piece. Its carried definitions come in blocks of at most 4 KiB, the bundles first, then
one block per group of clocks. Each block repeats the clocks and maps its tracks count through, so one damaged block
costs only its own tracks.</figcaption>
</figure>

One damaged block so costs only its own tracks, never another block's clock. A lost bundle block costs its tracks their
bundle.

A track defined twice at once ([format](format.md#definitions-written-twice-at-once)) loses nothing to one damaged
block. Its carried definitions go into a block of their own, and a copy of that block follows the run the piece
head's puts start.

## State tracks

A state put once and never again lies only in the piece that holds it. So a cut carries more for a state track: its
newest item, with its time, right after its definition in the same block. A reader of the new piece alone finds the
state in force with [`TapeReader::state_at`], as it finds the track.

The carried item counts as a record of the item in the piece. A put of the same item close behind writes nothing. Its
first copy comes 4 KiB on, with the next block. Its second comes at a use 1 MiB after that, as after any first record.

The writer keeps the newest item of each state track for this, learned from every state record it places:

| Case | The writer |
|---|---|
| the kept items pass `NearPolicy::kept_bytes_max` (64 MiB by default) | forgets the item placed longest ago |
| a chain resumes | learns the items of its last closed piece and of the piece it resumes |
| a cut finds no item for a state track | carries its definition alone, counted in `Closed::states_unknown`; the next put writes the item |

`just bench-carried` measures what a carried state costs in two chains:

- a chain that puts each of `n` state tracks once in its first piece and cuts, so the next piece holds one records
  block;
- the baseline: the same chain with `n` stream tracks that gave data, which carries the definitions alone.

--8<-- "docs/plots/bench_carried_state.txt"

Pieces end on 4 KiB, so the totals step by 4096 bytes. Most of the cost per state is its index entries, in the
checkpoint and the final index, not the item.

## What they cost

The cost per piece comes from `just bench-carried`, in this setup:

- records blocks of 4000 bytes go on one track, and the others stay quiet;
- each track is labeled `channel NNNNNNN`;
- the piece is cut by bytes.

A piece holds less data, and its final index grows past the cut's bound, which counts no final index:

--8<-- "docs/plots/bench_carried_pieces.txt"

Each definition costs its record and its index entries, in the checkpoint that lists it and in the final index. At the
track caps, a piece of one records block grows by the bytes of the next table:

--8<-- "docs/plots/bench_carried_caps.txt"

A piece cut by bytes below what its carried definitions take holds them and one chunk: each cut comes due right after
the head.

A resume that keeps only the piece head (a crash right after the cut) leaves the definitions out. The next call given
`defs` puts them first, with the states the chain's last closed piece holds. The piece so gets the bytes of a cut that
never stopped. A piece closed before any such call carries none: `close` takes no `defs`, so the piece before it keeps
the only copy.

Past the room the index policy leaves them, a cut carries fewer definitions: [When carried definitions don't
fit](carried_room.md).
