# AGENTS.md

Guidance for AI code assistants working in this repository.

## What this is

ww_tape is the tape format of [WireWeaver](https://github.com/vhrdtech/wire_weaver): tracks of typed records
in self-checking, variable-length blocks, for live feeds, log files and raw media. It is layer 3 (storage) of
WireWeaver.

It takes [shrink_wrap](https://github.com/romixlab/shrink_wrap) and
[exact_units](https://github.com/romixlab/exact_units) by git, and `ww_version` from
[ww_base](https://github.com/vhrdtech/ww_base). Of the framework it needs only one
dev-dependency, `wire_weaver_evolution`, for the API snapshot check.

WireWeaver (the link tap, the client's recorder, `ww explain`) and applications take it by git.

Read `docs/concepts.md` first (the words), then the pages in the order `docs/index.md` lists them. `docs/format.md`
is the reference, block by block.

The exact byte layouts are in the module docs (`ww_tape/src/lib.rs` and each module's `mod.rs`). Where the docs are
unclear, the source and its comments are authoritative.

The docs are the ww_tape book, a Zensical site of its own: `zensical.toml` configures it and `just book-check` builds
it. It links the WireWeaver book (`https://ww.vhrd.tech/...`) and the logring book by URL and includes neither.

WireWeaver's book links here by URL too, so keep the headings other books link to. `docs/writers.md` says how a
chapter reads and how it marks terms, links code and includes tested code.

## Layout

| Path | What |
| --- | --- |
| `ww_tape/` | blocks, definitions on track 0, state tracks and their lookups, chunks and their time, sample and payload columns, summary heads, index, close and tail, pieces, `TapeWriter`, `FileBlocks`, `TapeReader`; `golden/` the golden files |
| `ww_tape_codecs/` | column codecs with no notion of a block, over slices, ids and scalars: `bitpack`, `crc32c`, `from_file`, varints, `lz4`, `samples`, `times`, `lengths`, the summary kernels; `ww_tape` re-exports them |
| `ww_tape_summary/` | summary nodes above the core: `EventTree`, `SeriesSummaries`, `Summary` read in place, `EventNode`, `ChannelStats`, scales; re-exports the head types; `golden/` its golden file |
| `ww_tape_mux/` | `TapeMux`: a chain written from its own thread; handles (`EventsHandle`, `SeriesHandle`, `ChunkSender`, `RecordsHandle`, `RelayHandle`, `StateHandle`) send through a byte-bounded queue; `ProgressWatch` for readers in its process |
| `ww_tape_fs/` | everything that names a path: `FileSink`, `Files`, `FileSource`, the lock on the open file and its fork handler, `writer::{create, append, resume, cut}`, `chain::{create, resume}`, `mux::{create, resume}`, `follow` (`ChainFollower`, `TrackFollower`, `TimeMerge`) |
| `ww_tape_tests/` | the integration tests (`tests/it`), through the public API; the book pages against the code, their `rust` blocks as doctests (`src/book.rs`) |
| `ww_tape_fuzz/` | the fuzz target bodies (`targets`), hostile block builders (`hostile`), `seeds/` and their replay as tests, `ww_tape_fuzz_seeds` |
| `ww_tape_bench/` | what every bench binary shares: `--json`, `--color`, `--version`, completion, the timing loop, the run's report |
| `ww_tape_bench_report/` | a bench report as its JSON holds it: commit, build, host and machine, tables with their load; serde only |
| `ww_tape_codecs_benchmarks/` | float sample codecs on simulated, decoded and sensor data |
| `ww_tape_benchmarks/` | one binary per bench: `summaries`, `tracks`, `chunks`, `drive`, `event_times`, `starts`, `relations`, `mem_reader`, `carried`, `memory`, `rf_core`, `rf_chain`, `handles` (README lists what each measures) |
| `ww_tape_example/` | the book's running example: the drive motor controller's hour at any scale, a crash at any byte, the resume; the RF capture (`rf`); `cargo run -p ww_tape_example` prints the story's numbers |
| `ww_tape_figures/` | the book's figures that show bytes or numbers, drawn from blocks ww_tape writes into `docs/plots/` (`just book-figures`; a test checks them) |
| `fuzz/` | cargo-fuzz targets (`tape-*`), a workspace of its own on nightly; each target calls `ww_tape_fuzz` |
| `docs/` | the pages of the format; `figures/` drawings by hand, `plots/` drawn by `ww_tape_figures`, `shots/` eventfull screenshots of recorded data (`just book-shots` copies them from an eventfull checkout) |

What builds on ww_tape stays in WireWeaver: `ww_tape_bundle` (track name paths from an API bundle, above `ww_self`),
`ww_tap` (the link recorder), `tests/tape_identity`, the client's tap decoder.

## FEATURES.md

`FEATURES.md` is the feature tracker and roadmap, the only place status is kept. Check it before starting a feature
and update the matching item in the same change when finishing one (conventions at its top).

- IDs carry their area's prefix (`FMT`, `WRITE`, `READ`, `FS`, `CODEC`, `SUM`, `BOOK`, `REPO`) and are stable:
  never renumber or reuse one. A new item takes the next free number of its area.
- Use IDs in commits, CHANGELOG entries and `// TODO(FMT-1): ...`.
- When talking to the user, name an ID with a 2-4 word kebab-case slug from its title: `FMT-4 index-close-tail`.

## Commands

```sh
just test --changed   # the crates changed against main and their dependents; before a session's work is ready
just test             # everything: nextest, doctests, just check, a smoke run of every fuzz target
just lint             # fmt, cargo sort -w -g, typos, clippy, rustdoc -D warnings, just api-check, spl check
just check            # the reader, writer, chain and codecs for wasm32 without std (READ-1)
just api-check        # ww_tape's version and wire line against ww_tape/api_snapshots/; skipped without `ww`
just fuzz-tape-<target>   # one fuzz target until stopped, with its libFuzzer limits
just tape-golden-update   # rewrite golden/*.hex after a deliberate format change
just bench-<name>         # one bench at release, `--json FILE` for its report (ww_tape_benchmarks/README.md lists them)
just bench-book           # every bench the book draws from into docs/bench/; `just book-figures` draws them
just book-check           # the book (docs/) with warnings as errors, then spl's prose lints (deny)
just book-refs            # spl book refs: checks docs/glossary.toml, indexes the code links; every book build runs it
```

- Single test: `cargo nextest run -p ww_tape <test>`, or `-p ww_tape_tests` for an integration test. nextest is
  required.
- Plain `cargo sort` flattens grouped tables: always pass `-w -g`.
- Project words go into `typos.toml`.
- CI (`.github/workflows/ci.yml`) fails on any warning.
- `ww_tape`, `ww_tape_codecs`, `ww_tape_summary`, `ww_tape_mux`, `ww_tape_fuzz` and `ww_tape_tests` build at opt-level
  3 in tests: the hostile streams decode a million blocks, the mux tests resume at every block boundary.

## The format is a contract

- Golden files (`ww_tape/golden/v1*.hex`, `ww_tape_summary/golden/`) and a determinism test pin the bytes of every
  block kind. The same input gives the same bytes.
- A change of what any input writes is a format change. Say so in a breaking CHANGELOG entry and rewrite the goldens
  with `just tape-golden-update` in the same commit.
- After a format change, check the version byte and the readers of old files.
- Readers take damaged or hostile bytes. A number read from a block is a `FromFile<T>` (READ-2).
- Such a number reaches a length, an allocation or a loop only through a named cap or a length already known. The
  caps are `pub const`s listed in the crate docs' table of caps.
- The workspace `[lints]` deny panics, unchecked arithmetic, indexing and casts. An exception is an
  `#[expect(.., reason = "..")]` naming its invariant.
- Every decoder has a fuzz target and a hostile target. A fixed input goes into `ww_tape_fuzz/src/seeds.rs`
  (`FIXED_INPUTS`, or a `fix-<commit>-<what>` seed builder) and `cargo run -p ww_tape_fuzz --bin ww_tape_fuzz_seeds
  -- write` regenerates `seeds/`; a known, unfixed failure goes into `KNOWN_INPUTS` and `tests/it/known.rs`.
- Claims about bytes, sizes or speeds in docs are never hand-derived: run the code or the bench and copy the output.

## Speed and vector kernels

`bitpack` and the summary kernels (`ww_tape_codecs`) have AVX-512, AVX2 and NEON paths picked at run time. Tests and
fuzz targets check each one against the scalar path.

Measure a change to a hot path with the benches before and after, at release. The numbers go into the commit body
or the docs page they change.

## Proper fixes, never a lost capability

- A bug or limit found on the way is fixed where it lives, as its own commit, with a test that fails without it.
- A removal, replacement or refactor keeps everything that worked before. Never narrow tests, docs or
  `FEATURES.md` to fit a loss.
- Flaky and slow tests: find the cause, never raise a limit (`.config/nextest.toml` holds hang detectors only).

## Dependencies

- Git dependencies have no `rev` or `tag`: `Cargo.lock` pins the commit, `cargo update -p NAME` moves it.
- To develop alongside a dependency, put a `[patch."https://github.com/..."]` into an uncommitted
  `.cargo/config.toml` in a parent directory. Never put it into `Cargo.toml`, and commit no `Cargo.lock` that names
  a patched path.
- WireWeaver must follow some changes: an API it calls, a format change. Check such a change there before it lands,
  with a `[patch."https://github.com/vhrdtech/ww_tape"]` and its tap tests.
- `ww_tape/api_snapshots/` is the crate's API snapshot, written by WireWeaver's CLI. `ww_tape/tests/evolution.rs`
  checks the source against it (`wire_weaver_evolution`, a dev-dependency only).
- After a change to `Describe`, a type it names or the crate docs, run `ww api save ww_tape` and commit the snapshot.
  `just lint` runs `just api-check` (`ww api check ww_tape`) to catch a stale one.
- Dependencies live in the root `[workspace.dependencies]`. Crates inherit `authors`, `edition`, `rust-version`,
  `license`, `repository` and `[lints]`.
- The workspace `[lints]` hold the panic-class lints. Tests, benches, the figures and the fuzz bodies allow those
  that fire at their crate root, with a reason.
- No C build step: `lz4_flex` is pure Rust and pinned exactly, since payload columns must compress to the same bytes.
- `cc` stays, allowed in `no-sys-crates`: it is a build-dependency of `blake3`. `blake3` is built `pure`, so its build
  script only probes for a C compiler and compiles no C.

## Changelogs, commits and versions

- Every user-visible change gets a line under `## [Unreleased]` in `CHANGELOG.md`, in the same change, with the item
  id. A breaking entry says what users must change.
- One section per kind (`### Breaking`, `### Features`, `### Fixes`), no blank lines inside a list.
- Commits use Conventional Commits with a scope (`feat(index): ...`) and a short imperative subject. The body has at
  most 5 lines on what and why, then a `Tested:` line.
- A fix names its culprit: `Fixes: <12+ hex> ("subject")`. A move is never edited in the same commit.
- Work commits never bump versions; the release commit does. Each landing turns `## [Unreleased]` into a version
  section.
- Pre-1.0 a breaking change bumps the minor, anything else the patch.
- Public text (README, docs, CHANGELOG, commits, comments) is neutral: no private paths or internal tracker names.
