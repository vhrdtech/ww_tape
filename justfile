# `just jobs=N <recipe>` caps build and test parallelism for that run; unset leaves CARGO_BUILD_JOBS and
# NEXTEST_TEST_THREADS as inherited, or at cargo's default (the CPU count)
jobs := ""
export CARGO_BUILD_JOBS := if jobs != "" { jobs } else { env_var_or_default('CARGO_BUILD_JOBS', num_cpus()) }
export NEXTEST_TEST_THREADS := if jobs != "" { jobs } else { env_var_or_default('NEXTEST_TEST_THREADS', 'num-cpus') }

fuzz_targets := "reader resume chunk summary index times samples series payloads records-hostile chunk-hostile times-hostile payloads-hostile samples-hostile series-hostile summary-hostile index-hostile reader-hostile state-hostile"

# List all available targets
default:
    @just --list

# The tests. `just test`: all of them (test-all). `just test --changed`: only the crates changed against main and
# the crates that depend on them (through tpm when installed, otherwise the full suite)
test *flags:
    if [ "{{ flags }}" = "--changed" ]; then \
        if command -v tpm >/dev/null 2>&1; then tpm test changed -- cargo nextest run --workspace; \
        else echo "tpm not found; running the full test suite instead" >&2; {{ just_executable() }} test-all; fi; \
    else {{ just_executable() }} test-all {{ flags }}; fi

# Every test: nextest (unit and golden tests, the seed replay of every fuzz target, the hostile streams), the
# doctests, the builds of `just check`, and a short run of every fuzz target
test-all: check
    cargo nextest run --workspace --no-fail-fast
    # nextest does not run doctests
    cargo test --workspace --doc -q
    just fuzz-smoke

# cargo check what tests don't build: the positional reader for the browser, without std (STD-124)
check:
    cargo check -p ww_tape --no-default-features --features=alloc --target wasm32-unknown-unknown

# Format, manifests, spelling, clippy and rustdoc as CI runs them, warnings are errors
lint:
    cargo fmt --all --check
    cd fuzz && cargo +nightly fmt --check
    cargo sort -w -g --check
    typos
    cargo clippy --workspace --all-targets -- -D warnings
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features

# Rewrite ww_tape's golden files (golden/v1*.hex) after a deliberate format change (STD-117, STD-118, STD-120, STD-121, STD-135)
tape-golden-update:
    WW_TAPE_GOLDEN=update cargo nextest run -p ww_tape golden

# Float sample codecs of ww_tape on motor_sim, decoded speech and ADC data (STD-130); fetches the public-domain
# speech file (Roosevelt's Pearl Harbor address, Wikimedia Commons) once and checks it; `just bench-sample-codecs --help`
bench-sample-codecs *args:
    mkdir -p target/bench-data
    test -f target/bench-data/Roosevelt_Pearl_Harbor.ogg || curl -fsSL -o target/bench-data/Roosevelt_Pearl_Harbor.ogg https://upload.wikimedia.org/wikipedia/commons/f/fd/Roosevelt_Pearl_Harbor.ogg
    echo "90239988a1fc36a44f6531207c918544980bfea0  target/bench-data/Roosevelt_Pearl_Harbor.ogg" | sha1sum -c --quiet
    cargo run -q --release -p ww_tape_benchmarks -- --speech target/bench-data/Roosevelt_Pearl_Harbor.ogg {{ args }}

# Series summaries of ww_tape (STD-133): MB/s per element type through the RAW codec, the summaries and both;
# `just bench-summaries --help`
bench-summaries *args:
    cargo run -q --release -p ww_tape_benchmarks --bin summaries -- {{ args }}

# Install cargo-fuzz (fuzzing needs a nightly toolchain)
fuzz-install:
    cargo install cargo-fuzz

# Every fuzz target for 2000 runs: a smoke run, part of `just test` (the seed replay in ww_tape_fuzz runs every seed)
[working-directory('fuzz')]
fuzz-smoke:
    for t in {{ fuzz_targets }}; do cargo +nightly fuzz run -O tape-$t -- -runs=2000 -max_len=4096 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10 || exit 1; done

