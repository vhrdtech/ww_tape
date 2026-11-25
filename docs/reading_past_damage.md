---
level: basic
---

# Reading past damage

A reader that walks blocks in order must decide what to do at a bad [[block]]: stop, or read on. A crash leaves a torn
end, where stopping is right. A bad sector in the middle leaves good blocks behind it, which a salvage tool wants. This
page shows both walks over the drive's last [[piece]] with a flipped bit ([When bytes go bad](damage.md)). It then
salvages the items of a damaged events chunk.

## Two recovery modes

[`FileBlocks`] walks the blocks of a byte stream in order. What it does at damage depends on its [[recovery mode]], a
[`Recovery`] value given to [`FileBlocks::with_recovery`].

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/prefix_resync.svg"
<figcaption>One piece with two damaged blocks, walked twice. Prefix reads the blocks before the first damage and stops.
Resync hands out the chunk with its bad segment marked, reports the bad records block, skips to the next block and reads
on. Drawn by <code>ww_tape_figures</code> from the drive's last piece at 1/1000 of its data, a bit flipped in a chunk of
<code>phase_a</code> and one in a records block.</figcaption>
</figure>

| | [`Recovery::Prefix`], the default | [`Recovery::Resync`] |
|---|---|---|
| answers | how far the bytes are valid | what still reads anywhere |
| at a bad block | stops and reports [`FileError::Cut`] | reports [`FileError::Bad`] and reads on |
| a chunk with a good chunk head and a bad payload | stops there | hands it out, marked |
| sequence numbers | must not go backwards | may go anywhere; a repeated one is checked |

Over the drive's flipped piece, `cargo run -p example_drive` counts what each walk hands out:

--8<-- "docs/generated/damage_walks.txt"

### Prefix

Prefix reads the longest valid prefix of the bytes. It stops at the first of these:

- a block with a bad magic, length or CRC;
- a block cut short;
- a sequence number going backwards;
- a good block of another [[session]] where none may start.

It reports where it stopped and why. A crash leaves good blocks, then a [[torn block]], which is what Prefix expects.

### Resync

At a bad block Resync looks for the next block's magic one byte after the bad block's start. Any byte may start a block,
so Resync takes a block only when its CRCs match. It hands out a chunk with a good chunk head and a damaged payload
instead of skipping it. [`Block::is_intact`] is then false, and [`Chunk::bad_segments`] names the failed segments.

This walk counts what a flipped bit costs, as a test or a salvage tool would:

```rust
--8<-- "ww_tape_tests/tests/it/book_pages.rs:flipped_bit"
```

### Repeated sequence numbers

Damage can hide any block, so Resync has no order check on [[sequence number|sequence numbers]]. It guards against a
repeated number instead. It remembers the latest blocks of data it handed out, as many as the [format
reference](format.md#duplicates) states. A good block with the [[session]] and sequence number of one of them but other
bytes is a conflict ([`FileError::Conflict`]). Resync never hands out the second block of a conflict as a block of its
own.

The first block of a conflict went out already when Resync meets the second. An application that keeps what Resync
gives therefore holds blocks back since the last damage, or can take one back.

## Salvaging the items of an events chunk

An [[events chunk]] usually has one segment, so a flipped bit in it fails that segment whole. Its columns still carry
enough structure to [[salvage]] most items: read every item it can and mark the rest [[suspect]].
[`EventTimes::salvage`] and [`Payloads::salvage`] do this.

The chunk stores the time codec and the payload lengths per group of 128 items. Each group's record must agree with its
neighbours in three ways:

1. its offsets continue where the previous group's end;
2. its ticks are in order;
3. its widths are in range.

A group that breaks one of them is suspect, and so are its items only. The cost depends on where the damage lands:

| Damage | Suspect items |
|---|---|
| a flipped bit in one time residual | one item |
| a flipped bit in a group record | the items of one group or two |
| any bad byte in a payload column coded with lz4 | every item of the chunk, since lz4 decodes the column as a whole |

What damage costs a whole piece, at its start, its end or between, is the next page: [What a damaged piece
gives](damaged_pieces.md).
