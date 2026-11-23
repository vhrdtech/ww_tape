# example_drive

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
cargo run --release -p example_drive          # the story's numbers at full scale
cargo run --release -p example_drive -- 1000  # the same at 1/1000
```

The archived last piece then takes damage three ways: a flipped bit, its first 4 KiB lost, its last 4 KiB lost. The run is also written onto a raw SD card that still holds an earlier run behind it.

`tests/story.rs` checks three things. The full hour is the same every run, and its crash lands where the story says. A scaled run resumes from a crash at any byte.

`tests/writing.rs` pins the numbers of the book's writing chapters in `tests/writing.out`. They are the calls and the blocks they make, the hour in an aligned file and the columns per codec. They also cover the crash per sync policy, the cut, the chain without its first piece, and the relay.

`tests/damage.rs` checks what each damage costs and that a reader of the raw card never takes the earlier run for data.

The book's other cases have crates of their own: `example_rf`, `example_ethernet` and `example_wideband`, with what they share in `example_capture`.
