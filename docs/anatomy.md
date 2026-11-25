---
level: basic
---

# Anatomy of a piece

A [[piece]] is a file of a recording that reads alone, with its own [[definitions]], its own index and a fixed end. A
[[reader]] that opens one must find its way in it: where the data starts, where the index lies, and whether the file
was closed.

The parts of a piece answer those questions: a head, [[checkpoints]] while it grows, and an end. The drive's hour,
for example, lies in four pieces, one per quarter of an hour, and the numbers below come from the first.

## From piece head to tail

A [[piece]] holds its [[blocks]] in a fixed order, so a reader knows where to look for each part:

1. A [[piece head]] gives the piece's ids, the [[owner id]] and the hash of the piece before it.
2. The [[writer]] puts the [[producer]], the [[layout]] and the [[definitions]] the first data needs.
3. The data follows, with the [[puts]] each block needs just before it, and every 4 MiB an index block called a
   [[checkpoint]].
4. Three blocks end the piece: a [[final index]], a [[close]] record and a [[tail]].

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/piece.svg"
<figcaption>A closed piece, each block to scale on rows of 1 KiB: records blocks and events chunks with the puts they
need, two checkpoints, the final index, the close with its padding, and the 164-byte tail ending on 12 KiB. Drawn by
<code>ww_tape_figures</code> from a small piece that <code>PieceChain</code> wrote.</figcaption>
</figure>

The drive's first piece holds the first 15 minutes of the run in about 29 MB. The [[series chunks]] of `phase_a`
take most of it, and the blocks a [[reader]] uses to find its way take about 2.8 % of it. `cargo run -p
example_drive` prints this table and that share:

--8<-- "docs/generated/anatomy_parts.txt"

[Sizes and speeds](sizes.md#the-drives-hour) gives the disk and the writing cost of a whole run.

Every offset in a piece counts from the piece's first byte. An index therefore stays valid wherever the piece sits:
alone in a file, or among other pieces.

## Checkpoints while the piece grows

While the [[writer]] writes a [[piece]], nothing marks its end yet. The writer therefore places a [[checkpoint]] about
every 4 MiB. Each checkpoint lists every [[block]] placed since the previous checkpoint, and points back at that
checkpoint.

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/checkpoints.svg"
<figcaption>The drive's first piece, closed. Each checkpoint points back to the one before, so the newest one leads
to all. At the end, the close names the final index and the newest checkpoint, and the tail names the close. Drawn by
<code>ww_tape_figures</code> from the drive's recording at 1/1000 of its data, with a checkpoint every 3 KiB
instead of every 4 MiB.</figcaption>
</figure>

A [[reader]] of an unclosed piece finds the newest checkpoint from the end of the bytes, and follows the checkpoints
back. It then knows every block before the newest checkpoint, and scans only the stretch after it.
[When the power fails](crashes.md) shows the scan on a piece that a crash interrupted.

## The end of a closed piece

The [[final index]] lists every [[block]] of the [[piece]], sorted by track, with a directory of [[tracks]] at its
start. A [[reader]] of one track out of thousands reads that directory and that track's list, and nothing else. The
drive's first final index takes about 443 KB of its piece's 29 MB.

The [[close record]] states how the piece ended. Besides the reason, it holds three things:

- the piece's [[content hash]], a cryptographic BLAKE3 hash over every byte before the close;
- each track's totals: its [[items]], its [[ticks]] and its running hash;
- the offsets of the final index and of the newest [[checkpoint]].

The [[tail]] is the piece's last 164 bytes. It gives the piece's length and the place of the close. A reader that starts
at the end of the bytes thus finds everything from there. The close pads the piece so that the tail ends on a multiple
of 4 KiB. A file system that clones by block can then join pieces into one file without copying a byte.

A file that a [[writer]] started without a [[piece head]], a plain stream of blocks, is still a valid [[tape]]. It reads
as one piece with no ids.

## Reading by position

A [`TapeReader`] reads a [[piece]] through any [[byte source]], a source of bytes that reads at an offset. Three
such sources are common:

- a slice in memory;
- a file opened as a [`FileSource`];
- a resource fetched over HTTP by range requests.

The [[reader]] needs no file system and no threads, so it also runs in a browser.

The reader opens a [[closed]] piece with a few small reads: the [[tail]], the close and the [[final index]]'s directory.
After that, it reads only the [[blocks]] a query needs. This test records the drive at 1/1000 of its data and opens its
first piece:

```rust
--8<-- "ww_tape_tests/tests/it/book_pages.rs:closed_piece"
```

## Same input, same bytes

A [[writer]] given the same input writes the same bytes, for these reasons:

- [[Blocks]] hold no wall time.
- [[Definitions]] go in the order the application makes them.
- [[Puts]] and copies follow from the bytes before them.
- The writer ends blocks by size rather than by time.
- Every [[codec]] is integer-only, or uses arithmetic that every platform rounds the same way.

A [[producer]] gives up this property for the chunks it completes on a timer, and for the [[pieces]] it [[cuts]] by age.
The lz4 codec is one more exception. An lz4-coded [[payload]] [[column]] is byte-identical only for one version of the
lz4 library, and only on targets of one pointer width and byte order. The exception does not affect decoding.

Deterministic output makes files comparable by hash. A test can pin a file's exact bytes, and two machines that recorded
the same feed can confirm that they hold the same data. The drive's recording relies on it: a [[resume]] after the crash
at minute 37 gives the bytes of the run without the crash.
