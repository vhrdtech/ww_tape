---
level: basic
---

# Tracks and items

A recording usually holds several streams at once, each of its own type and timing. A [[reader]] must keep them apart,
find the same stream again in next week's file, and know how each one is timed. The drive's recording, for example,
holds five streams: log lines, the controller's mode, frames from the vehicle bus, the phase current and the board's
temperature.

A [[tape]] keeps each stream as a [[track]], and three numbers identify a track: its number, its key and its id. A
track's timing decides its form: a stream of [[events]] gives each item a time, and a [[series]] samples at a fixed
period.

## Items and tracks

An [[item]] is one value: a frame received on the bus, a temperature reading, a log line. The item of an events track is
most often a [[WireWeaver value]]. Such a value has a type defined in a WireWeaver API, such as a struct, an enum or a
number. The tape carries that type's definition and keeps the value in its shrink_wrap encoding.

A sample of a series is a number in a column instead, coded by ww_tape's series codecs. A [[reader]] that has the
track's [[definition]] therefore decodes the item years later, without the device that sent it. [WireWeaver and
logring](related_books.md) introduces both.

A track may instead hold plain bytes, numbers of an element the definition describes, or bytes of another format.
[What a track's items are](#what-a-tracks-items-are) lists the kinds.

A [[track]] is a sequence of items of one type over time. The drive has five tracks, one per stream, and [What a tape
holds](tape.md) lists them with their rates.

Every item has a time, a whole number of [[ticks]] of the track's [[clock]], which [Clocks and time](clocks.md)
explains. For the drive, every track counts the board's microsecond timer since boot. How an item keeps its time
depends on the track's kind, which [Events or series](#events-or-series) explains. An [[event]] stores its own tick.
A [[sample]] of a series takes its tick from its index.

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
from the resource `can0` of the drive's API, so its key is the hash of that path. Two files of the drive give `can0` the
same key, whichever program wrote them. [`track_key`] computes it as a [`TrackKey`].

A [[dimension]] is a name and value pair, such as `bench = 2`, that tells apart tracks of one key. Two benches that
record the same `can0` resource tell their tracks apart by `bench = 1` and `bench = 2`. The [[track id]] is a hash of the key and the
dimensions, and a track without dimensions has its key as its id. [`describe::track_id`] computes it as a
[`struct@TrackId`]. A [[chunk]] carries the id, so it names its track even to a reader without the definitions.

The [[track number]] is the small number a track has inside one file. Every [[record]], the form of one event in a
[[records block]], carries it, because it costs one byte where the id costs eight. The producer never picks it: when it
defines a track, [`Definitions::define_track`] gives the track the lowest number not yet taken and returns it. The drive
defines `can0` third, so `can0` is track 3 in the drive's files. Defining the same key and dimensions again gives the
same number back.

The track's [[definition]] ties the number to the key and the dimensions, so a reader turns any
number into an id.

```rust
use ww_tape::{Dimension, track_id};
use example_drive::{definitions, key};

// The key of can0 is the hash of its path in the drive's API.
let can0 = key("can0");
assert_eq!(can0.get(), 0x0158_4c57_693f_d3f2);
// Without dimensions, the id is the key.
assert_eq!(track_id(can0, []), can0.id());
// A dimension gives the same resource another id.
let bench2 = track_id(can0, [Dimension { name: "bench", value: "2" }]);
assert_eq!(bench2.get(), 0x4d71_7cfb_2248_cb86);
// Inside the drive's files, can0 is track 3: the number its definition was given.
let (_, numbers) = definitions(25_000, true, 32 << 20).expect("the drive's definitions");
assert_eq!(numbers.can0.get(), 3);
```

The three names compare like this:

| Name | What it is | The same in | Carried by |
|---|---|---|---|
| track number | a small integer | one file | every record and chunk head |
| track key | a 64-bit hash of the API path | every file of the recording | the track's definition |
| track id | a 64-bit hash of the key and the dimensions | every file of the recording | chunk heads and index entries |

Track number 0 is special: it holds the definitions themselves. [Definitions and state](definitions.md) explains it.

## Item numbers

Each item of a [[track]] has an [[item number]], which counts the track's [[items]] from 0 over the whole recording. The
count runs on across files, so item 3 120 000 of `can0` is the same frame in every file that holds the drive's
recording. That frame is the emergency frame the board sends when it faults at minute 52.

```rust
// can0 runs at 1 kHz: a frame is due every 1000 µs.
assert_eq!(example_drive::emergency_frame(1000), 3_120_000);
```

A [[reader]] addresses an item by its [[track id]] and item number, never by where it lies in a file. A file that keeps
only some tracks of the recording, or that joins several files into one, moves the bytes. The track id and the item
number stay the same.

## Events or series

A [[track]] keeps its [[items]] in one of two ways, and the [[producer]] picks the way per track.

An [[event]] is an item with a time of its own. The drive's frames are events: they arrive when the bus carries them,
about 1000 a second but never exactly on a beat. Each event keeps its own start [[tick]]. An event may also have an end,
which makes it a span, such as a run of the controller from start to stop.

A [[series]] is a track of [[samples]] at an exact period of its clock. The drive's phase current is a sample every
40 µs of the board's timer. A sample needs no time of its own, because its index gives it: sample `i` lies `i` periods after the
first. The drive's `phase_a` keeps 16-bit integers in steps of 1 mA, and each [[chunk]] of samples states that step
once.

The period is exact in the device's ticks, and 25 kHz is its nominal rate. The board's crystal runs 20 ppm fast, so its
40 µs are slightly shorter on the host's clock. The tape keeps the samples on the board's clock, where they are exact.
A [[clock map]] then places them on the host's time line, and [Clock maps](clocks.md#clock-maps) explains it. Over the
hour, the board's count runs 72 ms ahead of the host's:

```rust
use shrink_wrap::prelude::*;
use ww_tape::{Describe, Rounding, Slot};
use example_drive::{BOOT_UNIX_NS, definitions};

let (defs, numbers) = definitions(25_000, true, 32 << 20).expect("the drive's definitions");
let map = Slot::Map { from: numbers.board_clock, to: numbers.host_clock };
let bytes = defs.get(&map).expect("the drive's map");
let Describe::Map(map) = Describe::from_ww_bytes(bytes).expect("a definition") else {
    panic!("the slot of a map holds a map");
};
let segment = map.segments.iter().next().expect("one segment").expect("it reads");
// One hour on the board's timer, 90 000 000 samples at 40 µs.
let hour_us = 3_600 * 1_000_000;
assert_eq!(hour_us / 40, 90_000_000);
let host_ns = segment.map(hour_us, Rounding::Floor).expect("mapped") - BOOT_UNIX_NS;
// On the host's clock the hour of samples lasts 72 ms less.
assert_eq!(hour_us * 1_000 - host_ns, 72_000_000);
```

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/events_series.svg"
<figcaption>Events against samples. An events chunk holds a start tick per item, beside optional ends, flags and
payloads. A series chunk holds only its samples, and the grid in its chunk head times them.</figcaption>
</figure>

Choose a series when the device samples at a fixed rate and loses no beat that matters. Choose events for everything
else: messages, [[state]] changes, log lines and samples at an irregular rate. A series costs no bytes for time, and its
values pack tightly: the drive's `phase_a` takes about 1.1 bytes a sample. An event pays for its time, but it never
needs a fixed rate.

| | Events | Series |
|---|---|---|
| Time | each item has its own start, and maybe an end | the sample's index on a fixed period |
| Items | events of any kind: a WireWeaver value, plain bytes, numbers, bytes of a named format | samples: numbers of one element, one or more channels each |
| The drive's tracks | `log`, `mode`, `can0`, `temp` | `phase_a` |

[Blocks](blocks.md) shows the blocks that hold each kind, and [Events](events.md) and [Series](series.md) how a reader
reads them.

## What a track's items are

A [[reader]] must know what an [[item]]'s bytes mean before it can show a value. The track's [[definition]] says it in
[`TrackDef::items`], one of the kinds below. A tape keeps an events item's bytes as given and never parses them, so a new
kind of item needs no new block or codec.

| `Items` | One item is | Example |
|---|---|---|
| `WireWeaver` | a value of a stream of a WireWeaver API, in its shrink_wrap encoding | the drive's `mode` |
| `ShrinkWrap` | a value of a shrink_wrap type from a bundle, outside any API stream | a test bench's result per run |
| `RawBytes` | plain bytes with no type | frames as a sniffer received them |
| `Numbers` | one number of an element the definition describes, with an optional unit | the drive's `phase_a` |
| `Foreign` | bytes of a format with a name, a media type and a version | a pcap capture, `application/vnd.tcpdump.pcap` |

A foreign format may also carry its schema, such as a `.proto` file, inside the tape. The schema is a definition of
its own, [`Describe::Schema`], named by the BLAKE3 hash of its bytes. A reader years later then decodes the items
without the program that wrote them.

A newer ww_tape may add a kind of items. An older reader then reads that track's definition with its items marked
unknown, and every other field intact. A relay still stores the track's events as given.

### Numbers of any element

The items of a [[series]] are always `Numbers`. An [`ElementDesc`] describes one number by its stored bits, its sign
and its byte order. A float adds its exponent bits, and a pair such as I and Q has two parts. The description also
names the stored number that stands for zero. A track thus holds numbers as a device sends them, and no predefined
element has to match.

A low-cost software-defined radio is such a device. It sends 8-bit IQ samples: each sample is two unsigned bytes, I
then Q, and the value 127.5 stands for zero. The track describes exactly that:

```rs
--8<-- "ww_tape_tests/tests/it/iq8.rs:def"
```

The writer stores the radio's bytes as sent, two bytes a sample, and a reader decodes them back to the same numbers.
[`iq8.rs`] writes such a track and reads it back. Summaries take the zero into account: they keep `2·v - 255` for
each part, so the power of a node is exact. Floats of another layout, such as 16-bit or 8-bit floats, keep their own
bits the same way. [Items of a track](format.md#items-of-a-track) gives each field's range.
