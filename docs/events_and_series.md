# Events and series

The material of this page is now in the chapters on chunks, events and series. Each section below names where it went.

## Chunks

[Blocks](blocks.md#chunks) explains chunks and the two kinds of chunk, and [Chunks from columns](columns.md) how their
columns are made.

### The chunk head

[Chunks from columns](columns.md#a-chunk-is-a-head-and-its-columns) lists what a chunk head holds.

### Columns and codecs

[Chunks from columns](columns.md#a-chunk-is-a-head-and-its-columns) explains columns and codecs.

## Event times

[Chunks from columns](columns.md#event-times) explains the time modes and the time groups.

### Reading times in place

[Events](events.md#times-read-in-place) reads times in place.

### What it costs

[Numbers](numbers.md#event-times) gives the bytes and times per event, and compares the codec with two others.

### Ends and flags

[Chunks from columns](columns.md#ends-and-flags) explains how ends and flags are stored, and [Events](events.md#flags)
how a reader uses the flags.

### Payloads

[Chunks from columns](columns.md#payloads) explains the payload columns, and [Events](events.md#payloads-read-in-place)
how a reader reads them.
