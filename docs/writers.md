---
level: basic
---

# For writers

This page is for whoever writes or changes a chapter of this book. It says how a chapter reads, how it marks terms
and code, and how its code and figures stay true. A reader of the book can skip it.

## How a chapter reads

The book is about ww_tape: what a [[tape]] is, how it works and why it is built the way it is. Examples show a point
and then step aside. No chapter follows a plot, and no character, such as an engineer, carries it.

A chapter answers one question, and its title names it: "When the power fails", not "Resuming". It opens with its
subject and that question, in plain terms that a reader new to the book follows. The reader then knows why the
chapter matters before it explains anything.

Each part of a chapter has its own shape:

| Part | How it reads |
|---|---|
| Opening | the subject and its question; no road-map sentence such as "This chapter says what a tape is" |
| First example | a few sentences in, never the first line; its setup in one sentence, as in "The drive's fault lies in its last piece" |
| New idea | why and what before how, then a concrete case; never a scene, as in "An engineer opens the piece" |
| Mechanism | a figure right after the sentence that needs it |
| Paragraph | one idea and its reason, in two to five sentences |
| List or table | a list for parallel items, a table for a comparison; reasoning stays in prose |
| Reference detail | caps, constants, field lists and the edge cases of a refusal go to the [format reference](format.md), linked by anchor |
| End | the last fact, or one sentence that leads to the next chapter; no list of further pages |

A chapter names a constant only when the reader picks its value. Everything else about it is reference detail.

## The running example

Most chapters draw their examples from one recording, so that a reader learns its setup once. A motor controller
board, the drive, runs for an hour on an endurance bench. A host records it over USB into a [[chain]] of [[pieces]].
[What a tape holds](tape.md) introduces it and lists its five tracks, and a chapter links that table instead of
repeating it.

The recording has a few facts that examples rely on:

- The board counts time in microseconds since boot and the host in nanoseconds, and a [[clock map]] joins the two.
- At minute 37 the host loses power while it writes piece 2.
- At minute 52 the board faults on overcurrent.

A chapter uses such a fact where it shows a point, not as an event in a story. Where the drive has no fitting case, a
chapter takes the example crate that fits:

| Crate | Case | Used by |
|---|---|---|
| `example_drive` | the drive's hour, a crash at any byte, the resume | most chapters |
| `example_rf` | a radio capture into a piece or two chains | [series](series.md) |
| `example_ethernet` | a 10 Gbps link over receive queues and muxes | [the protocol analyser](protocol_analyser.md) |
| `example_wideband` | a 1 GS/s receiver in channels | [the wideband receiver](wideband.md) |

A number in an example comes from running code, never from a hand calculation. `cargo run -p example_drive` prints the
drive's numbers, such as the bytes a crash loses. Figures draw from a scaled run, which has a fraction of the data and
of every byte policy. A test calls the same code:

```rust
--8<-- "ww_tape_tests/tests/it/book_pages.rs:story"
```

## Numbers

A number is rounded where its precision doesn't matter and exact where it does. "A 115 MB recording" and "about
17 KB" tell the reader the size, and the last digits would only slow the reading. A rounded size uses decimal units
and keeps two significant digits at least: "115 MB", "17 KB", "2.1 KB".

A number stays exact where the reader uses its digits:

- byte layouts, offsets and field sizes, such as the tail's 164 bytes;
- caps and settings, such as 32 MiB of near or pieces of 256 MiB;
- a count the reader checks against code or output, such as item 3 120 000 or 28 reads;
- the size tables of [Sizes and speeds](sizes.md).

The test that pins a number keeps its exact value and checks the rounded text on the page. `example_capture::about`
rounds a size the way the pages write it. The test reads the page with its line breaks as spaces and asserts that it
holds the phrase, such as `format!("opening reads about {} in {} reads", about(bytes), reads)`. A changed recording
then fails the test until the page says the new number.

## Reading levels

Each page carries a reading level in its front matter, and a badge above its title shows it. [Reading
levels](levels.md) says what the three levels mean.

```yaml
---
level: basic
---
```

The value is `basic`, `advanced` or `nomicon`. The parts of the navigation form a reading ramp, and each part holds
pages of its own level only. A reader who clicks next thus never steps down. The shelves come after the ramp, and
their pages keep any level.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/book_levels.svg"
<figcaption>The navigation: the ramp rises from Introduction to Nomicon, and the Reference and Articles shelves follow
it. <code>vz check</code> holds the navigation to <code>book_checks.toml</code>.</figcaption>
</figure>

