---
level: basic
---

# Definitions

A [[reader]] that opens a recording in the middle needs facts that were first written long before. It needs to know
what each [[track]] is, which [[clock]] it counts and what its items mean. A [[tape]] states such facts as
[[definitions]], and keeps them close to every place that needs them.

The drive's fault at minute 52, for example, lies in the last of its four [[pieces|piece]]. A viewer that has only
that piece still needs the definitions of the drive's tracks. It finds them inside that piece, within 32 MiB of the
fault.

## What a definition says

A [[definition]] states one fact that a reader needs before the data means anything:

- the [[producer]], the program that writes the [[tape]];
- the [[layout]], how the [[writer]] places [[blocks]];
- an [[API bundle]], the WireWeaver description of the producer's API, which gives each [[item]]'s type;
- a [[clock]] or a [[clock map]];
- a [[track]]: its number, key, clock, kind and what its [[items|item]] are;
- a schema of a foreign item format, such as a `.proto` file;
- a typed item, such as a description of the run.

Each definition is a [[record]] on track 0, the track that holds only definitions. Records of track 0 carry no time,
and a [[block]] that holds them holds nothing else. The drive's definitions are its two clocks, the map between them
and its five tracks.

## Slots

Each [[definition]] fills a [[slot]] that follows from the definition itself:

- a [[track]]'s slot is its [[track number]];
- a clock's slot is its clock number;
- a map's slot is the two [[clocks]] it links;
- a [[bundle]]'s slot is its hash, and so is a schema's;
- the [[producer]] and the [[layout]] have one slot each.

A newer [[record]] in a slot replaces the older one for every [[reader]] after it. A reader at a place therefore takes
the newest record of each slot behind that place, and has every definition in force there. Adding a track an hour into
a recording costs one record, however many tracks the file already has.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/slots.svg"
<figcaption>Slots, one lane each, over the bytes of a piece. The writer writes each definition early and repeats it
later. A reader takes the newest record of each slot behind its place: the outlined ones. The mode is a state track
and fills a slot the same way, so this reader reads the mode as run.</figcaption>
</figure>

Definitions are [[state]]: each stays in force until a newer record of its slot replaces it. The writer keeps them
close behind every use by the rule that keeps any state close, which [State in force](state.md) explains.

## The definitions an application holds

The [[definitions]] belong to the application, not to the file. The application keeps them in a
[`struct@Definitions`] container and passes the container with every write. Defining writes nothing to the [[tape]]:
the [[writer]] places each definition just before the first data that needs it.

The drive defines its clocks, the map between them and its tracks this way:

```rust
--8<-- "examples/example_drive/src/defs.rs:define"
```

Each call returns the number it gives, the lowest not yet taken, so the application never picks a number. The same
clock key, or the same track key with the same dimensions, gets the same number back. A track or a map must name
clocks that the container holds.

The container has a call for each way an application changes what it holds:

| Call | What it does |
|---|---|
| [`Definitions::define_clock`], [`Definitions::define_track`] | holds a clock or a track and returns its number |
| [`Definitions::define_clock_with_maps`] | holds an [[epoch]] clock with the map that links it, which it must come with |
| [`Definitions::define_all`] | holds several maps, bundles or schemas at once |
| [`Definitions::close`] | drops a track, and the clocks, maps and bundles that no held track names |
| [`Definitions::with_numbers`] | starts a container with the numbers a tape already holds |

The container reports how many definitions it holds and their size in bytes. The drive holds 8 definitions in 469
bytes, and the application decides how many to keep.

### Numbers after a resume

A [[resume]] gives the numbers the [[tape]] already holds, in [`Resumed::numbers`]. A container started from them
keeps every clock and track at its number when the application defines it again. A new clock or track takes a number
not yet taken.

A [[chain]]'s resume reads the numbers from every [[piece]] still there. A track or clock defined only in a removed
piece loses its number. [`Resumed::numbers_unread`] counts the definitions the resume could not read, and a track or
clock among them may get a new number.

### What the writer defines itself

The [[producer]] and the [[layout]] belong to the writer, which takes them from [`WriterOptions`]. The writer puts
them in three places:

- after every [[piece head]];
- before the first block of a file;
- when a block starts another [[session]].

Every call that puts a definition or an item puts them too, so a piece whose first blocks are lost still names them.
The writer refuses a block of definitions that the caller wrote. A [[relay]] passes on another producer's [[blocks]]
through [`TapeWriter::relay_block`], which puts the definitions they carry by the writer's own rule.
