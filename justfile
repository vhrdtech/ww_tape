# `just jobs=N <recipe>` caps build and test parallelism for that run; unset leaves CARGO_BUILD_JOBS and
# NEXTEST_TEST_THREADS as inherited, or at cargo's default (the CPU count)
jobs := ""
export CARGO_BUILD_JOBS := if jobs != "" { jobs } else { env_var_or_default('CARGO_BUILD_JOBS', num_cpus()) }
export NEXTEST_TEST_THREADS := if jobs != "" { jobs } else { env_var_or_default('NEXTEST_TEST_THREADS', 'num-cpus') }

# The WireWeaver command line that checks the API snapshot: `ww` from PATH, or the binary `WW` names
ww := env_var_or_default('WW', 'ww')

fuzz_targets := "reader resume chunk summary index times samples series payloads records-hostile chunk-hostile times-hostile payloads-hostile samples-hostile series-hostile summary-hostile index-hostile reader-hostile state-hostile"
# cargo-fuzz defaults its fuzz target to the triple its own binary was built for; a
# prebuilt cargo-fuzz release (taiki-e/install-action's cargo-binstall fallback uses
# one) is a static musl build, so that default becomes a musl target this host never
# installed. Passing the host's own triple keeps the fuzz build on it regardless.
fuzz_host_triple := `rustc -vV | sed -n 's/^host: //p'`

# List all available targets
# The pinned nightly of the fuzz and miri recipes; rust-toolchain.toml pins the stable one
nightly := "nightly-2026-11-19"
# The Zensical release the book is built with, so a new release cannot break the build unseen
zensical := "zensical==0.1.2"
# The prose lints book-check runs: at deny in spirit_level.toml
prose_lints := "--only prose-run-words --only page-reading-time --only heading-words --only code-wall --only code-block-language --only passive-voice --only reading-ease --only orphan-pages --only page-title --only abbreviation-first-use --only term-before-definition --only paragraph-sentences --only sentence-words --only emoji --only page-links --only nav-entries"

default:
    @just --list

# The tests. `just test`: all of them (test-all). `just test --changed`: only the crates changed against main and
# the crates that depend on them (through tpm when installed, otherwise the full suite)
test *flags:
    if [ "{{ flags }}" = "--changed" ]; then \
        if command -v changed-tests >/dev/null 2>&1; then f=$(changed-tests) && if [ -n "$f" ]; then cargo nextest run --workspace -E "$f"; fi; \
        else echo "changed-tests not found; running the full test suite instead" >&2; {{ just_executable() }} test-all; fi; \
    else {{ just_executable() }} test-all {{ flags }}; fi

# Every test: nextest (unit and golden tests, the seed replay of every fuzz target, the hostile streams), the
# doctests, the builds of `just check`, and a short run of every fuzz target
test-all: check
    cargo nextest run --workspace --no-fail-fast
    # nextest does not run doctests
    cargo test --workspace --doc -q
    just fuzz-smoke

# cargo check what tests don't build: the positional reader, the writer and the chain for the browser, without std
# (STD-124); the block, chunk, summary node and codec readers without alloc, warnings as errors
check:
    cargo check -p ww_tape --no-default-features --features=alloc --target wasm32-unknown-unknown
    RUSTFLAGS="-D warnings" cargo check -p ww_tape --no-default-features --target wasm32-unknown-unknown
    RUSTFLAGS="-D warnings" cargo check -p ww_tape_summary --no-default-features --features=alloc --target wasm32-unknown-unknown
    RUSTFLAGS="-D warnings" cargo check -p ww_tape_summary --no-default-features --target wasm32-unknown-unknown
    cargo check -p ww_tape_codecs --no-default-features --target wasm32-unknown-unknown

# Format, manifests, spelling, clippy and rustdoc as CI runs them, warnings are errors; then `just api-check`
lint:
    cargo fmt --all --check
    cd fuzz && cargo +{{ nightly }} fmt --check
    cargo sort -w -g --check
    typos
    cargo clippy --workspace --all-targets -- -D warnings
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
    just api-check

