---
level: basic
---

# Deferred blocks and aligned files

Some blocks need not land at once, and some reads go faster when data starts on a page of the disk. Where does the
[[writer]] place a block that may wait, and what does a file gain when every payload starts on a 4 KiB boundary? The
examples compare the drive's hour written both ways with a file of its phase current alone.

## Deferred blocks

A [[summary]] block of a series is the usual block that may wait. A [`SeriesSummaries`] or an [`EventTree`] from
`ww_tape_summary` fills one as data comes. The mux, or an application that drives the writer itself, hands such a
block to [`TapeWriter::defer_block`], and the writer chooses where it lands. That makes it a [[deferred block]].

The writer puts the block's definitions at once, and the file's layout decides where the block itself lands:

| Layout | A chunk's payload starts | A deferred block lands |
|---|---|---|
| unaligned, the default | right after the chunk's head | at once |
| [`Layout::ALIGNED`] | on the next 4 KiB boundary, after zeros in the chunk's head | in such zeros, within 1 MiB of writes |

## Aligned files

Disks, the page cache and direct input and output all work in blocks of 4 KiB, or in sectors that divide that size.
A 4 KiB boundary is an offset in the file that 4096 divides. An **aligned** file starts every chunk's payload, the
column bytes after its head, on such a boundary. Its samples then begin at the start of a page of the page cache and
of a sector on the disk.

A reader gains three things from that:

- **Memory maps.** A reader maps the file and uses a chunk's samples in place, as a slice that starts on a page. A
  vector kernel or a copy engine takes it without moving a byte.
- **Direct input and output.** Reads that bypass the page cache, such as `O_DIRECT` on Linux, need offsets and lengths
  in whole blocks. A payload on a boundary reads that way with no bytes before it.
- **Raw media.** On a card or a disk without a file system, a payload on a boundary covers whole sectors. Reading one
  chunk's samples then reads no sector of the block before it.

Choose it with the layout [`Layout::ALIGNED`], or call [`TapeWriter::set_alignment`] before the first block.

## Deferred blocks fill the gaps

The zeros before an aligned payload are a gap that the writer fills with deferred blocks while they fit. A deferred
block waits at most 1 MiB of writes for a gap that fits it.

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/deferred.svg"
<figcaption>One second of samples in three files, each row from the end of one series chunk to the end of the next.
Not aligned, the payload starts wherever the head ends. Aligned, zeros in the chunk's head move the payload to the
next 4 KiB boundary. In a file of <code>phase_a</code> alone, the waiting summary fills part of that gap. In the
drive's file, the next second's put of the mode places the summary first, so the gap holds zeros only. Drawn by
<code>ww_tape_figures</code> from the drive's first minute at full rate.</figcaption>
</figure>

Every put and every block handed over places the waiting deferred blocks first, even a put that writes nothing. In the
drive's file each second starts with a put of the mode and a records block, before the next series chunk. The summary
blocks therefore land before the gap opens, and none of them fills one:

| File | Unaligned | Aligned | More | Summary blocks in a gap |
|---|---:|---:|---:|---:|
| the drive's hour | 116 MB | 129 MB | 12 % | 0 of 2948 |
| `phase_a` alone | 105 MB | 112 MB | 7.3 % | 2724 of 2933 |

Alignment therefore suits a file that holds mostly series and is read by memory maps or direct input and output. It
is optional: where space matters more than such reads, leave the file unaligned. The drive's recorder keeps its mixed
file unaligned.
