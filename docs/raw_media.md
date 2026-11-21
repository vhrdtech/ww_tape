---
level: advanced
---

# Where the data ends on a raw card

A [[raw medium]], such as an SD card without a file system, has no end of file. When a recorder writes
onto a card used before, the blocks of the earlier recording stay valid behind the new ones. A [[reader]] must find
where today's recording ends, and read it past [[stale data]], [[gaps]] and [[duplicates]].

ww_tape on a raw card serves a recorder with a host-class processor and RAM. Its [[writer]] keeps the index of the open
piece in memory. Firmware on a small device writes its card with
[logring](related_books.md#logring-a-log-store-on-the-device) instead, a ring that needs no heap and little RAM.
[Tapes and logring](https://lr.vhrd.tech/tapes_and_logring/) compares the two.

Besides the host's files, for example, a recorder box on the bench writes the drive's run onto an SD card. The card
still holds an earlier run of the same board, a full hour. At minute 37 the power fails, and the box stops in the
middle of a block. Good blocks of the earlier run lie right behind it.

## Sessions and stale data

In a file, the data ends where the file ends. A [[raw medium]] has no such end. A reused card holds an older recording
behind the current one, and its [[blocks]] are as valid as the new ones. A reader tells them apart by their [[session]].

A [[session]] is one writer's unbroken append, such as a producer's start or a device's boot. Every block of a session
carries the same 32-bit [[session tag]] in its header, and its sequence numbers count up from zero. A tag is never zero
nor FFFFFFFFh, so an erased header never names a session. A writer gets its tag in one of three ways:

- [`SessionTag::random`] picks it at random, as a device without stored state does;
- [`SessionTag::from_seed`] derives it from a seed;
- [`SessionTag::of_piece`] derives it from the [[piece]]'s id, so the same input gives the same bytes.

The drive's recorder takes the third way. The earlier run on the card is another chain of the board, so its pieces have
other ids and other session tags.

### Where a new session may start

A reader keeps to one session at a time. A good block of another session is [[stale data]], unless a new session may
start at that block. Where one may start depends on the medium, a [`Medium`] value the reader is opened with:

| Medium | A new session may start at |
|---|---|
| a file ([`Medium::File`]) | a records block that starts with a definition, where another producer appended; any [[piece head]], where files were joined |
| a raw medium ([`Medium::Raw`]) | only a piece head that names the session just ended as its previous one |

On the card, today's run ends at the [[torn block]], and the earlier run's blocks follow. They carry another session
tag, and none of them is a piece head naming today's session. A reader of the raw card therefore takes them as stale
data and ends today's run at the torn block. A reader judges a block by its session before its format version. Stale
data that an older firmware wrote in another format thus ends the data too.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/sessions.svg"
<figcaption>A card recorded twice. Above, after the power failed: session B's blocks end at the torn one, and A's older
blocks behind them carry another session tag. Below, after the next boot, a reader goes on into session C, whose piece
head names B, and stops at A's leftovers again.</figcaption>
</figure>

### Reading forward, never from the end

A reader of a file finds the newest [[piece]] from the end, by its [[tail]]. On a raw medium the end holds stale data or
nothing, and a stale tail there would pass for the newest piece. A reader of a raw medium therefore walks forward, from
the start or from a hint the medium layer gives ([`OpenOptions::start`]).

The drive's card shows the difference. It is 109.95 MiB long, and today's run wrote 68.97 MiB of it before the power
failed. Read as a raw medium, the card gives today's 3 pieces, the last one ending at the torn block. The earlier run's
other 40.99 MiB stay behind as stale data. Read as a file, the card gives 4 pieces, and the fourth is the earlier run's
last piece, found by its tail.

`cargo run -p ww_tape_example` prints these numbers, and this doctest reads the same card at 1/1000 of its data:

```rust
use ww_tape::{Medium, OpenOptions, TapeReader};
use ww_tape_example::{Drive, EARLIER_CHAIN};

let today = Drive::scaled(1000).expect("1000 divides the rates").on_sd_card();
let earlier = today.chain(EARLIER_CHAIN).record().expect("the earlier run records");
let run = today.record().expect("the run records");
let crashed = today.crash(run.story_crash_byte().expect("minute 37")).expect("the crash");
let card = crashed.on_card(&earlier).expect("the earlier run fills the card");

let raw = OpenOptions { medium: Medium::Raw, ..OpenOptions::default() };
let on_card = TapeReader::open_with(&card.bytes[..], raw).expect("the card opens");
// Today's two closed pieces and the torn one; the earlier run is stale data.
assert_eq!(on_card.pieces().len(), 3);

let as_file = TapeReader::open(&card.bytes[..]).expect("the card opens as a file");
// Read as a file, the earlier run's last piece passes for the newest.
assert_eq!(as_file.pieces().len(), 4);
```

## Writing a tape onto a card

A tape on a raw medium is written by the same [[writer]] as a file, with two differences. The [[layout]] says how far a
reader looks past a bad stretch, and every [[piece]] is one [[session]]. The drive's recorder for the card sets the
layout with [`Layout::on_sd_card`], and [`Drive::on_sd_card`] in `ww_tape_example` does the same.

A piece on a raw medium is one session, since a new session may start only at a piece head. The writer's next piece of a
chain starts with a piece head that names the session of the piece before it. A reader thus goes on from one piece into
the next. A writer that relays another producer's blocks onto a card passes them with [`TapeWriter::adopt_block`], which
renumbers each block into the writer's own session. A jump in the producer's sequence numbers stays a jump, so a reader
still sees that blocks were lost on the way.

On a raw medium too, a closed piece ends on a multiple of 4 KiB. A piece that starts on a 4 KiB boundary thus ends on
one. The next piece then starts where a reader looks for it after a bad stretch.

## Blank space and gaps

Blank bytes are all 0x00 or all 0xFF: what an erased or preallocated medium reads, depending on its kind. No block
starts with either value, so blank space that runs to the end of the medium is the end of the data. A blank or damaged
stretch in the middle is not always the end. A writer on raw NAND flash whose page program failed moves on to
the next erase block. A writer that pads a NAND page at a sync leaves 0xFF behind.

The [[layout]] says how far a writer may skip and where it goes on, at a multiple of its resume alignment:

| Medium | Longest gap (`gap_max`) | Goes on at a multiple of (`resume_align`) |
|---|---|---|
| a file ([`Layout::UNALIGNED`]) | 0: the first bad stretch is the end | 1 byte |
| an SD card or embedded MultiMediaCard (eMMC) ([`Layout::on_sd_card`]) | 4 MiB | 512 bytes, a sector |
| raw NAND ([`Layout::on_raw_nand`]) | two erase blocks | one page |

A reader learns these rules from the layout definition after the piece head, and again from its copy at least 1 MiB
on. A medium layer that knows the medium can give them from the start ([`OpenOptions::layout`]). A walk that starts
past lost first sectors needs them there, since until the copy nothing in the piece says them.

Past a blank or bad stretch, a reader reads one block header at the first non-blank byte and one at each resume place
within `gap_max`. A good block of the current session there goes on with the data. It makes the stretch a [[gap]], which
the reader reports with its length ([`FileError::Gap`]) before it reads on. When no such block follows, the stretch is
the end of the data.

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
[[duplicate]], not a new block. A reader in [`Recovery::Prefix`] drops a second copy with the same bytes. It takes a
copy with other bytes in place of the first, and reports the replacement ([`FileError::Replaced`]).

[`Recovery::Resync`] reports a pair with other bytes as a conflict instead, as
[When bytes go bad](damage.md#reading-past-damage) explains. In damaged bytes it cannot tell a copy from a rewrite, so
it takes neither for the other.

[Lessons from tape and flash](lessons.md) names the tape drives and flash file systems that solved these problems first,
and what the format takes from each.
