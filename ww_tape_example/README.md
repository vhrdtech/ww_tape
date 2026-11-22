# ww_tape_example

The running example of the ww_tape book: a motor controller board, called drive, recorded for an hour on an endurance bench. The book's figures draw from it, its doctests call it, and its numbers come from it.

| Track | What | Kind in a tape |
|---|---|---|
| `log` | the firmware's log lines, 3 a second | records blocks |
| `mode` | the controller's mode: idle, run, fault | state track |
| `can0` | CAN frames on the vehicle bus, 1 kHz | events chunks |
| `phase_a` | phase current, 25 kHz, 16-bit fixed point in mA, with summaries | series chunks |
| `temp` | board temperature, 1 Hz | records blocks |

The host records the run into a chain of pieces in memory. The drive starts at minute 1 and faults on overcurrent at minute 52.

In the story, the host loses power at minute 37 while it writes the third piece. It then resumes.

`Drive::full()` is the story's hour. `Drive::scaled(k)` divides the data rates and the byte policies by `k`, for figures that must fit a page. A piece holds 15 minutes of the run at every scale, so every scale has four pieces and the crash in the third. The same settings give the same bytes, and a crash at any byte resumes to the bytes of the run without it.

```sh
cargo run --release -p ww_tape_example        # the story's numbers at full scale
cargo run --release -p ww_tape_example -- 1000  # the same at 1/1000
```

The archived last piece then takes damage three ways: a flipped bit, its first 4 KiB lost, its last 4 KiB lost. The run is also written onto a raw SD card that still holds an earlier run behind it.

`tests/story.rs` checks three things. The full hour is the same every run, and its crash lands where the story says. A scaled run resumes from a crash at any byte.

`tests/writing.rs` pins the numbers of the book's writing chapters in `tests/writing.out`. They are the calls and the blocks they make, the hour in an aligned file and the columns per codec. They also cover the crash per sync policy, the cut, the chain without its first piece, and the relay.

`tests/damage.rs` checks what each damage costs and that a reader of the raw card never takes the earlier run for data.

## The RF capture

The module `rf` records the book's case for high rates. Its samples are complex IQ values from a software-defined radio's front end, 20 million a second (80 MB/s). The signal is three steady carriers, a little noise and five bursts of 2 ms a minute. It comes from fixed numbers, so every capture is the same. A minute is one piece with one track, `iq`, in chunks of 10 ms with summaries.

The module `rf_chains` writes the capture into two chains through two muxes in one process. A radio thread writes `iq`. A decoder thread finds the bursts, measures the band's power each second and writes them to `decoded`. A reader follows both while they are written and merges them by time.

```sh
cargo run --release -p ww_tape_example --bin rf                     # a minute's size in each codec
cargo run --release -p ww_tape_example --bin rf -- write rf.wwt 100  # the minute at 1/100 of the rate, into a file
cargo run --release -p ww_tape_example --bin rf -- chains rf 100     # the same minute in two chains under rf/, followed
```

`tests/rf.rs` checks that a capture reads back sample for sample and pins the size of a second at the full rate. It also checks the two chains: every burst found, every sample kept, the merge in time order. `ww_tape_benchmarks` times it: `--bin rf_core` on one core, `--bin rf_chain` into a chain of files.

## The protocol analyser

The module `ethernet` records both directions of a 10 Gbps Ethernet link, as a protocol analyser sees them. Every frame is an event of the track `frames` with its bytes. A decoder finds each frame's IPv4 header and its TCP or UDP header. It writes them to the tracks `ipv4` and `l4`, each item related to its frame. The traffic is synthetic: the simple IMIX over a set of flows, with random payloads, made from the frame's number.

A port spreads its frames over receive queues, as a NIC does. Each queue has a capture thread and a decoder thread, since a track has one producer. Both ports go through one mux, or a mux per port into a chain each.

```sh
cargo run --release -p ww_tape_example --bin ethernet        # what each track takes for 10 ms of the link
cargo run --release -p ww_tape_example --bin ethernet -- 100  # the same for 100 ms
```

`tests/ethernet.rs` reads every frame and header back and pins the sizes the book quotes. `ww_tape_benchmarks` times it: `--bin ethernet`, or `just bench-ethernet`.

## The wideband receiver

The module `wideband` records a receiver of 1 billion complex samples a second, split into channels of equal width. Each channel is the RF capture's signal at its share of the rate, a series track with summaries and a producer thread of its own. Channel `k` goes to mux `k % m`, so several muxes write several chains from one process. `overview` reads the nodes of one summary level from every piece and no samples.

`tests/wideband.rs` reads every sample of every channel back, finds the bursts from the overview alone and pins the sizes the book quotes. `ww_tape_benchmarks` times it: `--bin wideband`, or `just bench-wideband`.
