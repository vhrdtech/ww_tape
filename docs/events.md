---
level: basic
---

# Events

A busy bus delivers thousands of frames a second, each with a time of its own, and a reader usually wants a few of
them. A [[tape]] keeps such items in an [[events chunk]], with their times, ends and [[payloads]] in columns. A reader
finds the first [[event]] after a [[tick]] and reads it in place, without decoding the events before it.

The drive's emergency frame at 52:00, for example, lies on `can0` among a thousand frames of that second. A reader
finds it by its time, reads what it says, and reads the frames that followed it.

## Instants and spans

An [[event]] is an [[item]] with a time of its own. An event that happens at one tick is an instant, such as a
received frame. An event that lasts is a span, with a start and an end, such as a motor run from start to stop. A
track's definition says which kind its events are ([`EventsDef`]); the drive's `can0` holds instants.

A chunk stores the ends in one of three end modes ([`EndMode`]), whichever costs the least:

- **Absent**: every item is an instant, and the chunk stores no ends.
- **Next start**: each item ends where the next one starts, as states that follow each other do. The chunk stores no
  ends either, and the last item ends at the chunk's latest end.
- **Durations**: each item's length, stored per group of 128 items.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/end_modes.svg"
<figcaption>The three end modes. Instants store no end; spans that follow each other end at the next start; spans
with gaps between them store a duration each. The chunk head's latest end bounds every span, so an overlap query
skips chunks that end before its tick.</figcaption>
</figure>

The chunk head holds the latest end of any item in the chunk, and its [[index entry]] does too. A query for the spans
that cover a tick therefore skips every chunk whose latest end lies before the tick, without reading it.

## Times read in place

An events chunk keeps the start ticks of its items in one column, in [[time groups]] of 128 items. Each
group stores a line through its starts, and each start as a small distance from that line. [Event times and their
codecs](columns.md) explains this codec, and a reader needs none of its detail. [`EventTimes::read`] checks the
columns once. It then gives any item's time from its group's head and one load.

| Call | Gives |
|---|---|
| [`EventTimes::start`] | item `j`'s start tick |
| [`EventTimes::end`] | item `j`'s end: its start for an instant |
| [`EventTimes::flag`] | item `j`'s flags, 0 when the chunk stores none |
| [`Starts::lower_bound`] | the first item that starts at or after a tick |
| [`Starts::last_at_or_before`] | the last item that starts at or before a tick |

A search for a tick bisects the groups' first ticks, then uses the group's line to narrow the items inside it. A
viewer finds the first frame at or after the fault this way.

## Payloads read in place

Each item's value, its [[payload]], is encoded in shrink_wrap, or kept as given on a track of other items. A chunk keeps
the payloads back to back in one column, and their lengths in another, packed per group. [`Payloads::read`] opens both,
and [`Payloads::get`] gives item `j`'s bytes from one group of lengths, without the payloads before it. A payload column
that the writer compressed with lz4 is decoded once, whole, when the reader opens it.

A payload may be a whole frame as received. A protocol analyser keeps every frame of a network link that way, with the
headers its decoder finds on tracks beside them. [Keeping what matters of a 10 Gbps link](protocol_analyser.md) shows
how.

The drive's frames carry their 11-bit identifier in the first 4 bytes of the payload and the frame's data after it.
The program reads the chunk that holds the first frame at or after the fault, then every frame of the next 5 ms:

```rs
--8<-- "examples/example_drive/src/fault.rs:frames"
```

??? example "What the example uses here: `fault_us`, `the fault's constants`, `key`, `Frame, Frames`"

    ```rs
    --8<-- "examples/example_drive/src/fault.rs:fault_us_item"

    --8<-- "examples/example_drive/src/fault.rs:fault_consts_item"

    --8<-- "examples/example_drive/src/defs.rs:key_item"

    --8<-- "examples/example_drive/src/fault.rs:frame_types_item"
    ```

The chunk holds 1 000 frames in a block of about 1.7 KB. The first frame at or after 52:00 is item 3 120 000 of
`can0`, the emergency frame with identifier `0x081`. Its data begins `10 23`: the error code `0x2310`, overcurrent,
stored little-endian. The next four frames, items 3 120 001 to 3 120 004, are the drive's status, temperature,
voltage and command frames, one a millisecond. The step reads about 117 KB in 4 reads, almost all of it
`can0`'s list in the index.

## Flags

A track can give its events a 32-bit flags word, with a severity in its low bits. The writer stores the flags per
group of 128, beside the bitwise OR of the group's flags. A viewer that has zoomed out colours a stretch by those ORs
alone, without reading an item. A chunk in which every item's flags are 0 stores no flags column, and the drive's
frames have none.

## From an item number to the event

The item number names an event across the whole recording. A reader that holds item 3 120 000 of `can0`, such as
a marker saved in a viewer, finds it in two steps:

1. Among `can0`'s [[index entries]], it takes the one whose `first_item` and `count` hold 3 120 000.
2. In that chunk, it reads item `3 120 000 - first_item` with the calls above.

The address stays valid when the pieces are joined into one file or kept apart. The next chapter reads the current
that led to the fault: the samples of `phase_a` in the last 20 ms before 52:00.
