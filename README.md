<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo/ww_tape-dark.svg">
    <img alt="ww_tape" src="assets/logo/ww_tape-light.svg" width="420">
  </picture>
</p>

[![CI](https://github.com/vhrdtech/ww_tape/actions/workflows/ci.yml/badge.svg)](https://github.com/vhrdtech/ww_tape/actions/workflows/ci.yml)
[![Book](https://img.shields.io/badge/book-tape.vhrd.tech-blue)](https://tape.vhrd.tech/)
[![crates.io](https://img.shields.io/crates/v/ww_tape)](https://crates.io/crates/ww_tape)
[![crates.io](https://img.shields.io/crates/v/ww_tape_codecs)](https://crates.io/crates/ww_tape_codecs)
[![docs.rs](https://img.shields.io/docsrs/ww_tape)](https://docs.rs/ww_tape)
[![docs.rs](https://img.shields.io/docsrs/ww_tape_codecs)](https://docs.rs/ww_tape_codecs)
[![spirit level](.github/spl-badge.svg)](https://spl.vhrd.tech/tiers/small-workspace/)

> Tracks of typed records in self-checking, variable-length blocks, for live feeds, log files and raw media.

A **tape** is how [WireWeaver](https://github.com/vhrdtech/wire_weaver) records data over time. It holds what a device
streamed, what crossed a link and what a program computed from either.

One format serves a live feed, a file on a PC and a raw medium. An example of a raw medium is a
SD card in a data logger.

- **Typed data.** An event's payload is a ShrinkWrap value, and the file carries the API bundle that defines its type.
  A series stores its samples as numbers in columns.
- **Exact time.** Times are integer ticks of a named clock with an exact rate; a file holds tracks on many clocks.
- **Reads in place.** Large tracks are stored in columns that a reader queries as they lie in the file.
- **Damage stays local.** A crash, a torn write or a flipped bit costs the bytes it touched, and a reader says what
  was lost.

With the `alloc` feature, `ww_tape` is `no_std` and builds for `wasm32-unknown-unknown`. It then has the positional
reader, the writers and the piece chain on a sink of the caller's. The `std` feature (the default) adds memory sinks, the
system clock, reading blocks from a stream and owned definitions.

`ww_tape_fs` writes files and holds everything that names a path. Both crates depend on
[shrink_wrap](https://github.com/romixlab/shrink_wrap), [exact_units](https://github.com/romixlab/exact_units) and
`ww_version` from [ww_base](https://github.com/vhrdtech/ww_base), and on nothing of the WireWeaver framework.

## Documentation

The pages in [`docs/`](docs) explain the format. They start with these:

- [Tapes](docs/index.md): records, clocks, the anatomy of a file;
- [Writing a tape](docs/writing.md) and [Chunks from columns](docs/columns.md);
- [When bytes go bad](docs/damage.md);
- the [Format reference](docs/format.md).

The pages form the ww_tape book, which `just book` builds with Zensical.

The ww_tape book stands apart from the [WireWeaver book](https://ww.vhrd.tech/) and the
[logring book](https://lr.vhrd.tech/), and it links to them by URL. The exact byte layouts are in the
crate's module docs.

## Repository layout

<!-- spirit-level:crates -->
| Crate | Description |
|---|---|
| [`example_capture`](examples/example_capture) | What the ww_tape book's cases share: the capture into a window in memory and its triage, the RF signal |
| [`example_drive`](examples/example_drive) | The drive motor controller's recording that the book's figures, code and numbers use, at any scale |
| [`example_ethernet`](examples/example_ethernet) | The ww_tape book's protocol analyser: both directions of a 10 Gbps Ethernet link, captured into memory and triaged |
| [`example_images`](examples/example_images) | The ww_tape book's image sequence: frames of pixels on a 2-D grid over time, read by box and level |
| [`example_rf`](examples/example_rf) | The ww_tape book's RF capture: IQ at 20 MS/s into a piece, or into two chains from two muxes, followed live |
| [`example_thermal`](examples/example_thermal) | The ww_tape book's thermal field: a plate of sensors on a 2-D grid over time, read by box, region and level |
| [`example_wideband`](examples/example_wideband) | The ww_tape book's wideband receiver: 1 GS/s of IQ in channels over several muxes, captured into memory and triaged |
| [`ww_tape`](ww_tape) | Tape blocks: tracks of typed records in self-checking, variable-length blocks, for live feeds and log files |
| [`ww_tape_bench`](ww_tape_bench) | What every ww_tape bench binary shares: its command line, its timing loop, and its report with the build, machine and load |
| [`ww_tape_bench_report`](ww_tape_bench_report) | The report of a ww_tape bench run as its JSON holds it: build, commit, machine, tables and load |
| [`ww_tape_benchmarks`](ww_tape_benchmarks) | Benchmarks of ww_tape: summaries, tracks, carried definitions, starts, relations, the writer's cost per chunk and its heap, the RF capture, the protocol analyser, the wideband receiver, the mux's handles, a chain's pieces verified and range-hashed in parallel |
| [`ww_tape_codecs`](ww_tape_codecs) | Column codecs of ww_tape: bit packing, CRC-32C, varints, lz4, samples, times, payload lengths and summary kernels, with no notion of a block |
| [`ww_tape_codecs_benchmarks`](ww_tape_codecs_benchmarks) | Benchmarks of ww_tape_codecs: the float sample codec and bench-only ones on simulated, decoded and sensor data |
| [`ww_tape_figures`](ww_tape_figures) | The figures of ww_tape's book pages that hold bytes or numbers, drawn as SVG from blocks ww_tape writes |
| [`ww_tape_fs`](ww_tape_fs) | Files of ww_tape: a file sink and store of piece files, their lock, and writers, chains and muxes opened by path |
| [`ww_tape_fuzz`](ww_tape_fuzz) | Fuzz target bodies, hostile block builders and seed replay for ww_tape's decoders |
| [`ww_tape_mux`](ww_tape_mux) | A writing thread for ww_tape chains: producers on any thread send through handles and a byte-bounded queue |
| [`ww_tape_summary`](ww_tape_summary) | Summary trees of ww_tape tracks: event item trees and series statistics, built into summary blocks and read from them |
| [`ww_tape_tests`](ww_tape_tests) | Integration tests of ww_tape and its file layer, through their public API |
<!-- /spirit-level:crates -->

Beside the crates:

| Path | What |
| --- | --- |
| `fuzz/` | cargo-fuzz targets (`tape-*`), a workspace of its own on nightly |
| `docs/` | the pages of the format |

`just --list` shows the recipes: `just test`, `just lint`, `just check`, `just fuzz-tape-<target>`,
`just bench-sample-codecs`, `just bench-summaries`, `just bench-tracks`, `just bench-chunks`, `just tape-golden-update`.

## Depending on it

```toml
[dependencies]
ww_tape = { git = "https://github.com/vhrdtech/ww_tape", default-features = false, features = ["alloc"] }
```

Leave out `default-features = false` for memory sinks, the system clock and stream reading (`std`). To write files, add
`ww_tape_fs = { git = "https://github.com/vhrdtech/ww_tape" }`.

## License

The MIT License, see [LICENSE](LICENSE).
