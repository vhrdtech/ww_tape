---
level: basic
---

# For writers

This page is for whoever writes or changes a chapter of this book. It says how a chapter reads, how it marks terms
and code, and how its code and figures stay true. A reader of the book can skip it.

## How a chapter reads

The book is about ww_tape: what a tape is, how it works and why it is built the way it is. Examples show a point and
then step aside. No chapter follows a plot, and no character, such as an engineer, carries it.

A chapter answers one question, and its title names it: "When the power fails", not "Resuming". It opens with its
subject and that question, in plain terms that a reader new to the book follows. The reader then knows why the
chapter matters before it explains anything.

An example arrives as the first concrete case, a few sentences in, and never as the first line. A chapter has no
road-map sentence such as "This chapter says what a tape is": the opening shows the subject instead of announcing it.

Why and what come before how. Each new idea gets a concrete case, usually from the running example. The case states
its setup in one sentence where it is used, as in "The drive's fault lies in its last piece". It never opens with a
scene, as in "An engineer opens the piece".

Where an idea has a mechanism, a figure follows right after the sentence that needs it.

A paragraph carries one idea and its reason, in two to five sentences. A list holds parallel items only, and a table
holds a comparison only. Reasoning stays in prose.

Reference detail goes to the [format reference](format.md) and the chapter links it by anchor. Caps, constants, field
lists and the edge cases of a refusal are such detail. A chapter names a constant only when the reader picks its
value.

A chapter ends on its last fact, or on one sentence that leads to the next chapter. It has no list of further pages.

## The running example

The book draws most of its examples from one recording, so that a reader learns its setup once. A motor controller
board, the drive, runs for an hour on an endurance bench. A host records it over USB into a chain of pieces.
[What a tape holds](tape.md) introduces it and lists its five tracks, and a chapter links that
table instead of repeating it. Other examples, such as a radio capture, join where the drive has no fitting case.

The recording has a few facts that examples rely on. The board counts time in microseconds since boot and the host in
nanoseconds, and a clock map joins the two. At minute 37 the host loses power while it writes piece 2, and at minute
52 the board faults on overcurrent. A chapter uses such a fact where it shows a point, not as an event in a story.

A number in an example comes from running code, never from a hand calculation. The bytes a crash loses and the bytes
per frame are such numbers.

The crate `example_drive` records the drive's hour, and `cargo run -p example_drive` prints the example's
numbers.
Figures draw from a scaled run, which has a fraction of the data and of every byte policy. A doctest calls the same
code:

```rust
// The drive's hour at 1/1000 of its data, as the figures draw it.
let drive = example_drive::Drive::scaled(1000).expect("1000 divides the rates");
let story = drive.story().expect("the run records");
// The crash at minute 37 resumes to the bytes of the run without it.
assert!(story.same_bytes);
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

The value is `basic`, `advanced` or `nomicon`. The parts Basic, Advanced and Nomicon each hold pages of their own
level only, so a reader who clicks next never steps down. The Reference and Articles shelves come after them, and
their pages keep any level. `vz check` holds the navigation to `book_checks.toml` and fails a step down.

## Terms and code links

A term the book defines has one entry in `docs/glossary.toml`: its name, its other forms, one sentence and the
chapter that explains it. The page [Glossary](glossary.md) lists every entry. In prose, `[[piece]]` marks the term,
and `[[the last file|piece]]` shows other words for it. Mark a term at its first use in each section, not at every
use.

A name in square brackets and backticks links code. ``[`TapeWriter::resume_on`]`` links an item, ``[`crashes.rs`]``
a file, and ``[`crashes.rs:resume`]`` a marked section of a file. The reader sees the code in a card and opens its
lines on GitHub.

`just book-refs` checks every mark and link, and every build runs it. An unknown term, or a name that no item
matches, fails the build. A renamed item thus fails the book in the same change that renames it.

The book uses one name for each thing. Where two words could name one thing, the glossary entry decides, and the other
word goes into its forms.

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

Code on a page always comes from code that the tests compile and run. Two forms carry it.

A short example is a `rust` block, which runs as a doctest. It shows every line it runs and checks its result with
an assertion. For example, this block runs with `cargo test --doc`:

```rust
// A piece ends on a multiple of 4 KiB.
assert_eq!(ww_tape::PIECE_ALIGN, 4096);
```

A doctest runs only on a page listed in `ww_tape_tests/src/book.rs`. List the page there when you add its first
`rust` block.

Longer code lives in a test and the page includes a marked section of it. The test marks the section with
`// --8<-- [start:NAME]` and `// --8<-- [end:NAME]` lines. The page shows it in an `rs` block, which highlights as
Rust but does not run as a doctest:

````markdown
```rs
--8<-- "ww_tape_tests/tests/it/crashes.rs:resume"
```
````

The build fails when the file or the marked section is gone.

## Figures

A figure stands right after the sentence that needs it. Its caption states what the reader should take from it.

A figure that shows bytes or numbers is generated. `ww_tape_figures` draws it from blocks that ww_tape writes, and
`just book-figures` writes it into `docs/plots`. A test fails when a drawn figure differs from the file in git. The
caption names the generator, and also what it shrank, such as checkpoints every 4 KiB instead of every 4 MiB.

A drawing by hand is an SVG file in `docs/figures`. Both kinds go into the page through a `--8<--` line inside a
`figure` element, so they follow the light and dark scheme.

## Checks

`just book-check` builds the book with warnings as errors and then runs the prose lints of `spirit_level.toml`. The
lints hold a paragraph to five sentences and a sentence to 25 words. A stretch of prose ends within 400 words at a
heading, figure, table or code block.

The format reference is exempt from the reading time of a guide page, since a reader looks things up there.
