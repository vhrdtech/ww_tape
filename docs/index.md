---
level: basic
---

<!-- The logo is inlined (assets/logo, `just logo`) so the book needs no image files; one of the two shows per
scheme, by the rules of stylesheets/hero.css. -->
<div class="ww-tape-hero ww-tape-hero--light" markdown="0" role="img" aria-label="ww_tape">
--8<-- "assets/logo/ww_tape-light.svg"
</div>
<div class="ww-tape-hero ww-tape-hero--dark" markdown="0" role="img" aria-label="ww_tape">
--8<-- "assets/logo/ww_tape-dark.svg"
</div>

# Tapes

ww_tape records typed values over time as a [[tape]]. The source may be a device under test, a protocol analyser, a
radio front end or a simulation. A tape keeps each kind of value on a [[track]] of its own. Its blocks check themselves,
so a reader can trust any part of it without the rest.

A tape serves recordings that are large, long and read in parts:

- **Any typed data over time.** Log lines, states, messages on a bus and sampled signals share one tape. The book's
  example records log lines 3 times a second and a phase current 25 000 times a second. The same format takes a
  radio capture of 20 million samples a second. It also captures [a 10 Gbps link](protocol_analyser.md) and [a band
  of 250 million samples a second](wideband.md) into memory and keeps what matters.
- **Recordings of gigabytes.** A long recording is a chain of pieces, each a file that reads alone. A minute of the
  radio capture is one piece of 3.7 GB.
- **Small reads.** Opening the example's 116 MB recording reads about 17 KB. Finding one track's blocks around a
  given second then reads about 210 KB, nearly all of it that track's index.

It also keeps what it holds readable after damage and over years:

- **Damage stays local.** A crash, a torn write or a bad sector costs the bytes it touched. A reader says what it
  lost and reads the rest.
- **Readable years later.** A tape carries the definitions of its tracks and types. A reader on another computer
  needs no device and no side file.

The same blocks serve every place a recording goes:

| Where | What ww_tape does there |
|---|---|
| A live feed | hands each block to a viewer as soon as it is made |
| Files on a computer | writes a chain of pieces, each a file that reads alone |
| A raw card | writes blocks without a file system and finds them again after a power cut |
| A browser | reads tapes with a reader built without the standard library, for WebAssembly |
| A small device | leaves the log to its lighter sibling [logring](related_books.md#logring-a-log-store-on-the-device), which needs no heap |

## How to read this book

<span id="what-a-tape-holds"></span><span id="one-format-three-uses"></span><span id="what-the-format-is-for"></span>
<span id="state-tracks-and-near-use"></span><span id="pieces-and-chains"></span><span id="the-crates"></span>

The book runs in four parts, each one [reading level](levels.md) deeper than the one before. Read them in order
and stop when you have what you need:

| Part | What you can do after it |
|---|---|
| Introduction | Know what a tape holds and write [a first tape](first_tape.md). |
| [Basic](basic/index.md) | Record events, states and signals into a chain of files, read one second back, and resume after a crash. |
| [Advanced](advanced/index.md) | Pick clocks, codecs and relays, follow a growing recording, store fields and images, and check copies. |
| [Nomicon](nomicon/index.md) | Know why a piece holds what it holds, to write a reader in another language. |

The [Reference](reference/index.md) holds the glossary, the measured sizes and speeds, and the
[format reference](format.md) byte by byte. The Articles follow two cases from start to end: [a 10 Gbps
link](protocol_analyser.md) and [a fast radio band](wideband.md).

Most examples of the Basic part come from one recording, a motor controller's hour on a test bench, written by the
crate `example_drive`. Every number in an example comes from running the code that writes it.
