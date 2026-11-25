---
level: advanced
---

# Checking many pieces at once

A long recording is a chain of [[pieces]]. Checking it piece by piece reads every byte on one thread. Each piece reads
alone, so a reader can verify one piece, or hash one track's chunks in it, without the others. An archive can therefore
check its pieces in parallel.

## What runs at once

`ww_tape_fs::check` takes the paths of piece files and gives one result per path, in the order given:

- `verify_each` runs [`TapeReader::verify`] on each piece, one piece at a time.
- `range_hash_each` runs [`TapeReader::range_hash`] on each piece, one piece at a time.
- `verify_each_parallel` and `range_hash_each_parallel` give the same results with the pieces spread over rayon's
  threads. They exist under the cargo feature `parallel`.

A test checks a chain of piece files both ways and compares the results:

```rust
--8<-- "ww_tape_tests/tests/it/check_parallel.rs:verify_parallel"
```

`ww_tape` itself keeps no thread pool and builds without the standard library. Only `ww_tape_fs` takes rayon.

Each piece is folded from the start of its [[track hash]], so the results are each piece's own. They are not one
hash of the whole chain. Chaining pieces into the chain's hash still folds every chunk in order, once.

## What it gains

The bench `archive_parallel` writes three sets of piece files, each where its operation does real BLAKE3 work. A
chain at `Full` is verified. A chain at `CrcOnly` is range-hashed: its index carries no unit hashes, so every chunk's
payload is read and hashed. Unclosed pieces are resumed, which walks each piece from its start.

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/archive_parallel.svg"
<figcaption>Each operation over its set of pieces, one piece at a time and spread over rayon's threads, at the
median of several runs on warm pages. Drawn by <code>ww_tape_figures</code> from the bench report.</figcaption>
</figure>

Every timed pass is preceded by reading each piece once, so both strategies run on warm pages. The two strategies run
several times each, alternating which goes first. The tables give the shortest, median and longest run.

--8<-- "docs/plots/bench_archive_parallel_verify.txt"

--8<-- "docs/plots/bench_archive_parallel_range_hash.txt"

--8<-- "docs/plots/bench_archive_parallel_resume.txt"

BLAKE3's own multithreaded hashing is a ceiling for hashing one piece, not a stand-in. A piece's content hash chains
and substitutes chunk payloads. It is not one buffer to hand to `update_rayon`.

--8<-- "docs/plots/bench_archive_parallel_blake3_raw.txt"

Reading is bound by the storage too. A chain larger than memory is read cold, and its speedup is what the storage
serves to several readers at once.

--8<-- "docs/plots/bench_archive_parallel_machine.txt"
