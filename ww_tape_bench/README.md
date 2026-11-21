# ww_tape_bench

What every bench binary of ww_tape shares (`ww_tape_benchmarks`, `ww_tape_codecs_benchmarks`):

- the common command line: `--color`, `--json FILE`, `--version`, shell completion (`source <(COMPLETE=bash chunks)`);
- the timing loop: the best of a number of rounds, in ns per call;
- the run: a report (`ww_tape_bench_report`) with the commit and whether the tree was dirty, the build profile,
  opt-level and jobs, the machine (host, CPU model, logical CPUs, vector extensions, virtual or not), and the load
  average before and after each table; tables printed as Markdown, and written as JSON with `--json`.

The book's charts and tables of measured numbers are drawn from those JSON files (`docs/bench/`, `ww_tape_figures`).
`just bench-book` runs every bench into them.