# ww_tape's version and wire line against its newest snapshot (`ww api check`); skipped with a note without `ww`
api-check:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! command -v {{ ww }} >/dev/null 2>&1; then
        echo "skipped: ww api check, ww missing (install WireWeaver's ww, or set WW to its path)"
        exit 0
    fi
    {{ ww }} api check ww_tape

# Rewrite the golden files of ww_tape and ww_tape_summary (golden/v1*.hex) after a deliberate format change (STD-117, STD-118, STD-120, STD-121, STD-135)
tape-golden-update:
    WW_TAPE_GOLDEN=update cargo nextest run -p ww_tape -p ww_tape_summary -p ww_tape_tests golden

# Float sample codecs of ww_tape on motor_sim, decoded speech and ADC data (STD-130); fetches the public-domain
# speech file (Roosevelt's Pearl Harbor address, Wikimedia Commons) once and checks it; `just bench-sample-codecs --help`
bench-sample-codecs *args:
    mkdir -p target/bench-data
    test -f target/bench-data/Roosevelt_Pearl_Harbor.ogg || curl -fsSL -o target/bench-data/Roosevelt_Pearl_Harbor.ogg https://upload.wikimedia.org/wikipedia/commons/f/fd/Roosevelt_Pearl_Harbor.ogg
    echo "90239988a1fc36a44f6531207c918544980bfea0  target/bench-data/Roosevelt_Pearl_Harbor.ogg" | sha1sum -c --quiet
    cargo run -q --release -p ww_tape_codecs_benchmarks -- --speech target/bench-data/Roosevelt_Pearl_Harbor.ogg {{ args }}

# Series summaries of ww_tape (STD-133): MB/s per element type through the RAW codec, the summaries and both;
# `just bench-summaries --help`
bench-summaries *args:
    cargo run -q --release -p ww_tape_benchmarks --bin summaries -- {{ args }}

# ww_tape tracks added one by one, then the bytes put and copied per 256 MiB piece (STD-135); `just bench-tracks --help`
bench-tracks *args:
    cargo run -q --release -p ww_tape_benchmarks --bin tracks -- {{ args }}

# ww_tape series chunks: ns per 1 MiB chunk the writer spends, and its parts (TAPE-4); `just bench-chunks --help`
bench-chunks *args:
    cargo run -q --release -p ww_tape_benchmarks --bin chunks -- {{ args }}

# Worst wait of a reader of a memory piece while it is written, with and without reserving it (TAPE-6); `just bench-mem-reader --help`
bench-mem-reader *args:
    cargo run -q --release -p ww_tape_benchmarks --bin mem_reader -- {{ args }}

# Build the book (docs/, zensical.toml) into site/ with Zensical
[group('book')]
book:
    uv run --with {{ zensical }} zensical build --clean

# Serve the book at http://localhost:8000 and rebuild it on every change
[group('book')]
book-serve:
    uv run --with {{ zensical }} zensical serve

# The book's checks: the strict build, then the prose lints, so a broken link or a long page cannot land
[group('book')]
book-check: book-strict book-prose

# Build the book with warnings as errors: a link to a missing page, anchor or figure fails; links to other sites are not fetched
[group('book')]
book-strict:
    uv run --with {{ zensical }} zensical build --clean --strict

# The book's prose lints of spirit_level.toml (sentence and paragraph length, reading time, terms). Without spl it fails
# in CI and is skipped elsewhere
[group('book')]
book-prose:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! command -v spl >/dev/null 2>&1; then
        if [ -n "${CI:-}" ]; then
            echo "error: spl is missing in CI; install it as ci.yml does"
            exit 1
        fi
        echo "skipped: spl check of the book's prose, spl missing (install spirit_level's spl)"
        exit 0
    fi
    spl check --progress never {{ prose_lints }}

# Rewrite the figures of the book pages drawn from the format (docs/plots/*.svg, ww_tape_figures); a test fails until
# they match what it draws
book-figures:
    cargo run -q -p ww_tape_figures -- docs/plots

# Export the logo from assets/logo/logo.typ: SVG of each variant, PNG files at fixed sizes. Needs typst 0.13 or
# newer; the font (JetBrains Mono Bold, SIL OFL) is in assets/logo/fonts, system fonts are ignored
logo: logo-vector logo-png

