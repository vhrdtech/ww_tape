---
level: advanced
---

# Gaps and duplicates on a raw card

A [[raw medium]] has blank and bad stretches inside a recording, and a writer that retried a write may leave a block
twice. A [[reader]] must read past both without taking the end of the data for a gap, or a gap for the end. This page
follows [Where the data ends on a raw card](raw_media.md). It uses the same card: the drive's run over an earlier hour,
torn at minute 37.

## Blank space and gaps

Blank bytes are all 0x00 or all 0xFF: what an erased or preallocated medium reads, depending on its kind. No block
starts with either value, so blank space that runs to the end of the medium is the end of the data. A blank or damaged
stretch in the middle is not always the end. A writer leaves one in two cases:

- a writer on raw NAND flash whose page program failed moves on to the next erase block;
- a writer that pads a NAND page at a sync leaves 0xFF behind.

The [[layout]] says how far a writer may skip and where it goes on, at a multiple of its resume alignment:

| Medium | Longest gap (`gap_max`) | Goes on at a multiple of (`resume_align`) |
|---|---|---|
| a file ([`Layout::UNALIGNED`]) | 0: the first bad stretch is the end | 1 byte |
| an SD card or eMMC device ([`Layout::on_sd_card`]) | 4 MiB | 512 bytes, a sector |
| raw NAND ([`Layout::on_raw_nand`]) | two erase blocks | one page |

A reader learns these rules from the layout definition after the piece head. The writer copies that definition
about 1 MiB on and puts it again past [[near]], as it does a track's definition. A medium layer that knows the medium
can give them from the start ([`OpenOptions::layout`]). A walk that starts past lost first sectors needs them there,
since until the copy nothing in the piece says them.

Past a blank or bad stretch, a reader looks for the data to go on:

1. It reads one block header at the first non-blank byte.
2. It reads one more at each resume place within `gap_max`.
3. At a good block of the current session, it reports the stretch as a [[gap]] with its length ([`FileError::Gap`]) and
   reads on from that block.
4. When no such block follows, the stretch is the end of the data.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/gap_probe.svg"
<figcaption>A reader on a raw medium past a bad stretch. It reads a block header at each resume place, up to the
layout's longest gap. The session's next block at one of them makes the stretch a gap; with none, the data ends at the
stretch.</figcaption>
</figure>

On the drive's card the torn block is such a stretch. Within 4 MiB after it, the reader finds only the earlier run's
blocks, which are stale. Today's data therefore ends at the torn block, and the reader reads nothing further.

The format bounds what a gap costs a reader. It reads a header per resume place and checks only a few blocks whole. It
reads no further than blank space longer than `gap_max`, so an erased 64 GB card costs one gap's worth of reads. The
[format reference](format.md#blank-space-and-gaps) lists the bounds.

## Duplicates

A writer that retried a write may leave the same block twice. The same session and sequence number twice is a
[[duplicate]], not a new block. What a reader does with the second copy depends on its bytes and on the [[recovery
mode]] ([Reading past damage](reading_past_damage.md)):

| Second copy | [`Recovery::Prefix`] | [`Recovery::Resync`] |
|---|---|---|
| the same bytes | drops it | drops it |
| other bytes | takes it in place of the first and reports [`FileError::Replaced`] | reports a conflict and takes neither for the other |

Resync cannot tell a copy from a rewrite in damaged bytes, so it hands out only the first.

[Lessons from tape and flash](lessons.md) names the tape drives and flash file systems that solved these problems first,
and what the format takes from each.
