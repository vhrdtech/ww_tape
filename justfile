# `just jobs=N <recipe>` caps build and test parallelism for that run; unset leaves CARGO_BUILD_JOBS and
# NEXTEST_TEST_THREADS as inherited, or at cargo's default (the CPU count)
jobs := ""
export CARGO_BUILD_JOBS := if jobs != "" { jobs } else { env_var_or_default('CARGO_BUILD_JOBS', num_cpus()) }
export NEXTEST_TEST_THREADS := if jobs != "" { jobs } else { env_var_or_default('NEXTEST_TEST_THREADS', 'num-cpus') }
# Cargo's own warnings (an unused workspace dependency, a manifest key) are errors, as in CI, whose
# setup-rust-toolchain sets CARGO_BUILD_WARNINGS=deny: a tree red in CI cannot pass lint, test or the gate
export CARGO_BUILD_WARNINGS := env_var_or_default('CARGO_BUILD_WARNINGS', 'deny')

# `bench-book`'s RF chain: the directory it records into and the GiB it grows to. The directory needs that much free
# disk and is removed after the read; the default sits under target/, on disk
rf_dir := "target/bench-rf"
rf_gib := "100"

# The WireWeaver command line that checks the API snapshot: `ww` from PATH, or the binary `WW` names
ww := env_var_or_default('WW', 'ww')

fuzz_targets := "reader resume reclose chunk summary index times samples series payloads records-hostile chunk-hostile times-hostile payloads-hostile samples-hostile series-hostile summary-hostile index-hostile reader-hostile state-hostile"
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
# The release of spirit_level's book extension (spl_book_ext): the same as the `spl` in use, since `spl book refs`
# writes the code index the extension reads
spl := "0.7.9"
book_ext := "spl-book-ext @ git+https://github.com/romixlab/spirit_level@v" + spl + "#subdirectory=python/spl_book_ext"
# The prose lints book-check runs: at deny in spirit_level.toml
prose_lints := "--only prose-run-words --only page-reading-time --only heading-words --only code-wall --only code-block-language --only passive-voice --only reading-ease --only orphan-pages --only page-title --only abbreviation-first-use --only term-before-definition --only paragraph-sentences --only sentence-words --only emoji --only page-links --only nav-entries"

default:
    @just --list

# The tests. `just test`: all of them (test-all). `just test --changed`: only the crates changed against main and
# the crates that depend on them (through tpm when installed, otherwise the full suite)
test *flags:
    {{ if flags == "--changed" { "changedtests run" } else { just_executable() + " test-all " + flags } }}

# Every test: nextest (unit and golden tests, the seed replay of every fuzz target, the hostile streams), the
# doctests, the builds of `just check`, and a short run of every fuzz target
test-all: check
    cargo nextest run --workspace --no-fail-fast
    # nextest does not run doctests
    cargo test --workspace --doc -q
    just fuzz-smoke

# cargo check what tests don't build: the positional reader, the writer and the chain for the browser, without std
# (READ-1); the block, chunk, summary node and codec readers without alloc, warnings as errors
check:
    cargo check -p ww_tape --no-default-features --features=alloc --target wasm32-unknown-unknown
    RUSTFLAGS="-D warnings" cargo check -p ww_tape --no-default-features --target wasm32-unknown-unknown
    RUSTFLAGS="-D warnings" cargo check -p ww_tape_summary --no-default-features --features=alloc --target wasm32-unknown-unknown
    RUSTFLAGS="-D warnings" cargo check -p ww_tape_summary --no-default-features --target wasm32-unknown-unknown
    cargo check -p ww_tape_codecs --no-default-features --target wasm32-unknown-unknown

# Format, manifests, spelling, clippy and rustdoc as CI runs them, warnings are errors; then `just api-check` and
# every lint of spirit_level.toml (`spl check`)
lint:
    cargo fmt --all --check
    cd fuzz && cargo +{{ nightly }} fmt --check
    cargo sort -w -g --check
    typos
    cargo clippy --workspace --all-targets -- -D warnings
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
    just api-check
    spl check --progress never

# ww_tape's version and wire line against its newest snapshot (`ww api check`); skipped with a note without `ww`
api-check:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! command -v {{ ww }} >/dev/null 2>&1; then
        echo "skipped: ww api check, ww missing (install WireWeaver's ww, or set WW to its path)"
        exit 0
    fi
    {{ ww }} api check ww_tape

