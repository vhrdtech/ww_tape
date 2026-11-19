# AGENTS.md

Guidance for AI code assistants working in this repository.

## What this is

ww_tape is the tape format of [WireWeaver](https://github.com/vhrdtech/wire_weaver): tracks of ShrinkWrap records
in self-checking, variable-length blocks, for live feeds, log files and raw media. It is layer 3 (storage) of
WireWeaver: it takes [shrink_wrap](https://github.com/romixlab/shrink_wrap),
[exact_units](https://github.com/romixlab/exact_units) and `ww_version` from
[ww_base](https://github.com/vhrdtech/ww_base) by git, and depends on nothing of the framework but one
dev-dependency, `wire_weaver_evolution`, for the API snapshot check. WireWeaver (the link tap, the client's recorder,
`ww explain`) and applications take it by git.

Read `docs/concepts.md` first (the words), then `docs/index.md`, `docs/writing.md`, `docs/reading.md`,
`docs/events_and_series.md`, `docs/recovery.md` and `docs/format.md` (the reference, block by block). The pages and
their order are listed in `docs/index.md`. The exact byte layouts are in the module docs (`ww_tape/src/lib.rs` and each
module's `mod.rs`). The docs are the ww_tape book, a Zensical site of its own (`zensical.toml`, `just book-check`):
it links the WireWeaver book (`https://ww.vhrd.tech/...`) and the logring book by URL and includes neither, and
WireWeaver's book links here by URL too, so keep the headings other books link to. Where the docs are unclear, the
source and its comments are authoritative.

## Layout

| Path | What |
| --- | --- |
| `ww_tape/` | blocks, definitions on track 0, state tracks and their lookups, chunks and their time, sample and payload columns, summary heads, index, finish and tail, pieces, `TapeWriter`, `FileBlocks`, `TapeReader`; `golden/` the golden files |
| `ww_tape_codecs/` | column codecs with no notion of a block, over slices, ids and scalars: `bitpack`, `crc32c`, `from_file`, varints, `lz4`, `samples`, `times`, `lengths`, the summary kernels; `ww_tape` re-exports them |
| `ww_tape_summary/` | summary nodes above the core: `EventTree`, `SeriesSummaries`, `Summary` read in place, `EventNode`, `ChannelStats`, scales; re-exports the head types; `golden/` its golden file |
| `ww_tape_fs/` | everything that names a path: `FileSink`, `Files`, `FileSource`, the lock on `NAME.lock`, `writer::{create, append, resume, cut}`, `chain::{create, resume}` |
| `ww_tape_tests/` | the integration tests (`tests/it`), through the public API; the book pages against the code |
| `ww_tape_fuzz/` | the fuzz target bodies (`targets`), hostile block builders (`hostile`), `seeds/` and their replay as tests, `ww_tape_fuzz_seeds` |
| `ww_tape_codecs_benchmarks/` | float sample codecs on simulated, decoded and sensor data |
| `ww_tape_benchmarks/` | series summaries (`--bin summaries`); tracks added one by one, puts and copies (`--bin tracks`); the writer's cost per series chunk (`--bin chunks`) |
| `ww_tape_figures/` | the book's figures that show bytes or numbers, drawn from blocks ww_tape writes into `docs/plots/` (`just book-figures`; a test checks them) |
| `fuzz/` | cargo-fuzz targets (`tape-*`), a workspace of its own on nightly; each target calls `ww_tape_fuzz` |
| `docs/` | the pages of the format; `figures/` drawings by hand, `plots/` drawn by `ww_tape_figures` |

What builds on ww_tape stays in WireWeaver: `ww_tape_bundle` (track name paths from an API bundle, above `ww_self`),
`ww_tap` (the link recorder), `tests/tape_identity`, the client's tap decoder.

## FEATURES.md

`FEATURES.md` is the feature tracker and roadmap, the only place status is kept. Check it before starting a feature
and update the matching item in the same change when finishing one (conventions at its top).

- IDs are stable: never renumber or reuse one. The items came from WireWeaver with their `STD` IDs (`STD-42`,
  `STD-117` to `STD-133`); new items take the next free number of the area `TAPE`. Use them in commits, CHANGELOG
  entries and `// TODO(TAPE-1): ...`.
- When talking to the user, name an ID with a 2-4 word kebab-case slug from its title: `STD-122 index-finish-tail`.

## Commands

```sh
just test --changed   # the crates changed against main and their dependents; before a session's work is ready
just test             # everything: nextest, doctests, just check, a smoke run of every fuzz target
just lint             # fmt, cargo sort -w -g, typos, clippy, rustdoc -D warnings, just api-check
just check            # the reader, writer, chain and codecs for wasm32 without std (STD-124)
just api-check        # ww_tape's version and wire line against ww_tape/api_snapshots/; skipped without `ww`
just fuzz-tape-<target>   # one fuzz target until stopped, with its libFuzzer limits
just tape-golden-update   # rewrite golden/*.hex after a deliberate format change
just bench-sample-codecs  # float codecs (fetches a public-domain speech file once)
just bench-summaries      # series summary throughput
just bench-tracks         # tracks added one by one; puts and copies per 256 MiB piece
just bench-chunks         # ns per 1 MiB series chunk the writer spends, and its parts
just book-check           # the book (docs/) with warnings as errors: a broken link or anchor fails
```

Single test: `cargo nextest run -p ww_tape <test>`, or `-p ww_tape_tests` for an integration test (nextest is
required). Plain `cargo sort` flattens grouped tables: always pass `-w -g`. Project words go into `typos.toml`. CI
(`.github/workflows/ci.yml`) fails on any warning. `ww_tape`, `ww_tape_codecs`, `ww_tape_summary`, `ww_tape_fuzz` and
`ww_tape_tests` build at opt-level 3 in tests: the hostile streams decode a million blocks.

## The format is a contract

- The bytes of every block kind are pinned by golden files (`ww_tape/golden/v1*.hex`, `ww_tape_summary/golden/`)
  and a determinism test: the same input gives the same bytes. A change of what any input writes is a format change:
  say so in a breaking CHANGELOG entry, rewrite the goldens with `just tape-golden-update` in the same commit, and
  check the version byte and the readers of old files.
- Readers take damaged or hostile bytes. A number read from a block is a `FromFile<T>` and reaches a length, an
  allocation or a loop only through a named cap (a `pub const` listed in the crate docs' table of caps) or a length
  already known (STD-131). `ww_tape`'s `[lints]` deny panics, unchecked arithmetic, indexing and casts; an exception
  is an `#[expect(.., reason = "..")]` naming its invariant.
- Every decoder has a fuzz target and a hostile target. A fixed input goes into `ww_tape_fuzz/src/seeds.rs`
  (`FIXED_INPUTS`, or a `fix-<commit>-<what>` seed builder) and `cargo run -p ww_tape_fuzz --bin ww_tape_fuzz_seeds
  -- write` regenerates `seeds/`; a known, unfixed failure goes into `KNOWN_INPUTS` and `tests/it/known.rs`.
- Claims about bytes, sizes or speeds in docs are never hand-derived: run the code or the bench and copy the output.

## Speed and vector kernels

`bitpack` and the summary kernels (`ww_tape_codecs`) have AVX-512, AVX2 and NEON paths picked at run time, each
checked against the scalar path by tests and fuzz targets. A change to a hot path is measured with the benches before
and after, at release, and the numbers go into the commit body or the docs page they change.

## Proper fixes, never a lost capability

- A bug or limit found on the way is fixed where it lives, as its own commit, with a test that fails without it.
- A removal, replacement or refactor keeps everything that worked before. Never narrow tests, docs or
  `FEATURES.md` to fit a loss.
- Flaky and slow tests: find the cause, never raise a limit (`.config/nextest.toml` holds hang detectors only).

## Dependencies

- Git dependencies have no `rev` or `tag`: `Cargo.lock` pins the commit, `cargo update -p NAME` moves it.
- To develop alongside a dependency, put a `[patch."https://github.com/..."]` into an uncommitted
  `.cargo/config.toml` outside the repository (a parent directory), never into `Cargo.toml`, and commit no
  `Cargo.lock` that names a patched path.
- A change that WireWeaver must follow (an API it calls, a format change) is checked there before it lands: develop
  WireWeaver with a `[patch."https://github.com/vhrdtech/ww_tape"]` and run its tap tests.
- `ww_tape/api_snapshots/` is the crate's API snapshot, written by WireWeaver's CLI; `ww_tape/tests/evolution.rs`
  checks the source against it (`wire_weaver_evolution`, a dev-dependency only). After a change to `Describe`, a type
  it names or the crate docs, run `ww api save ww_tape` in this repository and commit it. `just lint` runs
  `just api-check` (`ww api check ww_tape`) to catch a snapshot left stale.
- Dependencies live in the root `[workspace.dependencies]`; crates inherit `authors`, `edition`, `rust-version`,
  `license`, `repository` and `[lints]` (`ww_tape`, `ww_tape_summary` and `ww_tape_fs` keep their own `[lints]` table: the
  workspace's plus the panic-class lints).
- No C build step: `blake3` is built `pure`, `lz4_flex` is pure Rust and pinned exactly, since payload columns must
  compress to the same bytes.

## Changelogs, commits and versions

- Every user-visible change gets a line under `## Unreleased` in `CHANGELOG.md`, in the same change, with the item
  id; one section per kind (`### ⚠️ Breaking`, `### 🚀 Features`, `### 🐛 Fixes`), no blank lines inside a list. For
  a breaking change say what users must change.
- Commits use Conventional Commits with a scope (`feat(index): ...`): a short imperative subject, a blank line, a
  body of at most 5 lines saying what and why, and a `Tested:` line. A fix names its culprit:
  `Fixes: <12+ hex> ("subject")`. A move is never edited in the same commit.
- Work commits never bump versions; the release commit does. Each landing turns `## Unreleased` into a version
  section. Pre-1.0 a breaking change bumps the minor, anything else the patch.
- Public text (README, docs, CHANGELOG, commits, comments) is neutral: no private paths or internal tracker names.
