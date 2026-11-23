# example_ethernet

The ww_tape book's protocol analyser: both directions of a 10 Gbps Ethernet link. Every frame is an event of the track `frames` with its bytes. A decoder finds each frame's IPv4 header and its TCP or UDP header. It writes them to the tracks `ipv4` and `l4`, each item related to its frame. The traffic is synthetic: the simple IMIX over a set of flows, with random payloads, made from the frame's number.

A port spreads its frames over receive queues, as a NIC does. Each queue has a capture thread and a decoder thread, since a track has one producer. Both ports go through one mux, or a mux per port into a chain each.

The module `ram` captures the link into the memory window (`example_capture::window`): a chain of pieces kept in memory per port, each written by its mux. A triage thread per chain reads each piece as it closes and keeps the pieces around each TCP reset. The resets and the pieces kept go to a results chain.

```sh
cargo run --release -p example_ethernet --bin ethernet_sizes         # what each track takes for 10 ms of the link
cargo run --release -p example_ethernet --bin ethernet_sizes -- 100  # the same for 100 ms
```

`tests/ethernet.rs` reads every frame and header back and pins the sizes the book quotes. `tests/window.rs` and `tests/ethernet_ram.rs` check what the window keeps and that every reset lies in a kept piece. `ww_tape_benchmarks` times it: `--bin ethernet`, or `just bench-ethernet`.
