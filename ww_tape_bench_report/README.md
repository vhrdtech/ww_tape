# ww_tape_bench_report

The report of a ww_tape bench run, as `--json` writes it. It names the bench, its `--version` line, its commit and
whether the tree was dirty. It gives the build profile and jobs, and the machine: host, CPU, logical CPUs, extensions,
virtual or not. Then come the settings, and each table with the load average before and after it.

Types only, with serde. `ww_tape_bench` measures and writes a report; `ww_tape_figures` reads the book's reports
(`docs/bench/`) back and refuses one that is not a release build of a clean commit.
