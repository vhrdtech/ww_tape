---
level: basic
---

# Basic

The Basic part teaches what you need to record data on a [[tape]] and read it back. After it, you can put events, states
and sampled signals on tracks of their own. You can write them from several threads into a [[chain]] of files. You can
then find one second of a long recording with a few small reads, and resume the recording after a crash.

The part assumes that you know Rust and have read the [Introduction](../index.md). It assumes nothing about the
format: each chapter builds on the chapters before it, so read them in order. Every page in this part has the Basic
[reading level](../levels.md) badge.

## Chapters

| Chapter | What it teaches |
|---|---|
| [How a tape is built](../tracks.md) | Tracks and items, the blocks that check themselves, definitions and state, and the parts of one file. |
| [Writing](../writing.md) | The calls of a writer and the blocks it adds, writing from many threads, files and syncing, pieces, chains and their retention, and a crash. |
| [Reading](../reading.md) | Finding a time in a long recording, then events, series and the summaries a plot reads. |
| [Damage](../damage.md) | What a bad byte costs a reader, reading on past it, what a damaged piece still gives, and the checks a fast writer can leave to a later seal. |

## Examples

Most chapters use one recording: a drive motor controller's hour on a test bench, written by the crate
`example_drive`. It holds log lines, a mode, bus frames and a phase current, so it shows each kind of track. Every
number from it comes from running that crate.

When you finish this part, the [Advanced part](../advanced/index.md) covers the options behind the defaults.
