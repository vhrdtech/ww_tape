---
level: basic
---

# Definitions and state

A [[reader]] that opens a recording in the middle needs facts that were first written long before. It needs to know
what each [[track]] is, and which values were in force at that point. A [[tape]] therefore keeps such facts close to
every place that needs them, so that a reader anywhere finds them within 32 MiB.

At minute 52, for example, the drive faults, and an engineer opens the [[piece]] that holds the fault. That piece is
the last of four, and the engineer has only it. To read it, the viewer needs the [[definitions]] of the drive's tracks
and the mode the controller was in, a [[state item]].

## What a definition says

A [[reader]] needs some facts before the data means anything. A [[definition]] states one of them:

- the [[producer]], the program that writes the [[tape]];
- the [[layout]], how the [[writer]] places [[blocks]];
- an [[API bundle]], the WireWeaver description of the producer's API, which gives each [[item]]'s type;
- a [[clock]] or a [[clock map]];
- a [[track]]: its number, key, clock, kind and where its items come from in the API;
- a typed item, such as a record of the run.

Each definition is a [[record]] on track 0, the track that holds only definitions. The drive's definitions are its two
clocks, the map between them and its five tracks.

## Slots

Each [[definition]] fills a [[slot]] that follows from the definition itself:

- a [[track]]'s slot is its [[track number]];
- a clock's slot is its clock number;
- a map's slot is the two [[clocks]] it links;
- a [[bundle]]'s slot is its hash;
- the [[producer]] and the [[layout]] have one slot each.

A newer [[record]] in a slot replaces the older one for every [[reader]] after it. A reader at a place therefore takes
the newest record of each slot behind that place, and has every definition in force there. Adding a track an hour into a
recording costs one record, however many tracks the file already has.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/slots.svg"
<figcaption>Slots, one lane each, over the bytes of a piece. The writer writes each definition early and repeats it
later. A reader takes the newest record of each slot behind its place: the outlined ones. The mode is a state track
and fills a slot the same way, so this reader reads the mode as run.</figcaption>
</figure>

## The definitions an application holds

The [[definitions]] belong to the application, not to the file. The application keeps the definitions of what it works
on in a [`Definitions`] container and passes the container with every write. Defining writes nothing to the [[tape]]:
the [[writer]] places each definition just before the first data that needs it.

[`Definitions::define_all`] holds several definitions at once and checks them against those already held. A [[track]] or
a map must name [[clocks]] that the container holds, and an [[epoch]] clock must come with its link.
[`Definitions::close`] drops a track, and the clocks, maps and [[bundles]] that no held track names go with it. The
container reports how many definitions it holds and their size in bytes, and the application decides how many to keep.

The drive holds 8 definitions in 467 bytes. The test below takes them from the example, [[puts]] the mode three times
and closes a track:

```rust
use ww_tape::{MemSink, SyncPolicy, TapeWriter};
use ww_tape_example::{CAN0, MODE, Mode, definitions};

let mut defs = definitions(25_000, true, 32 << 20).expect("the drive's definitions");
assert_eq!((defs.len(), defs.bytes()), (8, 467));

let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
// The first put writes the mode, with the definitions it needs.
assert!(w.put(&defs, MODE, 0, Mode::Run.item()).expect("put"));
// The same mode 1 ms later lies close behind: the put writes nothing.
assert!(!w.put(&defs, MODE, 1_000, Mode::Run.item()).expect("put"));
// A new mode is written.
assert!(w.put(&defs, MODE, 2_000, Mode::Fault.item()).expect("put"));

// Closing can0 drops its definition; the clocks stay, since other tracks name them.
assert!(defs.close(CAN0));
assert_eq!(defs.len(), 7);
```

The [[producer]] and the [[layout]] belong to the writer, which takes them from [`WriterOptions`]. The writer puts them
after every [[piece head]], before the first block of a file, and when a block starts another [[session]]. It refuses a
block of definitions that the caller wrote. A [[relay]] that passes on another producer's [[blocks]] uses
[`TapeWriter::relay_block`], which holds the definitions those blocks carry and puts them by the writer's own rule.

