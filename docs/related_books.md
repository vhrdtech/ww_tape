---
level: basic
---

# WireWeaver and logring

Two other books cover the projects that tapes are used with. Each book stands on its own, and therefore this page says
what the other two are about and where they connect to tapes.

Read this page when a page here mentions a WireWeaver type or a device log and you want the background.

## WireWeaver: typed APIs between devices and hosts

[WireWeaver](https://ww.vhrd.tech/) is a framework for APIs between microcontrollers and host
programs. An API is written once as a Rust trait, and WireWeaver generates the device side and the host side from it.

The messages travel in shrink_wrap, a dense binary format that needs no allocator on the device.

Tapes build on two parts of WireWeaver:

- **shrink_wrap.** The item of an events track is a value in its shrink_wrap encoding. The track names the item's
  type, and the reader decodes it with the same code that decodes it on the wire. The samples of a series are numbers
  in columns instead, coded by ww_tape's own codecs.
- **API bundles.** A bundle is the compiled description of an API: its types, its resources and their names. A tape
  stores the bundles of the devices it records, so a reader can name every track and decode every item without the
  device.

WireWeaver also produces tapes. Its link tap records the traffic of a WireWeaver link into a tape, and its command
line tool `ww` reads tapes back.

The [link tap](https://ww.vhrd.tech/transport/tap/) page of the WireWeaver book describes that recorder.

## logring: a log store on the device

[logring](https://lr.vhrd.tech/) is a log store for embedded devices. Firmware writes tracks of
shrink_wrap items into a fixed-size ring on a SD card or a flash chip.

When the ring is full, the newest data overwrites the oldest. A power cut costs at most the last second of data.

A logring and a tape both hold tracks of typed items with times, but they answer different needs:

- A logring keeps the newest data on the device itself, with no heap and little RAM.
- A tape keeps a whole recording, mostly on a host, and is built for fast reads of one track or a long overview.

The logring book compares the two on the points that decide the
choice, in [Tapes and logring](https://lr.vhrd.tech/tapes_and_logring/).
