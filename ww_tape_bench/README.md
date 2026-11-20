# ww_tape_bench

What every bench binary of ww_tape shares (`ww_tape_benchmarks`, `ww_tape_codecs_benchmarks`):

- the common command line: `--color`, `--json FILE`, shell completion (`source <(COMPLETE=bash chunks)`);
- the machine a run measures: CPU model, logical CPUs, vector extensions, and the load average before and after each
  table;
- the report: tables printed as Markdown, and written as JSON with `--json`.

The book's charts and tables of measured numbers are drawn from those JSON files (`docs/bench/`, `ww_tape_figures`).
`just bench-book` runs every bench into them.
