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

- **Any typed data over time.** Log lines, states, messages on a bus and sampled signals share one tape. The book's
  example records log lines 3 times a second and a phase current 25 000 times a second. The same format takes a
  radio capture of 20 million samples a second.
- **Recordings of gigabytes.** A long recording is a chain of pieces, each a file that reads alone. A minute of the
  radio capture is one piece of 3.7 GB.
- **Small reads.** Opening the example's 115 MB recording reads about 14 KB. Finding one track's blocks around a
  given second then reads about 197 KB, nearly all of it that track's index.
- **Damage stays local.** A crash, a torn write or a bad sector costs the bytes it touched. A reader says what it
  lost and reads the rest.
- **Readable years later.** A tape carries the definitions of its tracks and types. A reader on another computer
  needs no device and no side file.
- **One format, three uses.** The same blocks stream live to a viewer, fill files on a computer or lie on a
  raw card.
- **A reader for the browser.** The reader builds without the standard library, for WebAssembly in a browser or a
  tool.
- **A lighter sibling for firmware.** On a small device, [logring](related_books.md#logring-a-log-store-on-the-device)
  keeps the log with no heap and little RAM. ww_tape runs on a host.

## How to read this book

<span id="what-a-tape-holds"></span><span id="one-format-three-uses"></span><span id="what-the-format-is-for"></span>
<span id="state-tracks-and-near-use"></span><span id="pieces-and-chains"></span><span id="the-crates"></span>

Start where your question is:

- [What a tape holds](tape.md): tracks and items, the three uses, the goals, pieces and chains, and the crates.
- [A first tape](first_tape.md): a program that writes a tape file and reads it back.
- [Reading a tape](reading.md): find one second in a long recording with a few small reads.
- [When bytes go bad](damage.md): what a flipped bit costs, and how a reader reads past it.
- [WireWeaver and logring](related_books.md): the framework tapes belong to, and the store for firmware.
- [Format reference](format.md): every block byte by byte, for a reader in another language.

Most examples come from one recording, a motor controller's hour on a test bench, and the crate `ww_tape_example`
writes it. Every number in an example comes from running that code. A badge above each title gives its [reading
level](levels.md): read the basic chapters first and stop when you have what you need.
