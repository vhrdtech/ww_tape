---
level: advanced
---

# Sessions and raw media

A **medium** is what holds the bytes of a tape: a file, or a raw card or flash chip written without a file system. In
a file, the end of the data is the end of the file, but on a raw medium it is not.

This page explains how a reader finds the end of the data on a raw medium. It closes with what tape drives and flash
memory taught the format.

The page assumes [Damage and recovery](recovery.md) and [Reading a damaged file](damaged_files.md).

## Sessions and stale data

A Secure Digital (SD) card or raw NOT-AND (NAND) flash may hold an older, longer recording behind the current one. That
recording's blocks are as valid as the new ones.

### Session tags

A **session** is one writer's unbroken append, such as a producer start or a device boot. Every block of a session
carries the same 32-bit **session tag** in its header, and its sequence numbers count from 0.

A writer gets the tag in one of three ways:

- it picks the tag at random;
- it derives the tag from a seed;
- it derives the tag from the piece id, when the same input must give the same bytes.

A tag is never 0 nor FFFFFFFFh, so an erased header never names a session.

### Where a new session may start

A reader keeps to one session at a time. A good block of another session is **stale data**, unless a new session may
start at that block.

Where a new session may start depends on the medium, a `Medium` value the reader is opened with:

- In a file (`Medium::File`), a new session may start at a records block that starts with a definition. There another
  producer appended its blocks.
- In a file, a new session may also start at any piece head. There files were concatenated.
- On a raw medium (`Medium::Raw`), a new session starts only at a piece head that names the session just ended as its
  previous session.
- Stale data from an earlier recording names some other session. A reader on a raw medium never takes it, even when it
  starts with a piece head of its own.

A reader judges a block by its session before its format version. Stale data written by an older firmware in another
format is therefore the end of the data, like any other stale data.

Such stale data is never read as a piece of that older format.

A reader therefore reads a raw medium forward, from its start or from a hint the medium layer gives. It never reads a
raw medium from its end, where a stale tail could pass for the newest piece.

### Pieces on a raw medium

A closed piece is a multiple of 4 KiB long on a raw medium too. A piece that starts on a 4 KiB boundary therefore
ends on one.

The layout's `gap_max` and `resume_align` still say where a writer goes on after an erased or bad stretch. A piece
appended after a closed one starts on a resume place whenever `resume_align` divides 4 KiB, as a 512-byte sector
does.

## Blank space and gaps

**Blank** bytes are all 0x00 or all 0xFF: what an erased or preallocated medium reads, depending on its kind. No block
starts with either value.

A blank stretch that runs to the end of the file or medium is therefore the end of the data.

A blank or damaged stretch in the middle is not always the end. Two kinds of writer leave such stretches:

- A NAND writer whose page program failed moves on to the next erase block.
- A writer that pads a NAND page at a sync leaves 0xFF behind.

The **layout**, a record among the definitions, says how far a writer may skip and where it resumes:

| Medium | Longest gap (`gap_max`) | Resumes at a multiple of (`resume_align`) |
|---|---|---|
| file (`Layout::UNALIGNED`) | 0: the first bad stretch is the end | 1 byte |
| SD card or eMMC (`Layout::on_sd_card`) | 4 MiB | 512 bytes, a sector |
| raw NAND (`Layout::on_raw_nand`) | two erase blocks | one page |

Past a blank or bad stretch, a reader reads one block header at the first non-blank byte. It also reads one at each
resume place within the layout's `gap_max`.

A good block of the current session may go on with the data there. That block makes the stretch a **gap**, which the
reader reports with its length, and the walk goes on.

When there is no such block, the stretch is the end of the data.

### What a gap costs a reader

The format bounds what a reader pays per gap:

- A valid layout allows at most 64 MiB of gap with at most 8192 resume places.
- A reader checks at most 4 whole blocks per gap.
- On a raw medium, blank space longer than the gap is the end, and the reader reads nothing after it. An erased 64 GB
  card costs one gap's worth of header reads.
- A Resync walk looks once per stretch too, and only up to where Prefix ends.
- A Resync walk reads up to one gap and a block ahead, 128 MiB at most (`RESYNC_AHEAD_MAX`).

## Duplicates

A writer that retried a write may leave the same block twice. The same session and sequence number twice is a
**duplicate**, not a cut.

The two walks treat duplicates as follows:

- With the same bytes, Prefix drops the second copy.
- With other bytes, Prefix takes the later copy and reports the replacement (`FileError::Replaced`).
- Resync reports a pair with other bytes as a conflict instead. In damaged bytes it can't tell a copy from a rewrite.

## Example: a logger card recorded twice

A data logger records to an SD card as a raw medium. Its first recording, session A, filled 100 MiB.

