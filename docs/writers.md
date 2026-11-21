---
level: basic
---

# For writers

This page is for whoever writes or changes a chapter of this book. It says how a chapter reads, how it marks terms
and code, and how its code and figures stay true. A reader of the book can skip it.

## How a chapter reads

A chapter answers one question, and its title names it: "When the power fails", not "Resuming". It opens with the
running example's situation and the question, in two or three sentences. The reader then knows why the chapter
matters before it explains anything.

Why and what come before how. Each new idea gets a concrete case from the running example. Where the idea has a
mechanism, a figure follows right after the sentence that needs it.

A paragraph carries one idea and its reason, in two to five sentences. A list holds parallel items only, and a table
holds a comparison only. Reasoning stays in prose.

Reference detail goes to the [format reference](format.md) and the chapter links it by anchor. Caps, constants, field
lists and the edge cases of a refusal are such detail. A chapter names a constant only when the reader picks its
value.

A chapter ends on its last fact, or on one sentence that leads to the next chapter. It has no list of further pages.

## The running example

One recording runs through the whole book: a motor controller board, the drive, on an endurance bench. The host
records it over USB into a chain of pieces. [What a tape holds](index.md#what-a-tape-holds) lists its five tracks,
and a chapter links that table instead of repeating it.

The board counts time in microseconds since boot and the host in nanoseconds. A clock map joins the two. At minute 37
the host loses power while it writes piece 2, and at minute 52 the board faults on overcurrent.

A number in the story comes from running code, never from a hand calculation. The bytes a crash loses and the bytes
per frame are such numbers.

The crate `ww_tape_example` records the drive's hour, and `cargo run -p ww_tape_example` prints the story's numbers.
Figures draw from a scaled run, which has a fraction of the data and of every byte policy. A doctest calls the same
code:

```rust
// The drive's hour at 1/1000 of its data, as the figures draw it.
let drive = ww_tape_example::Drive::scaled(1000).expect("1000 divides the rates");
let story = drive.story().expect("the run records");
// The crash at minute 37 resumes to the bytes of the run without it.
assert!(story.same_bytes);
```

## Reading levels

Each page carries a reading level in its front matter, and a badge above its title shows it. [Reading
levels](levels.md) says what the three levels mean.

```yaml
---
level: basic
---
```

The value is `basic`, `advanced` or `nomicon`. Each part of the navigation lists its basic chapters first.

## Terms and code links

A term the book defines has one entry in `docs/glossary.toml`: its name, its other forms, one sentence and the
chapter that explains it. The page [Glossary](glossary.md) lists every entry. In prose, `[[piece]]` marks the term,
and `[[the last file|piece]]` shows other words for it. Mark a term at its first use in each section, not at every
use.

A name in square brackets and backticks links code. ``[`TapeWriter::resume_on`]`` links an item, ``[`crashes.rs`]``
a file, and ``[`crashes.rs:resume`]`` a marked section of a file. The reader sees the code in a card and opens its
lines on GitHub.

`just book-refs` checks every mark and link, and every build runs it. An unknown term, or a name that no item matches,
fails the build. A renamed item thus fails the book in the same change that renames it.

The book uses one name for each thing. Where two words could name one thing, the glossary entry decides, and the other
word goes into its forms.

## Abbreviations

Prose writes an abbreviation alone and never spells it out: "a PC", never `a personal computer (PC)`. The
`abbreviation-first-use` lint in `spirit_level.toml` holds two lists, and every abbreviation goes into one of them.

| List | Holds | In the book |
|---|---|---|
| `known` | ones every reader knows, and names in capitals: PC, USB, AMD, EPYC | plain |
| `long-forms` | every other, with its long form: CAN, SD, CRC, IQ, MCU | its long form on hover |

A new abbreviation that a reader may not know goes into `long-forms`. `just book-refs` writes the table, and the
build marks each use so that the reader sees the long form under the pointer. The lint fails an abbreviation in
neither list, and prose that spells out one of either list.

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
