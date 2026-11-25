---
level: basic
---

# Tracks and items

A recording usually holds several streams at once, each of its own type and timing. A [[reader]] must keep them apart,
find the same stream again in next week's file, and know how each one is timed. The drive's recording, for example,
holds five streams: log lines, the controller's mode, frames from the vehicle bus, the phase current and the board's
temperature.

A [[tape]] keeps each stream as a [[track]], and three numbers identify a track: its number, its key and its id. A
track's timing decides its form. [[Events|event]] carry a time each, and a [[series]] samples at a fixed period. A
[[bytes track]] holds values that a reader takes whole.

## Items and tracks

An [[item]] is one value: a frame received on the bus, a temperature reading, a log line. A [[track]] is a sequence of
items of one type over time. The drive has five tracks, one per stream, and [What a tape holds](tape.md) lists them
with their rates.

The track's [[definition]] says what its items are, so a reader decodes an item years later without the device that
sent it. The drive's frames are [[WireWeaver values|WireWeaver value]], each of a type that the drive's API defines.
Its phase current is numbers in a column. [What a track's items are](items.md) lists every kind of item a track can
hold.

Every item has a time, a whole number of [[ticks]] of the track's [[clock]]. For the drive, every track counts the
board's microsecond timer since boot. An [[event]] stores its own tick, and a [[sample]] of a series takes its tick
from its index. [Clocks and time](clocks.md) explains clocks.

## Number, key and id

A [[track]] has three names, because three uses need different things. A [[block]] needs a short name it repeats for
every event it holds. A [[reader]] of two files needs a name that stays the same from file to file. One sensor recorded
twice side by side needs two names for one source.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/track_identity.svg"
<figcaption>A track's three names. Its API path hashes to its key, which every file of the recording shares. The
key and the dimensions hash to its id, which is the key itself without dimensions. Inside one file, a small number
stands for the track. The values are the drive's <code>can0</code>.</figcaption>
</figure>

The [[track key]] is a 64-bit hash of where the track comes from in the [[producer]]'s API. The drive's `can0` comes
from the resource `can0` of the drive's API, so its key is the hash of that path. Two files of the drive give `can0`
the same key, whichever program wrote them. [`track_key`] computes it as a [`TrackKey`].

A [[dimension]] is a name and value pair, such as `bench = 2`, that tells apart tracks of one key. Two benches that
record the same `can0` resource tell their tracks apart by `bench = 1` and `bench = 2`. The [[track id]] is a hash of
the key and the dimensions, and a track without dimensions has its key as its id. [`describe::track_id`] computes it
as a [`struct@TrackId`]. A [[chunk]] carries the id, so it names its track even to a reader without the definitions.

The [[track number]] is the small number a track has inside one file. Every [[record]] carries it, because it costs
one byte where the id costs eight. The producer never picks it. [`Definitions::define_track`] gives the track the
lowest number not yet taken and returns it. The drive defines `can0` third, so `can0` is track 3 in the drive's files.

The track's [[definition]] ties the number to the key and the dimensions, so a reader turns any number into an id.
This test computes all three names of `can0`:

```rust
--8<-- "ww_tape_tests/tests/it/book_pages.rs:track_ids"
```

The three names compare like this:

| Name | What it is | The same in | Carried by |
|---|---|---|---|
| track number | a small integer | one file | every record and chunk head |
| track key | a 64-bit hash of the API path | every file of the recording | the track's definition |
| track id | a 64-bit hash of the key and the dimensions | every file of the recording | chunk heads and index entries |

Track number 0 is special: it holds the definitions themselves. [Definitions](definitions.md) explains it.

## Item numbers

Each item of a [[track]] has an [[item number]], which counts the track's [[items]] from 0 over the whole recording.
The count runs on across files, so item 3 120 000 of `can0` is the same frame in every file that holds the drive's
recording. That frame is the emergency frame the board sends when it faults at minute 52.

```rust
--8<-- "ww_tape_tests/tests/it/book_pages.rs:emergency_frame"
```

A [[reader]] addresses an item by its [[track id]] and item number, never by where it lies in a file. A file that
keeps only some tracks of the recording, or that joins several files into one, moves the bytes. The track id and the
item number stay the same.

## Events, series or values { #events-or-series }

A [[track]] keeps its [[items]] in one of three ways, and the [[producer]] picks the way per track. The way follows
from how the items are timed and how a reader takes them.

| | Events | Series | Bytes track |
|---|---|---|---|
| Time | each item has its own start, and maybe an end | the sample's index on a fixed period | one tick per value |
| Items | any kind: a WireWeaver value, plain bytes, numbers, bytes of a named format | numbers of one element, one or more channels each | values of any length, read whole |
| A reader takes | any item or range of items | any range of samples | a whole value, its hash checked |
| Examples | the drive's `log`, `mode`, `can0`, `temp` | the drive's `phase_a` | a firmware image |

### Events

An [[event]] is an item with a time of its own. The drive's frames are events: they arrive when the bus carries them,
about 1000 a second but never exactly on a beat. Each event keeps its own start [[tick]]. An event may also have an
end, which makes it a span, such as a run of the controller from start to stop.

Choose events for messages, [[state]] changes, log lines and samples at an irregular rate. An event pays for its time,
but it never needs a fixed rate. [Events](events.md) shows how a reader reads them.

### Series

A [[series]] is a track of [[samples]] at an exact period of its clock. The drive's phase current is a sample every
40 µs of the board's timer. A sample needs no time of its own, because its index gives it: sample `i` lies `i` periods
after the first. A series costs no bytes for time, and its values pack tightly: the drive's `phase_a` takes about 1.1
bytes a sample.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/events_series.svg"
<figcaption>Events against samples. An events chunk holds a start tick per item, beside optional ends, flags and
payloads. A series chunk holds only its samples, and the grid in its chunk head times them.</figcaption>
</figure>

Choose a series when the device samples at a fixed rate and loses no beat that matters. The period is exact in the
device's ticks, and 25 kHz is its nominal rate. The board's crystal runs 20 ppm fast, so its 40 µs are slightly
shorter on the host's clock.

The tape keeps the samples on the board's clock, where they are exact. A [[clock map]] then places them on the host's
time line, as [Clock maps](clocks.md#clock-maps) explains. Over the hour, the board's count runs 72 ms ahead of the
host's:

```rust
--8<-- "ww_tape_tests/tests/it/book_pages.rs:drift_hour"
```

[Series](series.md) shows how a reader reads samples.

### Values read whole { #values-read-whole }

Some data is made elsewhere and only means something whole: a document, a design file, a firmware image. A reader
never needs byte 1000 of a firmware image on its own. It needs the image that a board ran, all of it, and proof that
no byte changed.

A [[bytes track]] holds such data as values. A tuning session of a motor controller, for example, records the firmware
image it flashes beside the parameters it sets. The crate `example_settings` records such a session, and defines the
firmware's track as a bytes track:

```rust
--8<-- "examples/example_settings/src/lib.rs:define"
```

The writer streams a value as [[parts]], each an events chunk of one item. A last part, the [[seal]], holds the
value's length and its BLAKE3 hash. [`BytesWriter`] cuts the value into parts, and the chain writes each part as it
comes:

```rust
--8<-- "examples/example_settings/src/lib.rs:firmware"
```

A reader asks for a value by its id, the item number of its first part. [`bytes_track::read_value`] finds the parts in
each piece's index, reads them in order and checks the hash against the seal:

```rust
--8<-- "examples/example_settings/src/lib.rs:read_value"
```

The image of 200 000 bytes takes three parts of 64 KiB, a short last part and the seal. `cargo run -p example_settings`
prints both sides:

```text
--8<-- "docs/generated/settings_firmware.txt"
```

A value is whole or absent, never partial. A crash in the middle of a value leaves parts without a seal, and the reader
reports the value as torn. A value may span a [[cut]], so a large image goes on in the next [[piece]]. [Bytes
tracks](format.md#bytes-tracks) gives the parts' layout.
