---
level: advanced
---

# Where the data ends on a raw card

A [[raw medium]], such as an SD card without a file system, has no end of file. When a recorder writes
onto a card used before, the blocks of the earlier recording stay valid behind the new ones. A [[reader]] must find
where today's recording ends. This chapter takes the questions in two pages:

- this page: [[session|sessions]], [[stale data]], and how a writer puts a tape onto a card;
- [Gaps and duplicates on a raw card](raw_gaps.md): blank and bad stretches inside a recording, and blocks written
  twice.

ww_tape on a raw card serves a recorder with a host-class processor and RAM. Its [[writer]] keeps the index of the
open piece in memory. Firmware that writes an SD card or a flash chip directly takes
[logring](https://lr.vhrd.tech/) instead. That store needs no standard library, no heap and little RAM.
[Tapes and logring](https://lr.vhrd.tech/tapes_and_logring/) compares the two.

In the example, the drive's run goes onto an SD card that still holds an earlier hour of the same board. The power
fails at minute 37 in the middle of a block, so good blocks of the earlier run lie right behind the torn one.

## Sessions and stale data

In a file, the data ends where the file ends. A [[raw medium]] has no such end. A reused card holds an older recording
behind the current one, and its [[blocks]] are as valid as the new ones. A reader tells them apart by their [[session]].

A [[session]] is one writer's unbroken append, such as a producer's start or a device's boot. Every block of a session
carries the same 32-bit [[session tag]] in its header, and its sequence numbers count up from zero. A tag is never zero
nor FFFFFFFFh, so an erased header never names a session. A writer gets its tag in one of three ways:

- [`SessionTag::random`] picks it at random, as a device without stored state does;
- [`SessionTag::from_seed`] derives it from a seed;
- [`SessionTag::of_piece`] derives it from the [[piece]]'s id, so the same input gives the same bytes.

The drive's recording takes the third way. The earlier run on the card is another chain of the board, so its pieces have
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

The drive's card shows the difference. It holds about 116 MB, and today's run wrote about 72 MB of it before the power
failed:

| Card read as | Pieces it gives | The earlier run's other 43 MB or so |
|---|---|---|
| a raw medium | today's 3, the last one ending at the torn block | stay behind as stale data |
| a file | 4: the fourth is the earlier run's last piece, found by its tail | pass for the newest piece |

`cargo run -p example_drive` prints these numbers, and this test reads the same card at 1/1000 of its data:

```rust
--8<-- "ww_tape_tests/tests/it/book_pages.rs:stale_card"
```

## Writing a tape onto a card

A tape on a raw medium is written by the same [[writer]] as a file. The drive's recorder for the card sets the
[[layout]] with [`Layout::on_sd_card`], and [`Drive::on_sd_card`] in `example_drive` does the same. Four things differ
from a file:

| On a raw medium | So that |
|---|---|
| the [[layout]] says how far a reader looks past a bad stretch | a reader goes on past a [[gap]] ([Gaps and duplicates](raw_gaps.md)) |
| every [[piece]] is one [[session]], and the next piece head names the session before it | a reader goes on from one piece into the next |
| right after its head, each piece carries the chain's definitions and each [[state track]]'s newest item | a walk that starts at a piece head reads that piece alone, the mode in force included |
| a closed piece ends on a multiple of 4 KiB | the next piece starts where a reader looks for it after a bad stretch |

A piece is one session since a new session may start only at a piece head. A walk may start at such a head from a hint
of the medium layer.

A writer that relays another producer's blocks onto a card passes them with [`TapeWriter::adopt_block`]. It renumbers
each block into the writer's own session. A jump in the producer's sequence numbers stays a jump, so a reader still sees
that blocks were lost on the way.

What a reader does at a blank or bad stretch inside today's session is the next page: [Gaps and duplicates on a raw
card](raw_gaps.md).
