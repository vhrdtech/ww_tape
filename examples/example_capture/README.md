# example_capture

What the ww_tape book's cases share.

| Item | What |
|---|---|
| `window` | the memory window: a chain of pieces kept in memory, written by a mux, the oldest pieces dropped; a triage thread reads each piece as it closes and copies the ones worth keeping to a store of their own |
| `rf` | the RF signal and its track `iq`, recorded by `example_rf` and by each channel of `example_wideband` |
| `chain_options` | a chain whose bytes depend only on what is written |
| `pieces` | the bytes of the closed pieces of a chain in memory |
| `hash`, `about` | the fixed numbers the synthetic signals are made from; a size rounded for prose |

A piece in the window holds the same tracks, blocks and summaries a piece in a file holds. When the analyser falls behind and the window is full, the oldest pieces leave unread, and the triage counts them.