## Terms and code links

A term the book defines has one entry in `docs/glossary.toml`: its name, its other forms, one sentence and the
chapter that explains it. The page [Glossary](glossary.md) lists every entry. The book uses one name for each thing.
Where two words could name one thing, the glossary entry decides, and the other word goes into its forms.

Marks in prose turn into cards that the reader opens under the pointer:

| Mark | Links |
|---|---|
| `[[piece]]` | the term, at its first use in each section, not at every use |
| `[[the last file\|piece]]` | the term, shown in other words |
| ``[`TapeWriter::resume_on`]`` | an item of the code |
| ``[`crashes.rs`]`` | a file |
| ``[`crashes.rs:resume`]`` | a marked section of a file, with its lines on GitHub |

`just book-refs` checks every mark and link, and every build runs it. An unknown term, or a name that no item
matches, fails the build. A renamed item thus fails the book in the same change that renames it.

## Abbreviations

Prose writes an abbreviation alone and never spells it out: "a PC", never `a personal computer (PC)`. The
`abbreviation-first-use` lint in `spirit_level.toml` holds two lists, and every abbreviation goes into one of them.

| Place | Holds | In the book |
|---|---|---|
| `known` in `spirit_level.toml` | ones every reader knows, and names in capitals: PC, USB, AMD, EPYC | plain |
| `docs/abbreviations.toml` | every other, with its long form: CAN, SD, CRC, IQ, MCU | its long form on hover |

A new abbreviation that a reader may not know goes into `docs/abbreviations.toml`. The build reads the file and
marks each use so that the reader sees the long form under the pointer. The lint fails an abbreviation in neither
place, and prose that spells out one of either.

## Code on a page

Code on a page always comes from code that the tests compile and run. The same holds for a program's output and for
a figure that shows bytes. Each kind has its own source and its own check.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/book_sources.svg"
<figcaption>Where each kind of content comes from. Nothing on a page is typed by hand except prose and drawings of a
mechanism.</figcaption>
</figure>

A test marks a section with `// --8<-- [start:NAME]` and `// --8<-- [end:NAME]` lines, and the page includes that
section in a `rust` block:

````markdown
```rust
--8<-- "ww_tape_tests/tests/it/book_pages.rs:piece_align"
```
````

The block on the page then shows the lines between the marks:

```rust
--8<-- "ww_tape_tests/tests/it/book_pages.rs:piece_align"
```

A short example goes into `ww_tape_tests/tests/it/book_pages.rs` as a test of its own, with an assertion that checks
its result. A longer one is a section of an example crate or of the test that already runs it. The build fails when
the file or the marked section is gone, and `vz check` fails a `rust` block that includes nothing.

What a program prints comes from a test too. The test passes the output to `ww_tape_book_outputs::check`, which
fails when the file under `docs/generated/` holds something else. `just book-outputs` writes the files again, and the
page includes one in a `text` block, as [A first tape](first_tape.md) does.

## Figures

A figure stands right after the sentence that needs it. Its caption states what the reader should take from it.

| Kind | Made by | Lives in | Checked by |
|---|---|---|---|
| bytes or numbers | `ww_tape_figures`, from blocks that ww_tape writes (`just book-figures`) | `docs/plots` | a test that fails when a drawn figure differs from git |
| a mechanism | hand, as an SVG with the classes of `figures.css` | `docs/figures` | the strict build |

The caption of a drawn figure names the generator, and also what it shrank, such as checkpoints every 4 KiB instead of
every 4 MiB. Both kinds go into the page through a `--8<--` line inside a `figure` element, so they follow the light
and dark scheme.

## Checks

`just book-check` runs three steps, and `just test` runs the first one too:

1. `vz check`, the book kit's check, holds the navigation to `book_checks.toml`. A page may not step down a level,
   must carry a level and appears once. A bare `rust` block fails.
2. The build runs with warnings as errors, so a link to a missing page, anchor or figure fails.
3. The prose lints of `spirit_level.toml` run at deny: five sentences to a paragraph and 25 words to a sentence.

The reading lints hold a page to the shape of the table under [How a chapter reads](#how-a-chapter-reads). A run of
prose ends within 350 words at a list, figure, table or code block. A page has one such element per 120 words. A
page longer than 500 words has a figure, and one more for every 600 words after.

The format reference is exempt from the reading time of a guide page, since a reader looks things up there.