logo-vector:
    just _logo ww_tape-light.svg lockup light
    just _logo ww_tape-dark.svg lockup dark
    just _logo mark-light.svg mark light
    just _logo mark-dark.svg mark dark
    just _logo wordmark-light.svg wordmark light
    just _logo wordmark-dark.svg wordmark dark
    just _logo icon.svg icon dark
    just _logo icon-small.svg icon-small dark
    just _logo ../../docs/assets/ww_tape_icon.svg icon-small dark

logo-png:
    just _logo png/ww_tape-light-640.png lockup light 640
    just _logo png/ww_tape-light-1280.png lockup light 1280
    just _logo png/ww_tape-dark-640.png lockup dark 640
    just _logo png/ww_tape-dark-1280.png lockup dark 1280
    just _logo png/mark-light-256.png mark light 256
    just _logo png/mark-light-512.png mark light 512
    just _logo png/mark-dark-256.png mark dark 256
    just _logo png/mark-dark-512.png mark dark 512
    just _logo png/icon-small-16.png icon-small dark 16
    just _logo png/icon-small-32.png icon-small dark 32
    just _logo png/icon-small-48.png icon-small dark 48
    just _logo png/icon-128.png icon dark 128
    just _logo png/icon-256.png icon dark 256
    just _logo png/icon-512.png icon dark 512
    just _logo png/social.png social dark

# One variant: the output's extension picks the format, size is the longer side in pixels (empty: natural size)
_logo out kind theme size="":
    typst compile --ppi 72 --font-path assets/logo/fonts --ignore-system-fonts --input kind={{ kind }} --input theme={{ theme }} --input size={{ size }} assets/logo/render.typ assets/logo/{{ out }}

# Install cargo-fuzz (fuzzing needs a nightly toolchain)
fuzz-install:
    cargo install cargo-fuzz

# Every fuzz target for 2000 runs: a smoke run, part of `just test` (the seed replay in ww_tape_fuzz runs every seed)
[working-directory('fuzz')]
fuzz-smoke:
    for t in {{ fuzz_targets }}; do cargo +{{ nightly }} fuzz run -O --target {{ fuzz_host_triple }} tape-$t -- -runs=2000 -max_len=4096 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10 || exit 1; done

# The targets below run with libFuzzer limits: -rss_limit_mb=1024 (1 GiB of memory), -malloc_limit_mb=128 (no
# single allocation over 128 MiB) and -timeout=10 (10 s per input); an input over one is a crash. The corpus a run
# grows (fuzz/corpus) and a crash (fuzz/artifacts) stay out of git.
# Fuzz the ww_tape file reader: prefix and salvage walks over any bytes
[working-directory('fuzz')]
fuzz-tape-reader:
    cargo +{{ nightly }} fuzz run --target {{ fuzz_host_triple }} tape-reader -- -max_len=65536 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape's writer resuming a cut and mutated unfinished piece: it finishes whole (STD-132)
[working-directory('fuzz')]
fuzz-tape-resume:
    cargo +{{ nightly }} fuzz run --target {{ fuzz_host_triple }} tape-resume -- -max_len=512 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape chunk blocks: reading and salvage on any bytes, a flipped bit costs one segment (STD-118)
[working-directory('fuzz')]
fuzz-tape-chunk:
    cargo +{{ nightly }} fuzz run --target {{ fuzz_host_triple }} tape-chunk -- -max_len=262144 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape summary blocks: reading any bytes, trees built from the input read back (STD-121)
[working-directory('fuzz')]
fuzz-tape-summary:
    cargo +{{ nightly }} fuzz run --target {{ fuzz_host_triple }} tape-summary -- -max_len=16384 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape index, finish, tail and piece head blocks and the positional reader on any bytes (STD-122, STD-124)
[working-directory('fuzz')]
fuzz-tape-index:
    cargo +{{ nightly }} fuzz run --target {{ fuzz_host_triple }} tape-index -- -max_len=65536 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape's event time codec: reads and salvage on any bytes, round trips, kernels against scalar (STD-120)
[working-directory('fuzz')]
fuzz-tape-times:
    cargo +{{ nightly }} fuzz run --target {{ fuzz_host_triple }} tape-times -- -max_len=65536 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape sample codecs and lz4 payloads: reading any bytes, round trips of values from the input (STD-119)
