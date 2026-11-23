# example_rf

The ww_tape book's case for high rates: complex IQ samples from a software-defined radio's front end, 20 million a second (80 MB/s). The signal is three steady carriers, a little noise and five bursts of 2 ms a minute (`example_capture::rf`). It comes from fixed numbers, so every capture is the same. A minute is one piece with one track, `iq`, in chunks of 10 ms with summaries.

The module `chains` writes the capture into two chains through two muxes in one process. A radio thread writes `iq`. A decoder thread finds the bursts, measures the band's power each second and writes them to `decoded`. A reader follows both while they are written and merges them by time.

```sh
cargo run --release -p example_rf                       # a minute's size in each codec
cargo run --release -p example_rf -- write rf.wwt 100   # the minute at 1/100 of the rate, into a file
cargo run --release -p example_rf -- chains rf 100      # the same minute in two chains under rf/, followed
```

`tests/rf.rs` checks that a capture reads back sample for sample and pins the size of a second at the full rate. It also checks the two chains: every burst found, every sample kept, the merge in time order. `ww_tape_benchmarks` times it: `--bin rf_core` on one core, `--bin rf_chain` into a chain of files.