The card is reused. Session B writes 30 MiB from the start, then the power fails during a write.

A reader walks forward from the start of the card. It takes B's blocks up to the torn one, then probes for B's next
block within 4 MiB.

The reader finds only A's old blocks. They carry A's session tag and don't start with a piece head naming B.

A's blocks are therefore stale, and the data ends at the torn block. The reader mistakes none of A's 70 remaining MiB
for B's.

When the logger boots again, it finds the same end of data and writes its next session after it. That session, C,
starts with a piece head that names B as the previous session.

The next reader takes C. A's leftovers beyond C stay stale.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/sessions.svg"
<figcaption>Above, the example's card after the power failure: B's blocks end at the torn one, and A's older blocks
behind them carry another session tag. Below, after the next boot, a reader goes on into session C, whose piece head
names B, and stops at A's leftovers again.</figcaption>
</figure>

## What tape and flash media taught the format

Tape drives and flash memory solved these problems long before. The format takes their answers where they fit, and
each point below names its source.

### Sessions and the end of the data

- **A tag per write session.** Linear Tape-Open (LTO) marks every data set with a write pass number. The number grows
  whenever data is overwritten, so a reader tells current data from residue (ECMA-319, §13.5.16).
- **The same in flash.** Unsorted Block Images (UBI) stamps an image sequence number on every eraseblock for the same
  reason (Linux `ubi-media.h`).
- **A random tag, not a counter.** The session tag does this job. It is random, because a wiped card or a device
  without stored state has no counter to continue.
- **Nothing after the end is data.** LTO declares data recorded after the end-of-data set inaccessible and invalid
  (ECMA-319, §13.4.3).
- **No end-of-data block.** The format has none, since a crash leaves none. Blank space and the session tag mark the
  end instead.

### Erased bytes and gaps

- **Erased is not always zero.** NAND flash erases to 0xFF (Micron TN-29-19). An SD card reads 0 or 1 after an erase,
  by vendor (SD Physical Layer Simplified Specification 9.10, §4.3.5.1).
- **eMMC says which.** An eMMC device reports its erased value (JESD84-A441). Blank is therefore 0x00 or 0xFF.
- **A blank unit isn't the end.** A NAND writer that hits a failed program moves on and leaves erased pages behind.
- **Looking past them.** The UBI file system (UBIFS) looks past such units when it scans for the end
  of written data (Linux `fs/ubifs/scan.c`, `recovery.c`).
- **Gap sizes follow the media.** The layout's gap and resume alignment say how far to look. SD allocation units reach
  64 MB (SD 9.10, §4.13.1).
- **NAND units.** The Open NAND Flash Interface (ONFI) defines NAND erase blocks and pages (ONFI 6.0, §3.1).
- **Program a page once.** Programming the same part of a page twice without an erase gives undefined contents
  (ONFI 6.0, §6.7.1.25).
- **One program per page.** Micron's multi-level cell (MLC) example allows one program per page (Micron TN-29-19). A
  writer on raw NAND pads the rest of the page at each sync, and readers take the padding as a gap.

### Rewrites, indexes, compression and correction

- **Rewrites: the later copy wins.** LTO rewrites an interrupted data set in its entirety, and the later write counts
  (ECMA-319, §15.4). The same block twice in a tape is a duplicate, and the reader takes the later copy.
- **Self and back pointers.** A Linear Tape File System (LTFS) index carries a self pointer and a
  pointer to the index before it. The LTFS Format Specification 2.5.1 gives both in §5.4.2 and §5.4.3.
- **A wrong self pointer means data.** The same specification takes an index whose self pointer doesn't match as data.
- **The same two offsets.** A tape's index blocks carry the same two offsets. A reader checks the self offset against
  where it found the block.
- **Never grow incompressible data.** Streaming Lossless Data Compression (SLDC) switches to a scheme that copies
  bytes (ECMA-321, §7.2).
- **What that saves.** With SLDC, incompressible data grows by about 0.05 % instead of 12.5 %.
- **The same in a tape.** A writer that codes a block's body keeps the plain body when coding doesn't shrink it.
- **Correction belongs to the medium.** ONFI leaves error correction to the host controller, per page (ONFI 6.0,
  §6.7.1.26). A tape only detects what that correction missed.

## See also

- [Damage and recovery](recovery.md): checksums, and resuming after a crash.
- [Reading a damaged file](damaged_files.md): the walk past damage, salvage, and the limits of the format.
- [Format reference](format.md#sessions-media-and-damage): sessions, media, gaps and state records as a reference.
- [Syncing, cuts and relays](syncing_and_cuts.md#adopting-blocks-on-raw-media): writing a piece as one session on a raw
  medium.