[working-directory('fuzz')]
fuzz-tape-samples:
    cargo +{{ nightly }} fuzz run --target {{ fuzz_host_triple }} tape-samples -- -max_len=65536 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape series tracks: track kinds from any bytes, chunk heads on grids, damaged and in any order (STD-127)
[working-directory('fuzz')]
fuzz-tape-series:
    cargo +{{ nightly }} fuzz run --target {{ fuzz_host_triple }} tape-series -- -max_len=4096 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape's payload codec: reads and salvage on any bytes, round trips, damaged columns (STD-126)
[working-directory('fuzz')]
fuzz-tape-payloads:
    cargo +{{ nightly }} fuzz run --target {{ fuzz_host_triple }} tape-payloads -- -max_len=65536 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape records blocks, of data or of definitions, of hostile clock tables, record heads, lengths and coded bodies (STD-131)
[working-directory('fuzz')]
fuzz-tape-records-hostile:
    cargo +{{ nightly }} fuzz run --target {{ fuzz_host_triple }} tape-records-hostile -- -max_len=256 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape chunk blocks of a hostile head: payload offset, segment shift, column table, counts, ticks (STD-131)
[working-directory('fuzz')]
fuzz-tape-chunk-hostile:
    cargo +{{ nightly }} fuzz run --target {{ fuzz_host_triple }} tape-chunk-hostile -- -max_len=256 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape events chunks of hostile time group records, read and salvaged (STD-131)
[working-directory('fuzz')]
fuzz-tape-times-hostile:
    cargo +{{ nightly }} fuzz run --target {{ fuzz_host_triple }} tape-times-hostile -- -max_len=256 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape events chunks of hostile payload lengths, lz4 lengths and codecs, read and salvaged (STD-131)
[working-directory('fuzz')]
fuzz-tape-payloads-hostile:
    cargo +{{ nightly }} fuzz run --target {{ fuzz_host_triple }} tape-payloads-hostile -- -max_len=256 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape series chunks of hostile sample columns, counts and shapes (STD-131)
[working-directory('fuzz')]
fuzz-tape-samples-hostile:
    cargo +{{ nightly }} fuzz run --target {{ fuzz_host_triple }} tape-samples-hostile -- -max_len=256 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape series tracks of hostile periods and chunk heads of hostile grids (STD-131)
[working-directory('fuzz')]
fuzz-tape-series-hostile:
    cargo +{{ nightly }} fuzz run --target {{ fuzz_host_triple }} tape-series-hostile -- -max_len=256 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape summary blocks of a hostile head, scale or node values (STD-131)
[working-directory('fuzz')]
fuzz-tape-summary-hostile:
    cargo +{{ nightly }} fuzz run --target {{ fuzz_host_triple }} tape-summary-hostile -- -max_len=256 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape index, finish, piece head and tail blocks of hostile offsets, directories and entries (STD-131)
[working-directory('fuzz')]
fuzz-tape-index-hostile:
    cargo +{{ nightly }} fuzz run --target {{ fuzz_host_triple }} tape-index-hostile -- -max_len=256 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape pieces of hostile offsets, lengths, layouts and gaps, through the positional reader and the walks (STD-131)
[working-directory('fuzz')]
fuzz-tape-reader-hostile:
    cargo +{{ nightly }} fuzz run --target {{ fuzz_host_triple }} tape-reader-hostile -- -max_len=256 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# A library has nothing to put on PATH: builds it with the deploy profile, as a dependent's release build would
install:
    cargo build --profile deploy --locked --workspace

# Bring this checkout up to main: refuses on local changes, pulls, builds
deploy:
    #!/usr/bin/env bash
    set -euo pipefail
    dirty=$(git status --porcelain)
    if [ -n "$dirty" ]; then echo "local changes, not deploying:"; echo "$dirty"; exit 1; fi
    git pull --ff-only
    just install

# Fuzz ww_tape state records and their index entries lying behind good CRCs: lookups and resume (STD-135)
[working-directory('fuzz')]
fuzz-tape-state-hostile:
    cargo +{{ nightly }} fuzz run --target {{ fuzz_host_triple }} tape-state-hostile -- -max_len=256 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10
