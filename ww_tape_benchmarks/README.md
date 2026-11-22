# ww_tape_benchmarks

Benchmarks of `ww_tape`, one binary each. The float sample codecs are measured in `ww_tape_codecs_benchmarks`.

Every binary prints Markdown tables, each with the load average before and after it, and writes them as JSON with
`--json FILE`. `--help` lists a binary's settings. `just bench-book` runs the benches of the book's "Sizes and speeds"
into `docs/bench/`:

| Binary | Recipe | What |
|---|---|---|
| `drive` | `just bench-drive` | the book's running example at full scale: bytes by block kind, bytes per item, time to record |
| `event_times` | `just bench-event-times` | event start ticks against raw ticks, varints and Elias-Fano |
| `summaries` | `just bench-summaries` | series summaries, and the kernels against one statistic at a time |
| `chunks` | `just bench-chunks` | the writer's cost per series chunk, into a memory sink and a chain in one memory tape, and a track's timeline |
| `tracks` | `just bench-tracks` | tracks added one by one, puts and copies |
| `starts` | `just bench-starts` | a starts column read intact and salvaged |
| `relations` | `just bench-relations` | track relation columns |
| `mem_reader` | `just bench-mem-reader` | a reader's wait on a memory piece being written |
| `memory` | `just bench-memory` | the heap a writer holds over one piece, by tracks and block kind |
| `rf_core` | `just bench-rf-core` | the RF capture's writer and reader on one core, per codec |
| `rf_chain` | `just bench-rf-chain` | the RF capture into a chain of piece files at full speed, then read back |
| `carried` | `just bench-carried` | the definitions a piece carries for the tracks of its head, and the most a cut carries |
| `ethernet` | `just bench-ethernet` | a protocol analyser on both directions of a 10 Gbps link: receive queues a port, one mux or two, against the link |
| `wideband` | `just bench-wideband` | a wideband receiver at 1 GS/s in channels over one or several muxes, against its rate; one channel's overview read from its summaries |
| `handles` | `just bench-handles` | events and series handles on a `TapeMux` against one thread writing a chain directly, by tracks |

The `summaries` binary measures series summaries in MB/s of sample bytes per element type. It measures three paths:
the `RAW` codec alone, `SeriesSummaries::push` with every statistic, and both.

It prints the CPU's vector extensions
and the load average with them.

```sh
just bench-summaries
just bench-summaries --frames 1024 --mib 64
```

The `tracks` binary adds tracks one by one, as a recorder that learns of a new source does. For each track it:

1. holds the track in its `Definitions` (`Definitions::define`);
2. writes one records block on it, and the writer puts the definition before that block;
3. closes the piece and reads its definitions back.

It prints the time to add the first 1 000, 5 000 and all tracks.

Then it writes a piece of `--piece-mib` (256 by default) with every track defined. Records blocks of `--block-kib` (4)
go on the tracks in use in turn.

It prints the bytes the writer put and copied: with every track in use, and with 1 % in use. Last, it prints the bytes
a writer hashes and copies per block when nothing needs writing.

```sh
just bench-tracks
just bench-tracks --tracks 100000
```

The `rf_core` binary times the writer and the reader on the RF capture of `ww_tape_example::rf`, on one core. The
samples of each chunk are made before it and not timed. It prints, per codec, what the writer spends on a chunk: the
codec, `TapeWriter::write_chunk` and the summaries. Then it reads a capture in memory, every chunk checked and decoded.

The `rf_chain` binary records the capture at full speed into a chain of piece files, a piece a minute, until the chain
holds `--gib` GiB. Worker threads make and code the chunks, and one thread writes them. It then opens every piece and
reads one second, counting the bytes and reads.

```sh
just bench-rf-core
just bench-rf-chain --dir /data/rf --gib 100 --remove
```

The `ethernet` binary records the protocol analyser of `ww_tape_example::ethernet` at full speed. It takes both
directions of a 10 Gbps link at its line rate, with a capture and a decoder thread per receive queue. A run per number
of queues and of muxes writes into a store that keeps no bytes. With `--dir`, the most queues also write chains of
piece files there. It prints the frames a second and the share of the link the run kept up with.

```sh
just bench-ethernet --dir target/bench-ethernet
just bench-ethernet --ms 500 --queues 1,2,4,8 --muxes 1,2
```

The `wideband` binary records the receiver of `ww_tape_example::wideband` at full speed. It splits 1 GS/s of IQ into
channels, each a producer thread, over one or several muxes. Each thread pushes chunks made before the run, so the run
times the recorder and not the signal. A run per number of channels, muxes and codec writes into a store that keeps no
bytes. With `--dir`, the most channels and muxes also write chains of piece files there.

A reader then draws channel 0's overview from its summary nodes, counting the bytes it reads.

```sh
just bench-wideband --dir target/bench-wideband
just bench-wideband --ms 500 --channels 8,16 --muxes 1,2,4,8
```
