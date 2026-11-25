---
level: nomicon
---

# The definitions a new piece carries

A chain is read from its oldest piece left. Dropping the pieces before it must not lose what a track is. So a new piece
carries the definitions of the tracks it holds no data for.

A piece head carries a row for every track that gave data or a state in the chain so far. The writer puts the definition of each of
these tracks right after the head, the layout and the producer. The clocks, maps and bundles it needs come with it. A
quiet track, one with no data in the new piece, so keeps its kind, its clock and its label there.

Older pieces may be dropped or removed. A reader of the chain then finds, in the oldest piece left, the definition of
each track that `defs` held at the cut:

- a track that gave data before that piece: right after its head;
- a track that gives data first in a later piece: beside its first data, as before.

Some tracks have no definition there:

- A track that never gave data or a state has no row and no definition in any piece.
- A track closed in `defs` (`Definitions::close`) keeps its row but no definition, and its row is not marked: dropping
  the pieces before loses it.
- A row whose number `defs` holds under another id or owner is passed over, since that definition is another track's.

The carried definitions come in blocks of at most `CARRIED_BLOCK_LEN` (4 KiB):

- First the bundles, each once. A bundle longer than a block has a block of its own.
- Then the tracks, grouped by the clocks and maps they count through. Each block holds its group's clocks and maps
  again, then as many tracks as fit.

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
copy comes at a use 1 MiB on, as after any first record.

The writer keeps the newest item of each state track for this, learned from every state record it places. The items
take at most `NearPolicy::kept_bytes_max` (64 MiB by default); past it, the item placed longest ago is forgotten. A
resumed chain learns the items of its last closed piece and of the piece it resumes. A cut that finds no item for a
state track carries its definition alone, and `Closed::states_unknown` counts such tracks. The next put of the track
writes its item.

`just bench-carried` measures what a carried state costs. A chain puts each of `n` state tracks once in its first piece
and cuts; the next piece holds one records block. The same chain with `n` stream tracks that gave data is the
baseline, which carries the definitions alone:

--8<-- "docs/plots/bench_carried_state.txt"

Pieces end on 4 KiB, so the totals step by 4096 bytes. Most of the cost per state is its index entries, in the
checkpoint and the final index, not the item.

## When they don't fit

Each definition and each carried state is a state record and takes an index entry, so the index policy bounds what a
piece carries. The
contract: a new piece carries every definition while the chain's tracks fit its room. That room is
`IndexPolicy::state_entries_max` less the layout, the producer and the records of the call the cut is made for. Past
that room, the tracks with the oldest data are left out. The next head flags their rows, the close counts them (`Closed::not_carried`) and a reader of
the piece names them (`DefinitionsAt::not_carried`).

A keyed state track is never left out. It goes first, with every live key. The writer refuses a put that would make
its keys too many for a piece ([Keyed state tracks](format.md#keyed-state-tracks)).

The room is counted in entries, not bytes, so a label's length changes it little. `just bench-carried` measures how
many tracks a cut carries whole, each with its own definition and no bundle:

--8<-- "docs/plots/bench_carried_room.txt"

### Refused calls and left-out tracks

The copies of the layout and the producer, which every call that puts a record may write, take no part in this room.
A copy that finds the state entries full is skipped, and the call goes on without it.

A call is admitted only when its records fit an empty piece under the same policy. Those are the definitions its tracks
need, its put, the layout, the producer and the keys every cut carries. Otherwise `InputError::CarriedEntries` refuses it: nothing written, no cut.

A cut is never refused for the definitions it carries. They outgrow the room in four ways:

- the chain counts in more tracks than the room holds;
- `defs` gains a clock map after the tracks were counted in;
- a definition grows, such as a longer label, so more blocks repeat the clocks;
- the index policy is lowered.

The cut keeps room for the records of the call it is made for. Those are its put and the definitions of the tracks
written last, or named with `TapeWriter::want`, put again before its data. The rest goes once each, with no repeated
clocks and maps, in this order while room lasts:

1. The keyed state tracks, each with its clock chain, its bundles and every live key.
2. The tracks of that call, each with its clock chain and bundles.
3. The clocks and maps the other tracks count through.
4. The tracks with data in the piece, newest first, each with its bundles; then the quiet tracks.

Carried once, a damaged block of clocks and maps costs every track using them.

A state track's carried item goes with its definition, so a track left out leaves out its state too. A track that
doesn't fit is left out. The next head marks its row as not carried, as does `Closed::tracks` of the cut
(`Closed::not_carried` counts them). A reader of the piece alone names each such track in `DefinitionsAt::not_carried`,
by the row's id and owner.

The invariant: an admitted call fits the piece a cut made for it. A full piece refuses data on a track left out
(`IndexError::StateEntries`, nothing written). `PieceChain` cuts for that call and writes it again: that piece holds data.
A cut by hand (`cut_now`) carries the last call's tracks first. Data on other tracks or changed definitions after it
may find no room and cut again, leaving a piece of definitions alone.

## What they cost

The cost per piece comes from `just bench-carried`. Records blocks of 4000 bytes go on one track, the others stay quiet,
each track is labeled `channel NNNNNNN`, and the piece is cut by bytes. A piece holds less data, and its final index
grows past the cut's bound, which counts no final index:

--8<-- "docs/plots/bench_carried_pieces.txt"

Each definition costs its record and its index entries, in the checkpoint that lists it and in the final index. At the
track caps, a piece of one records block grows by the bytes of the next table:

--8<-- "docs/plots/bench_carried_caps.txt"

A piece cut by bytes below what its carried definitions take holds them and one chunk: each cut comes due right after
the head.

A resume that keeps only the piece head (a crash right after the cut) leaves the definitions out. The next call given
`defs` puts them first, with the states the chain's last closed piece holds. The piece so gets the bytes of a cut
that never stopped. A piece closed before any such call
carries none: `close` takes no `defs`, so the piece before it keeps the only copy.
