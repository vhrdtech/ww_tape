---
level: basic
---

# State in force

Most [[tracks]] are streams of [[events]]: a frame received at one [[tick]] says nothing about the next tick. Some
tracks hold [[state]] instead: an [[item]] that stays in force until the next one replaces it. A [[reader]] that
starts in the middle of a recording needs the state in force there, written perhaps an hour before.

The drive's `mode` is such a [[state track]]: the mode is idle from boot, run from minute 1 and fault from minute 52
of the run. A viewer that opens the recording at minute 40 must show run, although the mode last changed 39 minutes
before. The [[definitions]] are state too, on track 0, and follow the same rules.

## Why a reader needs state close behind

A reader that reads a file from its start always knows the state, because it has seen every [[record]]. A reader that
starts in the middle looks backwards for the newest record of the [[slot]]. That works only when the record lies in
bytes the reader has.

A record written once at the start of a week-long file lies a week of bytes away. Three [[readers]] then fail, even
with unlimited time:

1. A reader of a file whose start is damaged: the only record may lie in the damaged bytes.
2. A reader of one [[piece]] of a long recording: the record lies in a piece it does not have.
3. A reader that joins a [[live feed]] late: the record went out before it joined.

The format therefore keeps every [[state item]] close behind each place that uses it, in the same piece. The first
reader finds a copy past the damage, and the second finds one inside its piece. The third has every [[item]] once it
has received that much of the feed.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/state_reader.svg"
<figcaption>Three readers that start in the middle. Past a damaged start and inside one piece, each finds a record
of the state within near behind it. A reader that joins a live feed late has every item once it has received near
bytes.</figcaption>
</figure>

## Near, puts and copies

A [[state track]]'s policy, a [`StatePolicy`] in [`TrackRepeat::State`], holds two numbers:

- [[Near]] is how far behind a use the [[item]] must lie in the same [[piece]], 32 MiB by default.
- `copies` is how many extra [[records]] an item gets after it is first written in a piece, as a guard against
  damage around one place. The default is two [[state copies]]: the first 4 KiB after the first record, the second
  at least 1 MiB after the first copy.

Each time the [[producer]] uses a [[state item]], it [[puts]] the item: it calls [`TapeWriter::put`] at that place.
The [[writer]] compares the item, by its hash, with the newest record of its [[slot]]. When the same item lies less
than near behind, the put writes nothing. Otherwise the put writes the item again, so a [[reader]] at this place finds
it close behind.

The first copy needs no use. Once an item's only record lies 4 KiB behind, the next call that writes a block of data
first writes that copy ([`NearPolicy::copy_first`]). A track whose data has stopped thus still gets a copy of its item
past the first sector of the piece. Each later copy comes at a use, [`NearPolicy::copy_spacing`] after the newest
record.

A put is idempotent, so the producer keeps no memory of what it put before. This test puts the drive's mode three
times, and the second put writes nothing:

```rust
--8<-- "ww_tape_tests/tests/it/book_pages.rs:puts"
```

The drive's recorder puts the mode before each one-second [[block]], 900 times while it writes the first piece. Only
six of those puts write, as `cargo run -p example_drive` prints:

| Put at | Writes |
|---|---|
| the start | idle |
| 1 and 41 seconds | idle's copies |
| minute 1 | run |
| 61 and 97 seconds | run's copies |

Each first copy comes with the first put once the record lies 4 KiB behind. The piece ends at about 28 MiB, before the
record of run lies 32 MiB behind, so no other put writes.

The writer puts [[definitions]] the same way before every block of data, by the [[layout]]'s own policy. The drawing
shows the rule shrunk, so that a whole cycle fits on one line.

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/puts.svg"
<figcaption>A state track put before each of 26 blocks of data, of which only four puts write. They are the first, one
copy 2 KiB on, and the item again each time the newest record lies more than <code>near</code> behind. Drawn by
<code>ww_tape_figures</code> from the blocks <code>TapeWriter</code> wrote, with near shrunk to 8 KiB and the first
copy to 2 KiB.</figcaption>
</figure>

A record that a put or a copy writes goes into a block whose header bit 1 is set, [`FLAG_REPEAT`]. The bit belongs to
the block contract, so every reader knows a block of state from a block of data. A reader that already holds the same
item in that slot takes the record as nothing new.

The writer never writes a state item ahead of a use, apart from its first copy. A [[track]] whose data has stopped
therefore costs one copy of each item, then nothing until its data continues.

## Keyed state tracks

Some state comes as many values side by side, each with a name. A motor controller has dozens of parameters: a
current limit, the gains of its control loop, a trace switch. Each one stays in force until it changes, and a reader
at any place needs all of them.

A [[keyed state track]] holds such state. Its items have a [[key]], such as `pid/kp`, and each key is a slot of its
own. The newest record of each key is in force, and the writer keeps each key near its use, as it keeps any slot. The
crate `example_settings` records a tuning session of a controller, whose parameters lie on such a track. It defines
the track with [`TrackRepeat::KEYED`]:

```rust
--8<-- "examples/example_settings/src/lib.rs:define"
```

The session puts each parameter by its key with [`TapeWriter::put_key`]. [`TapeWriter::delete_key`] ends a key, and
the deletion is a record too, so older values stay in the file:

```rust
--8<-- "examples/example_settings/src/lib.rs:keys"
```

Each put prints whether it wrote. The same value of `pid/kp` a second later lies close behind, so that put writes
nothing:

```console
$ cargo run -p example_settings
```

```text
--8<-- "docs/generated/settings_puts.txt"
```

A reader asks for one key at a place with [`TapeReader::key_at`], or for every live key with
[`TapeReader::keys_at`]. [`TapeReader::key_history`] lists a key's records, newest first. A [[cut]] carries every
live key into the new piece, so the last piece read alone still lists all of them:

```rust
--8<-- "examples/example_settings/src/lib.rs:read_keys"
```

```console
$ cargo run -p example_settings
```

```text
--8<-- "docs/generated/settings_keys.txt"
```

The keyed records go into blocks of their own kind, keyed puts. The index lists each record by its key, so a lookup
reads only that key's records. [Keyed state tracks](format.md#keyed-state-tracks) gives the layout of a keyed item.

## Groups of puts

Some changes only make sense together. The tuning session switches the controller to its fast profile, and the fast
profile comes with higher gains. A reader that saw the new profile with the old gains would show a setting the
controller never ran.

A [[group of puts]] puts several items as one: a reader takes all of them or none. The items may lie on several state
tracks, and on several keys of a keyed track. [`TapeWriter::put_group`] takes the group:

```rust
--8<-- "examples/example_settings/src/lib.rs:group"
```

Each put of a group decides by the near rule, so a group writes only the items not already in force close behind. The
writer checks the whole group before it writes the first block, and refuses it whole when one put does not fit.

The writer marks each block of the group, and its first block apart. A crash inside the group leaves marked blocks
without the last one, and a reader drops them all. [Blocks](blocks.md#kinds-of-block) lists the blocks of the tuning
session with their marks, and [Groups of puts](format.md#groups-of-puts) gives the rules.

## Series need no state

A [[series]] holds no state. Each [[series chunk]] carries its own [[grid]], which gives the [[tick]] of one sample
and the [[scale]] of the stored numbers. The [[chunk]] places its [[samples]] alone, so nothing needs to stay near it.
[Anatomy of a piece](anatomy.md) shows where the definitions, the puts and the data sit in a whole piece.