# Rewrite the golden files of ww_tape and ww_tape_summary (golden/v1*.hex) after a deliberate format change (FMT-2, FMT-3, CODEC-2, SUM-1, FMT-9)
tape-golden-update:
    WW_TAPE_GOLDEN=update cargo nextest run -p ww_tape -p ww_tape_summary -p ww_tape_tests golden

# Float sample codecs of ww_tape on motor_sim, decoded speech and ADC data (CODEC-4); fetches the public-domain
# speech file (Roosevelt's Pearl Harbor address, Wikimedia Commons) once and checks it; `just bench-sample-codecs --help`
bench-sample-codecs *args:
    mkdir -p target/bench-data
    test -f target/bench-data/Roosevelt_Pearl_Harbor.ogg || curl -fsSL -o target/bench-data/Roosevelt_Pearl_Harbor.ogg https://upload.wikimedia.org/wikipedia/commons/f/fd/Roosevelt_Pearl_Harbor.ogg
    echo "90239988a1fc36a44f6531207c918544980bfea0  target/bench-data/Roosevelt_Pearl_Harbor.ogg" | sha1sum -c --quiet
    cargo run -q --release -p ww_tape_codecs_benchmarks -- --speech target/bench-data/Roosevelt_Pearl_Harbor.ogg {{ args }}

# Series summaries of ww_tape (SUM-3): MB/s per element type through the RAW codec, the summaries and both;
# `just bench-summaries --help`
bench-summaries *args:
    cargo run -q --release -p ww_tape_benchmarks --bin summaries -- {{ args }}

# ww_tape tracks added one by one, then the bytes put and copied per 256 MiB piece (FMT-9); `just bench-tracks --help`
bench-tracks *args:
    cargo run -q --release -p ww_tape_benchmarks --bin tracks -- {{ args }}

# ww_tape: the definitions a piece carries for the tracks of its head, per piece (WRITE-9); `just bench-carried --help`
bench-carried *args:
    cargo run -q --release -p ww_tape_benchmarks --bin carried -- {{ args }}

# ww_tape series chunks: ns per 1 MiB chunk the writer spends, and its parts (WRITE-4); `just bench-chunks --help`
bench-chunks *args:
    cargo run -q --release -p ww_tape_benchmarks --bin chunks -- {{ args }}

# ww_tape: the heap a writer holds over one 256 MiB piece, by tracks and block kind (BOOK-2); `just bench-memory --help`
bench-memory *args:
    cargo run -q --release -p ww_tape_benchmarks --bin memory -- {{ args }}

# ww_tape event times against raw ticks, varints and Elias-Fano: bytes, coding, item j and first at or after a tick
# (CODEC-2); the system journal workload reads this machine's last 20 days of journal times, when journalctl has them
bench-event-times *args:
    mkdir -p target/bench-data
    -journalctl -q --no-pager -o short-unix --since -20d | cut -d" " -f1 > target/bench-data/journal.txt
    cargo run -q --release -p ww_tape_benchmarks --bin event_times -- --journal target/bench-data/journal.txt {{ args }}

# ww_tape events chunk starts column: open intact and salvaged, get and is_suspect per item (READ-2); `just bench-starts --help`
bench-starts *args:
    cargo run -q --release -p ww_tape_benchmarks --bin starts -- {{ args }}

# ww_tape track relations: column bytes, encode, read, lower bound and write_chunk; `just bench-relations --help`
bench-relations *args:
    cargo run -q --release -p ww_tape_benchmarks --bin relations -- {{ args }}

# The book's running example at full scale: the drive's hour by block kind, bytes per item and time to record
# (BOOK-3); `just bench-drive --help`
bench-drive *args:
    cargo run -q --release -p ww_tape_benchmarks --bin drive -- {{ args }}

# ww_tape: the RF capture's writer and reader on one core, per codec (BOOK-2); `just bench-rf-core --help`
bench-rf-core *args:
    cargo run -q --release -p ww_tape_benchmarks --bin rf_core -- {{ args }}

# ww_tape: the RF capture into a chain of piece files in `--dir` at full speed, then read back; `just bench-rf-chain --help`
bench-rf-chain *args:
    cargo run -q --release -p ww_tape_benchmarks --bin rf_chain -- {{ args }}

# Worst wait of a reader of a memory piece while it is written, with and without reserving it (WRITE-5); `just bench-mem-reader --help`
bench-mem-reader *args:
    cargo run -q --release -p ww_tape_benchmarks --bin mem_reader -- {{ args }}

