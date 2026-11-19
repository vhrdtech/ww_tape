# ww_tape_benchmarks

Benchmarks of `ww_tape`, one binary each. The float sample codecs are measured in `ww_tape_codecs_benchmarks`.

The `summaries` binary measures series summaries: MB/s of sample bytes per element type through the RAW codec alone, `SeriesSummaries::push` with every statistic, and both, with the CPU's vector extensions and the load average.

```sh
just bench-summaries
just bench-summaries --frames 1024 --mib 64
```

The `tracks` binary adds tracks one by one, as a recorder that learns of a new source does: it holds each in its `Definitions` (`Definitions::define`), writes one records block on it (the writer puts the definition before it), finishes the piece and reads its definitions back, and prints the time to add the first 1 000, 5 000 and all tracks. It then writes a piece of `--piece-mib` (256 by default) with every track defined and records blocks of `--block-kib` (4) on the tracks in use in turn, and prints the bytes the writer put and copied: with every track in use, and with 1 % in use. Last, it prints the bytes a writer hashes and copies per block when nothing needs writing.

```sh
just bench-tracks
just bench-tracks --tracks 100000
```
