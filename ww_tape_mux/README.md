# ww_tape_mux

A writing thread for [ww_tape](../README.md) chains. A `TapeMux` owns a `PieceChain` and its definitions on a thread of its own; producers on any thread write through handles:

| Handle | Sends |
|---|---|
| `ChunkSender` | chunks of one events or series track, encoded by the caller |
| `RecordsHandle` | records blocks |
| `RelayHandle` | another producer's blocks |
| `StateHandle` | state items, on change |

Units go through one queue bounded by bytes. A handle waits for room (the default), fails with the unit back, or drops it and counts. The writing thread places units in the order they came and does the chores: puts, cuts, sync and checkpoints by time, the close.

`ww_tape_fs::mux` opens one on a directory of piece files.
