---
level: basic
---

# What a track's items are

A [[reader]] must know what an [[item]]'s bytes mean before it can show a value. A bus frame, a radio sample and a
network capture each have a format of their own. The track's [[definition]] names it in [`TrackDef::items`].

## The kinds of item { #what-a-tracks-items-are }

A track's items are one of five kinds. A tape keeps an events item's bytes as given and never parses them, so a new
kind of item needs no new block or codec.

| `Items` | One item is | Example |
|---|---|---|
| `WireWeaver` | a value of a stream of a WireWeaver API, in its shrink_wrap encoding | the drive's `mode` |
| `ShrinkWrap` | a value of a shrink_wrap type from a bundle, outside any API stream | a test bench's result per run |
| `RawBytes` | plain bytes with no type | frames as a sniffer received them |
| `Numbers` | one number of an element the definition describes, with an optional unit | the drive's `phase_a` |
| `Foreign` | bytes of a format with a name, a media type and a version | a pcap capture, `application/vnd.tcpdump.pcap` |

A [[WireWeaver value]] has a type defined in a WireWeaver API, such as a struct, an enum or a number. The tape
carries that type's definition in an [[API bundle]] and keeps the value in its shrink_wrap encoding. [WireWeaver and
logring](related_books.md) introduces both.

A foreign format may carry its schema, such as a `.proto` file, inside the tape. The schema is a definition of its
own, [`Describe::Schema`], named by the BLAKE3 hash of its bytes.

A newer ww_tape may add a kind of items. An older reader then reads that track's definition with its items marked
unknown, and every other field intact.

## Numbers of any element

The items of a [[series]] are always `Numbers`. The track keeps a device's numbers in the device's own layout, so
no predefined element has to match. An [`ElementDesc`] describes one number by these fields:

- its stored bits, its sign and its byte order;
- for a float, its exponent bits;
- for a pair such as I and Q, its two parts;
- the stored number that stands for zero.

A low-cost software-defined radio is such a device. It sends 8-bit IQ samples: each sample is two unsigned bytes, I
then Q, and the value 127.5 stands for zero. The track describes exactly that:

```rust
--8<-- "ww_tape_tests/tests/it/iq8.rs:def"
```

The writer stores the radio's bytes as sent, two bytes a sample, and a reader decodes them back to the same numbers.
[`iq8.rs`] writes such a track and reads it back. Summaries take the zero into account: they keep `2·v - 255` for
each part, so the power of a node is exact.

Floats of another layout, such as 16-bit or 8-bit floats, keep their own bits the same way. [Items of a
track](format.md#items-of-a-track) gives each field's range.
