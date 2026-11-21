# ww_tape_bench_report

The report of a ww_tape bench run, as `--json` writes it: the bench, its `--version` line and commit, whether the tree
was dirty, the build profile and jobs, the machine (host, CPU, logical CPUs, extensions, virtual or not), the settings,
and each table with the load average before and after it.

Types only, with serde. `ww_tape_bench` measures and writes a report; `ww_tape_figures` reads the book's reports
(`docs/bench/`) back and refuses one that is not a release build of a clean commit.
