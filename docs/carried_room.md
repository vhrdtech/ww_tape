---
level: nomicon
---

# When carried definitions don't fit

A new piece carries the definitions of the chain's tracks ([The definitions a new piece
carries](carried_definitions.md)). Each one takes an index entry, and the index policy bounds the entries of a piece.
This page says how much room the definitions get, what a cut leaves out past it and which calls the writer refuses.

## The room

Each definition and each carried state is a state record and takes an index entry. The contract: a new piece carries
every definition while the chain's tracks fit its room. That room is `IndexPolicy::state_entries_max` less the layout,
the producer and the records of the call the cut is made for.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/carried_room.svg"
<figcaption>A piece's state entries. The layout, the producer and the call's records come first, outside the room. The
room fills in a fixed order, and the tracks with the oldest data are left out past it.</figcaption>
</figure>

Past that room, the tracks with the oldest data are left out. Three places name them:

- the next head flags their rows;
- the close counts them (`Closed::not_carried`);
- a reader of the piece names them (`DefinitionsAt::not_carried`).

A keyed state track is never left out. It goes first, with every live key. The writer refuses a put that would make
its keys too many for a piece ([Keyed state tracks](format.md#keyed-state-tracks)).

The room is counted in entries, not bytes, so a label's length changes it little. `just bench-carried` measures how
many tracks a cut carries whole, each with its own definition and no bundle:

--8<-- "docs/plots/bench_carried_room.txt"

## Refused calls and left-out tracks

[[State copies]] take no part in this room, nor do the copies of the layout and the producer. A copy that finds the
state entries full is skipped, and the call goes on without it.

A call is admitted only when its records fit an empty piece under the same policy. Those are the definitions its tracks
need, its put, the layout, the producer and the keys every cut carries. Otherwise `InputError::CarriedEntries` refuses
it: nothing written, no cut.

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

A state track's carried item goes with its definition, so a track left out leaves out its state too. Besides the next
head, `Closed::tracks` of the cut marks the track's row as not carried. `DefinitionsAt::not_carried` names it by the
row's id and owner.

## Data on a track left out

The invariant: an admitted call fits the piece a cut made for it. What happens to data on a track left out depends on
who cut:

| Cut by | Data on a track left out |
|---|---|
| `PieceChain`, for a call | a full piece refuses it (`IndexError::StateEntries`, nothing written); the chain cuts for that call and writes it again, so that piece holds data |
| hand (`cut_now`) | the cut carries the last call's tracks first; data on other tracks or changed definitions after it may find no room and cut again, leaving a piece of definitions alone |

