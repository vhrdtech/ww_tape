# ww_tape_mux

A writing thread for [ww_tape](../README.md) chains. A `TapeMux` owns a `PieceChain` and its definitions on a thread of its own; producers on any thread write through handles:

| Handle | Sends |
|---|---|
| `EventsHandle` | items of one events track (instants, spans, flags, relation values), encoded into chunks on the producer's thread |
| `SeriesHandle` | samples of one series track on a grid, encoded the same way |
| `ChunkSender` | chunks of one events or series track, encoded by the caller |
| `RecordsHandle` | records blocks |
| `RelayHandle` | another producer's blocks |
| `StateHandle` | state items, on change |

Units go through one queue bounded by bytes. A handle waits for room (the default), fails with the unit back, or drops it and counts. The writing thread places units in the order they came and does the chores: puts, cuts, sync and checkpoints by time, the close.

The typed handles hash each chunk's payload and compute the leaves of the track's summaries on the producer's thread. The writing thread builds the levels above, one tree per track and piece. A quiet handle's open chunk is completed by the writing thread once its first item waited the handle's latency.

`ww_tape_fs::mux` opens one on a directory of piece files.