# Every bench the book draws from ("Sizes and speeds", "The definitions a new piece carries"), at release, into
# docs/bench/*.json; then the charts and tables drawn from them (`just book-figures`). `just jobs=N bench-book` caps
# the build's jobs. Each bench runs on one core, except rf_chain: 8 workers and a writer thread.
# rf_chain needs `rf_gib` GiB free on the disk of `rf_dir` (100 GiB by default), and removes its chain after the read:
# `just rf_dir=/data/rf bench-book` puts it elsewhere.
# A report names the commit it ran, so the tree must be clean, and every bench is built before the first report is written
bench-book:
    test -z "$(git status --porcelain)" || (echo "bench-book: commit or drop the changes first, a report names its commit" && exit 1)
    cargo build -q --release -p ww_tape_benchmarks -p ww_tape_codecs_benchmarks
    mkdir -p docs/bench
    just bench-sample-codecs --json docs/bench/sample_codecs.json
    just bench-event-times --json docs/bench/event_times.json
    just bench-summaries --json docs/bench/summaries.json
    just bench-chunks --json docs/bench/chunks.json
    just bench-tracks --json docs/bench/tracks.json
    just bench-drive --json docs/bench/drive.json
    just bench-carried --json docs/bench/carried.json
    just bench-memory --json docs/bench/memory.json
    just bench-rf-core --json docs/bench/rf_core.json
    just bench-rf-chain --dir {{ rf_dir }} --gib {{ rf_gib }} --remove --json docs/bench/rf_chain.json

# Build the book (docs/, zensical.toml) into site/ with Zensical
[group('book')]
book: book-refs
    uv run --with {{ zensical }} --with "{{ book_ext }}" zensical build --clean

# Serve the book at http://localhost:8000 and rebuild it on every change
[group('book')]
book-serve: book-refs
    uv run --with {{ zensical }} --with "{{ book_ext }}" zensical serve

# The book's checks: the strict build, then the prose lints, so a broken link or a long page cannot land
[group('book')]
book-check: book-strict book-prose

# Build the book with warnings as errors: a link to a missing page, anchor or figure fails; links to other sites are not fetched
[group('book')]
book-strict: book-refs
    uv run --with {{ zensical }} --with "{{ book_ext }}" zensical build --clean --strict

# Check the glossary (docs/glossary.toml) and write what the book's references read: the index of the crates' public
# items (target/spl-book/code_index.json), the cards' stylesheet and script, and the footer's build stamp and its age
# script (not in git)
[group('book')]
book-refs:
    spl book refs
    spl book stamp --name ww_tape overrides/partials/stamp.html docs/javascripts/book-stamp-age.js

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

