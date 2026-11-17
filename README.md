# ww_tape

[![CI](https://github.com/vhrdtech/ww_tape/actions/workflows/ci.yml/badge.svg)](https://github.com/vhrdtech/ww_tape/actions/workflows/ci.yml)

> Tape blocks: tracks of ShrinkWrap records in self-checking, variable-length blocks, for live feeds, log files and
> raw media.

A **tape** is how [WireWeaver](https://github.com/vhrdtech/wire_weaver) records data over time: what a device
streamed, what crossed a link, what a program computed from either. One format serves a live feed, a file on a PC and
a raw medium such as an SD card in a data logger.

- **Typed data.** Every item is a ShrinkWrap type, and the file carries the API bundle that defines it.
- **Exact time.** Times are integer ticks of a named clock with an exact rate; a file holds tracks on many clocks.
- **Reads in place.** Large tracks are stored in columns that a reader queries as they lie in the file.
- **Damage stays local.** A crash, a torn write or a flipped bit costs the bytes it touched, and a reader says what
  was lost.

`ww_tape` is `no_std` with the `alloc` feature for the positional reader (it builds for `wasm32-unknown-unknown`);
`std` (the default) adds the writers, file reading and owned definitions. It depends on
[shrink_wrap](https://github.com/romixlab/shrink_wrap), [exact_units](https://github.com/romixlab/exact_units) and
`ww_version` from [ww_base](https://github.com/vhrdtech/ww_base), and on nothing of the WireWeaver framework.

## Documentation

The pages in [`docs/`](docs) explain the format: [Tapes](docs/index.md) (records, clocks, the anatomy of a file),
[Events and series](docs/events_and_series.md), [Damage and recovery](docs/recovery.md) and the
[Format reference](docs/format.md). They are published as the Tapes chapter of the
[WireWeaver book](https://ww.vhrd.tech/), which assembles them from this repository, so their links to other parts of
the book work there. The exact byte layouts are in the crate's module docs.

## Repository layout

| Path | What |
| --- | --- |
| `ww_tape/` | the format: blocks, definitions and state on track 0, chunks, codecs, summaries, index, pieces, writers and readers; `golden/` the golden files |
| `ww_tape_fuzz/` | fuzz target bodies, hostile block builders, the seed files (`seeds/`) and their replay as tests |
| `ww_tape_benchmarks/` | the float sample codec and series summary benchmarks |
| `fuzz/` | cargo-fuzz targets (`tape-*`), a workspace of its own on nightly |
| `docs/` | the pages of the format |

`just --list` shows the recipes: `just test`, `just lint`, `just check`, `just fuzz-tape-<target>`,
`just bench-sample-codecs`, `just bench-summaries`, `just tape-golden-update`.

## Depending on it

```toml
[dependencies]
ww_tape = { git = "https://github.com/vhrdtech/ww_tape", default-features = false, features = ["alloc"] }
```

Leave out `default-features = false` for the writers and file reading (`std`).

## License

MIT, see [LICENSE](LICENSE).