## State

Most [[tracks]] are streams of [[events]]: a frame received at one [[tick]] says nothing about the next tick. Some
tracks hold [[state]] instead: an [[item]] that stays in force until the next one replaces it. The drive's `mode` is
such a [[state track]]: idle from boot, run from minute 1, fault from minute 52. Definitions are state too, on track 0.

A [[reader]] at a place needs the item in force there to understand the data after it. A reader that reads a file from
its start always knows the state, because it has seen every [[record]]. A reader that starts in the middle looks
backwards for the newest record of the [[slot]]. That works only when the record lies in bytes the reader has.

### Why a reader needs state close behind

A [[record]] written once at the start of a week-long file lies a week of bytes away. Three [[readers]] then fail, even
with unlimited time:

1. A reader of a file whose start is damaged: the only record may lie in the damaged bytes.
2. A reader of one [[piece]] of a long recording: the record lies in a piece it does not have.
3. A reader that joins a [[live feed]] late: the record went out before it joined.

The format therefore keeps every [[state item]] close behind each place that uses it, in the same piece. The first
reader finds a copy past the damage, and the second finds one inside its piece. The third has every [[item]] once it has
received that much of the feed.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/state_reader.svg"
<figcaption>Three readers that start in the middle. Past a damaged start and inside one piece, each finds a record
of the state within near behind it. A reader that joins a live feed late has every item once it has received near
bytes.</figcaption>
</figure>

## Near, puts and copies

A [[state track]]'s policy, a [`StatePolicy`] in [`TrackRepeat::State`], holds two numbers:

- [[Near]] is how far behind a use the [[item]] must lie in the same [[piece]], 32 MiB by default.
- `copies` is how many extra [[records]] an item gets after it is first written in a piece, as a guard against damage
  around one place. The default is one [[state copy]], at least 1 MiB after the first record.

Each time the [[producer]] uses a [[state item]], it [[puts]] the item: it calls [`TapeWriter::put`] at that place. The
[[writer]] compares the item, by its hash, with the newest record of its [[slot]]. When the same item lies less than
`near` behind, the put writes nothing. Otherwise the put writes the item again, and a [[reader]] at this place finds it
close behind.

A put is idempotent, so the producer needs no record of what it put before. The drive's recorder puts the mode before
each one-second [[block]], 901 times while it writes the first piece. Only four of those puts write, as `cargo run -p
ww_tape_example` prints. They are idle at the start, its copy at 40 seconds, run at minute 1 and its copy at 96 seconds.
The piece ends at 28 MiB, before the run record lies 32 MiB behind, so no other put writes.

The writer puts [[definitions]] the same way before every block of data, by the [[layout]]'s own policy. The drawing
shows the rule shrunk, so that a whole cycle fits on one line.

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/puts.svg"
<figcaption>A state track put before each of 26 blocks of data, of which only four puts write. They are the first, one
copy 2 KiB on, and the item again each time the newest record lies more than <code>near</code> behind. Drawn by
<code>ww_tape_figures</code> from the blocks <code>TapeWriter</code> wrote, with near shrunk to 8 KiB and the copy
spacing to 2 KiB.</figcaption>
</figure>

A record that a put or a copy writes goes into a block with the flag [`FLAG_REPEAT`] in its header. A reader that
already holds the same item in that slot takes the record as nothing new. The writer never writes a state item ahead of
a use, so a [[track]] whose data has stopped costs nothing until its data continues.

A [[series]] holds no state. Each [[series chunk]] carries its own [[grid]], which gives the [[tick]] of one sample and
the [[scale]] of the stored numbers. The [[chunk]] places its [[samples]] alone, so nothing needs to stay near it.
[Anatomy of a piece](anatomy.md) shows where the definitions, the puts and the data sit in a whole piece.