# Copy the data figures eventfull renders from ww_tape_example's tapes into docs/shots, with the eventfull commit they
# came from; refuses when that checkout's shots differ from its commit. No eventfull build here
[group('book')]
book-shots eventfull="../eventfull":
    test -z "$(git -C {{ eventfull }} status --porcelain -- crates/eventfull_book_shots/shots)"
    cp {{ eventfull }}/crates/eventfull_book_shots/shots/*.png {{ eventfull }}/crates/eventfull_book_shots/shots/manifest.toml docs/shots/
    echo "eventfull = \"$(git -C {{ eventfull }} rev-parse HEAD)\"" >> docs/shots/manifest.toml

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

# Fuzz ww_tape's writer resuming a cut and mutated unclosed piece: it closes whole (WRITE-2)
[working-directory('fuzz')]
fuzz-tape-resume:
    cargo +{{ nightly }} fuzz run --target {{ fuzz_host_triple }} tape-resume -- -max_len=512 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz a piece closed more than once, its last append torn and mutated: the newest whole close is in force (WRITE-7)
[working-directory('fuzz')]
fuzz-tape-reclose:
    cargo +{{ nightly }} fuzz run --target {{ fuzz_host_triple }} tape-reclose -- -max_len=512 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape chunk blocks: reading and salvage on any bytes, a flipped bit costs one segment (FMT-3)
[working-directory('fuzz')]
fuzz-tape-chunk:
    cargo +{{ nightly }} fuzz run --target {{ fuzz_host_triple }} tape-chunk -- -max_len=262144 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape summary blocks: reading any bytes, trees built from the input read back (SUM-1)
[working-directory('fuzz')]
fuzz-tape-summary:
    cargo +{{ nightly }} fuzz run --target {{ fuzz_host_triple }} tape-summary -- -max_len=16384 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape index, close, tail and piece head blocks and the positional reader on any bytes (FMT-4, READ-1)
[working-directory('fuzz')]
fuzz-tape-index:
    cargo +{{ nightly }} fuzz run --target {{ fuzz_host_triple }} tape-index -- -max_len=65536 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape's event time codec: reads and salvage on any bytes, round trips, kernels against scalar (CODEC-2)
[working-directory('fuzz')]
fuzz-tape-times:
    cargo +{{ nightly }} fuzz run --target {{ fuzz_host_triple }} tape-times -- -max_len=65536 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape sample codecs and lz4 payloads: reading any bytes, round trips of values from the input (CODEC-1)
[working-directory('fuzz')]
fuzz-tape-samples:
    cargo +{{ nightly }} fuzz run --target {{ fuzz_host_triple }} tape-samples -- -max_len=65536 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape series tracks: track kinds from any bytes, chunk heads on grids, damaged and in any order (FMT-6)
[working-directory('fuzz')]
fuzz-tape-series:
    cargo +{{ nightly }} fuzz run --target {{ fuzz_host_triple }} tape-series -- -max_len=4096 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape's payload codec: reads and salvage on any bytes, round trips, damaged columns (CODEC-3)
[working-directory('fuzz')]
fuzz-tape-payloads:
    cargo +{{ nightly }} fuzz run --target {{ fuzz_host_triple }} tape-payloads -- -max_len=65536 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape records blocks, of data or of definitions, of hostile clock tables, record heads, lengths and coded bodies (READ-2)
[working-directory('fuzz')]
fuzz-tape-records-hostile:
    cargo +{{ nightly }} fuzz run --target {{ fuzz_host_triple }} tape-records-hostile -- -max_len=256 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape chunk blocks of a hostile head: payload offset, segment shift, column table, counts, ticks (READ-2)
[working-directory('fuzz')]
fuzz-tape-chunk-hostile:
    cargo +{{ nightly }} fuzz run --target {{ fuzz_host_triple }} tape-chunk-hostile -- -max_len=256 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape events chunks of hostile time group records, read and salvaged (READ-2)
[working-directory('fuzz')]
fuzz-tape-times-hostile:
    cargo +{{ nightly }} fuzz run --target {{ fuzz_host_triple }} tape-times-hostile -- -max_len=256 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape events chunks of hostile payload lengths, lz4 lengths and codecs, read and salvaged (READ-2)
[working-directory('fuzz')]
fuzz-tape-payloads-hostile:
    cargo +{{ nightly }} fuzz run --target {{ fuzz_host_triple }} tape-payloads-hostile -- -max_len=256 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape series chunks of hostile sample columns, counts and shapes (READ-2)
[working-directory('fuzz')]
fuzz-tape-samples-hostile:
    cargo +{{ nightly }} fuzz run --target {{ fuzz_host_triple }} tape-samples-hostile -- -max_len=256 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape series tracks of hostile periods and chunk heads of hostile grids (READ-2)
[working-directory('fuzz')]
fuzz-tape-series-hostile:
    cargo +{{ nightly }} fuzz run --target {{ fuzz_host_triple }} tape-series-hostile -- -max_len=256 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape summary blocks of a hostile head, scale or node values (READ-2)
[working-directory('fuzz')]
fuzz-tape-summary-hostile:
    cargo +{{ nightly }} fuzz run --target {{ fuzz_host_triple }} tape-summary-hostile -- -max_len=256 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape index, close, piece head and tail blocks of hostile offsets, directories and entries (READ-2)
[working-directory('fuzz')]
fuzz-tape-index-hostile:
    cargo +{{ nightly }} fuzz run --target {{ fuzz_host_triple }} tape-index-hostile -- -max_len=256 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape pieces of hostile offsets, lengths, layouts and gaps, through the positional reader and the walks (READ-2)
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

# Fuzz ww_tape state records and their index entries lying behind good CRCs: lookups and resume (FMT-9)
[working-directory('fuzz')]
fuzz-tape-state-hostile:
    cargo +{{ nightly }} fuzz run --target {{ fuzz_host_triple }} tape-state-hostile -- -max_len=256 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10