# The targets below run with libFuzzer limits: -rss_limit_mb=1024 (1 GiB of memory), -malloc_limit_mb=128 (no
# single allocation over 128 MiB) and -timeout=10 (10 s per input); an input over one is a crash. The corpus a run
# grows (fuzz/corpus) and a crash (fuzz/artifacts) stay out of git.
# Fuzz the ww_tape file reader: prefix and salvage walks over any bytes
[working-directory('fuzz')]
fuzz-tape-reader:
    cargo +nightly fuzz run tape-reader -- -max_len=65536 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape's writer resuming a cut and mutated unfinished piece: it finishes whole (STD-132)
[working-directory('fuzz')]
fuzz-tape-resume:
    cargo +nightly fuzz run tape-resume -- -max_len=512 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape chunk blocks: reading and salvage on any bytes, a flipped bit costs one segment (STD-118)
[working-directory('fuzz')]
fuzz-tape-chunk:
    cargo +nightly fuzz run tape-chunk -- -max_len=262144 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape summary blocks: reading any bytes, trees built from the input read back (STD-121)
[working-directory('fuzz')]
fuzz-tape-summary:
    cargo +nightly fuzz run tape-summary -- -max_len=16384 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape index, finish, tail and piece head blocks and the positional reader on any bytes (STD-122, STD-124)
[working-directory('fuzz')]
fuzz-tape-index:
    cargo +nightly fuzz run tape-index -- -max_len=65536 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape's event time codec: reads and salvage on any bytes, round trips, kernels against scalar (STD-120)
[working-directory('fuzz')]
fuzz-tape-times:
    cargo +nightly fuzz run tape-times -- -max_len=65536 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape sample codecs and lz4 payloads: reading any bytes, round trips of values from the input (STD-119)
[working-directory('fuzz')]
fuzz-tape-samples:
    cargo +nightly fuzz run tape-samples -- -max_len=65536 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape series tracks: track kinds and segment records from any bytes, damaged segment records (STD-127)
[working-directory('fuzz')]
fuzz-tape-series:
    cargo +nightly fuzz run tape-series -- -max_len=4096 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape's payload codec: reads and salvage on any bytes, round trips, damaged columns (STD-126)
[working-directory('fuzz')]
fuzz-tape-payloads:
    cargo +nightly fuzz run tape-payloads -- -max_len=65536 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape records blocks, of data or of definitions, of hostile clock tables, record heads, lengths and coded bodies (STD-131)
[working-directory('fuzz')]
fuzz-tape-records-hostile:
    cargo +nightly fuzz run tape-records-hostile -- -max_len=256 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape chunk blocks of a hostile head: payload offset, segment shift, column table, counts, ticks (STD-131)
[working-directory('fuzz')]
fuzz-tape-chunk-hostile:
    cargo +nightly fuzz run tape-chunk-hostile -- -max_len=256 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape events chunks of hostile time group records, read and salvaged (STD-131)
[working-directory('fuzz')]
fuzz-tape-times-hostile:
    cargo +nightly fuzz run tape-times-hostile -- -max_len=256 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape events chunks of hostile payload lengths, lz4 lengths and codecs, read and salvaged (STD-131)
[working-directory('fuzz')]
fuzz-tape-payloads-hostile:
    cargo +nightly fuzz run tape-payloads-hostile -- -max_len=256 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape series chunks of hostile sample columns, counts and shapes (STD-131)
[working-directory('fuzz')]
fuzz-tape-samples-hostile:
    cargo +nightly fuzz run tape-samples-hostile -- -max_len=256 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape series tracks of hostile periods and segment records (STD-131)
[working-directory('fuzz')]
fuzz-tape-series-hostile:
    cargo +nightly fuzz run tape-series-hostile -- -max_len=256 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape summary blocks of a hostile head, scale or node values (STD-131)
[working-directory('fuzz')]
fuzz-tape-summary-hostile:
    cargo +nightly fuzz run tape-summary-hostile -- -max_len=256 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape index, finish, piece head and tail blocks of hostile offsets, directories and entries (STD-131)
[working-directory('fuzz')]
fuzz-tape-index-hostile:
    cargo +nightly fuzz run tape-index-hostile -- -max_len=256 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

# Fuzz ww_tape pieces of hostile offsets, lengths, layouts and gaps, through the positional reader and the walks (STD-131)
[working-directory('fuzz')]
fuzz-tape-reader-hostile:
    cargo +nightly fuzz run tape-reader-hostile -- -max_len=256 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10

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
    cargo +nightly fuzz run tape-state-hostile -- -max_len=256 -rss_limit_mb=1024 -malloc_limit_mb=128 -timeout=10
