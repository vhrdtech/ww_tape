---
level: basic
---

# WireWeaver and logring

Two other books cover the projects that tapes are used with. Each book stands on its own, so this page says what
each project does and where it meets a tape. Read it when a page here names a WireWeaver type or a device log.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/related_books.svg"
<figcaption>Where the three projects meet. WireWeaver gives a tape the encoding of its items and the names of its
tracks, and records tapes; logring keeps a device's newest data on the device.</figcaption>
</figure>

## WireWeaver: typed APIs between devices and hosts

[WireWeaver](https://ww.vhrd.tech/) is a framework for APIs between microcontrollers and host programs. An API is
written once as a Rust trait, and WireWeaver generates the device side and the host side from it. The messages travel
in shrink_wrap, a dense binary format that needs no allocator on the device.

A tape takes two parts of WireWeaver:

| Part | What it is | What a tape does with it |
|---|---|---|
| shrink_wrap | the binary encoding of a typed value | stores each item of an events track in it, decoded by the same code as on the wire |
| API bundle | the compiled description of an API: its types, resources and names | stores the bundles of the recorded devices, so a reader names every track without the device |

The samples of a series are the exception: they are numbers in columns, coded by ww_tape's own codecs. WireWeaver
also writes and reads tapes. Its [link tap](https://ww.vhrd.tech/transport/tap/) records the traffic of a link into a
tape, and its command line tool `ww` reads tapes back.

## logring: a log store on the device

[logring](https://lr.vhrd.tech/) is a log store for embedded devices. Firmware writes tracks of shrink_wrap items
into a fixed-size ring on an SD card or a flash chip. When the ring is full, the newest data overwrites the oldest, and
a power cut costs at most the last second of data.

A logring and a tape both hold tracks of typed items with times, but they answer different needs:

| | logring | tape |
|---|---|---|
| Keeps | the newest data | a whole recording |
| Lives | on the device itself | mostly on a host |
| Built for | no heap and little RAM | fast reads of one track or a long overview |

The logring book compares the two on the points that decide the choice, in
[Tapes and logring](https://lr.vhrd.tech/tapes_and_logring/).
