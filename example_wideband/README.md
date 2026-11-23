# example_wideband

The ww_tape book's wideband receiver: 1 billion complex samples a second, split into channels of equal width. Each channel is the RF signal (`example_capture::rf`) at its share of the rate, a series track with summaries and a producer thread of its own. Channel `k` goes to mux `k % m`, so several muxes write several chains from one process. `overview` reads the nodes of one summary level from every piece and no samples.

The module `ram` captures the band into the memory window (`example_capture::window`) and keeps the pieces around each burst, found from the summary nodes alone. The bursts and the pieces kept go to a results chain.

`tests/wideband.rs` reads every sample of every channel back, finds the bursts from the overview alone and pins the sizes the book quotes. `tests/wideband_ram.rs` checks what the window keeps and that every burst lies in a kept piece. `ww_tape_benchmarks` times it: `--bin wideband`, or `just bench-wideband`.
