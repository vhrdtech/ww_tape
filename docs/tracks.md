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

A track may instead hold plain bytes, such as frames as a sniffer received them. It may hold numbers of an element the
definition describes, or bytes in a format that a media type names, such as `application/vnd.tcpdump.pcap`. The track's
definition says which, in `TrackDef::items`, and the bytes are kept as given, with nothing added.

A [[track]] is a sequence of items of one type over time. The drive has five tracks, one per stream, and [What a tape
holds](tape.md) lists them with their rates.

A [[record]] is one item on one track at one time. The time is a whole number of [[ticks]] of the track's [[clock]],
which [Clocks and time](clocks.md) explains. For the drive, every track counts the board's microsecond timer since boot.

## Number, key and id

A [[track]] has three names, because three uses need different things. A [[block]] needs a short name it repeats in
every [[record]]. A [[reader]] of two files needs a name that stays the same from file to file. One sensor recorded
twice side by side needs two names for one source.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/track_identity.svg"
<figcaption>A track's three names. Its API path hashes to its key, which every file of the recording shares. The
key and the dimensions hash to its id, which is the key itself without dimensions. Inside one file, a small number
stands for the track. The values are the drive's <code>can0</code>.</figcaption>
</figure>

The [[track key]] is a 64-bit hash of where the track comes from in the [[producer]]'s API. The drive's `can0` comes
from the resource `can0` of the drive's API, so its key is the hash of that path. Two files of the drive give `can0` the
same key, whichever program wrote them. [`track_key`] computes it.

A [[dimension]] is a name and value pair, such as `bench = 2`, that tells apart tracks of one key. Two benches that
record the same `can0` resource tell their tracks apart by `bench = 1` and `bench = 2`. The [[track id]] is a hash of the key and the
dimensions, and a track without dimensions has its key as its id. [`describe::track_id`] computes it. A [[chunk]]
carries the id, so it names its track even to a reader without the definitions.

The [[track number]] is the small number a track has inside one file: `can0` is track 3 in the drive's files. Records
carry it, because it costs one byte where the id costs eight. The track's [[definition]] ties the number to the key and
the dimensions, so a reader turns any number into an id.

```rust
use ww_tape::{Dimension, track_id};
use ww_tape_example::{CAN0, key};

// The key of can0 is the hash of its path in the drive's API.
let can0 = key("can0");
assert_eq!(can0, 0x0158_4c57_693f_d3f2);
// Without dimensions, the id is the key.
assert_eq!(track_id(can0, []), can0);
// A dimension gives the same resource another id.
let bench2 = track_id(can0, [Dimension { name: "bench", value: "2" }]);
assert_eq!(bench2, 0x4d71_7cfb_2248_cb86);
// Inside the drive's files, can0 is track 3.
assert_eq!(CAN0, 3);
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
assert_eq!(ww_tape_example::emergency_frame(1000), 3_120_000);
```

A [[reader]] addresses an item by its [[track id]] and item number, never by where it lies in a file. A file that keeps
only some tracks of the recording, or that joins several files into one, moves the bytes. The track id and the item
number stay the same.

## Events or series

A [[track]] keeps its [[items]] in one of two ways, and the [[producer]] picks the way per track.

An [[event]] is an item with a time of its own. The drive's frames are events: they arrive when the bus carries them,
about 1000 a second but never exactly on a beat. Each event keeps its own start [[tick]]. An event may also have an end,
which makes it a span, such as a run of the controller from start to stop.

A [[series]] is a track of [[samples]] at an exact period, such as the phase current at 25 kHz. A sample needs no time
of its own, because its index gives it: sample `i` lies `i` periods after the first. The drive's `phase_a` keeps 16-bit
integers in steps of 1 mA, and each [[chunk]] of samples states that step once.

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
| Items | a WireWeaver value, plain bytes, or bytes of a named format | numbers, one or more channels a sample |
| The drive's tracks | `log`, `mode`, `can0`, `temp` | `phase_a` |

[Blocks](blocks.md) shows the blocks that hold each kind, and [Events](events.md) and [Series](series.md) how a reader
reads them.
