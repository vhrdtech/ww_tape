---
level: advanced
---

# Checks and range hashes

Recordings get copied, cut apart and archived, and whoever holds a copy must know that it is whole and unchanged. A
tape answers this at three levels. A [[chain]]'s links show a missing or swapped [[piece]], and a reader verifies a
piece's bytes. A [[range hash]] proves that two files hold the same data in a range of items.

For example, the drive's last piece, which holds the fault, is copied alone to another computer, and an archive
keeps the whole chain. The copy must hold the frames the bench recorded, and the archive must hold every piece
unchanged.

## The links of a chain

Each [[piece]] names its neighbours, so a [[reader]] tells a whole chain from one with a piece missing or swapped. The
links run both ways:

- **Backward.** Each [[piece head]] names the piece before it by its id and by its [[content hash]]. The content hash
  is a BLAKE3 hash, a cryptographic hash, over the earlier piece's bytes, and the earlier piece's [[close]] states
  it.
- **Forward.** A close cannot name the next piece's hash, which is not written yet. It names the next piece's id, and
  the next piece head repeats that id.

[`TapeReader::chain`] checks every link between the pieces at hand. It reads nothing beyond what opening read, since
the heads and closes are already in memory. Its [`ChainReport`] lists each link, how the chain ends, and the pieces
that belong to another chain. [`ChainReport::is_whole`] says that every piece is there and linked.

```rs
--8<-- "examples/example_drive/src/fault.rs:check"
```

??? example "What the example uses here: `Checked`"

    ```rs
    --8<-- "examples/example_drive/src/fault.rs:checked_item"
    ```

The drive's chain has three links between its four pieces, and all three are linked. The report also tells a chain whose
first pieces were deleted to free space from one with a piece lost in the middle. [Tape pieces](format.md#tape-pieces)
in the format reference lists every case.

## Verifying a piece

A link proves that a piece head names the hash its neighbour states. It does not prove that the piece's bytes still
have that hash. [`TapeReader::verify`] does: it hashes the piece's bytes before its close and compares the result with
the content hash that the close states.

Verify also replays the piece block by block, as its [[writer]] placed them. Each [[index entry]] must be the entry that
the writer makes of the block at its offset. Each track's row in the close must match the track's blocks. A writer that
wrote a wrong index therefore fails too, not only a changed byte.

Verify reads every byte. For the drive's chain it reads about 119 MB in 41 534 reads. That is about 3.9 MB more
than the recording's 116 MB. Verify reads each block's 32-byte header before the block, 13 824 headers in all.

Verify reads each [[final index]] whole, about 1.8 MB in all. It then reads the first 120 bytes of each index entry again,
one by one as it replays the blocks: about 1.7 MB more. It does not read the closes and tails, about 14 KB in all.

All four pieces hold. An archive runs verify once after a transfer and on a schedule, never on each open.

## Track hashes

A content hash covers a piece's bytes, so it changes whenever any byte does. Two files can hold the same data in
other bytes, such as a recording joined into one file and the same recording in four. To compare data, a
reader needs a hash of a [[track]]'s content that does not depend on where the content lies.

The [[track hash]] is that hash. It starts as 32 zero bytes and takes in each [[chunk]] and each [[record]] of the
track as the writer writes them. The writer leaves out [[puts]] and [[state copies]], which repeat what the tape
already holds. Each chunk's share is its [[unit hash]], which the writer stores in the chunk's index entry. The
piece head states each track's hash before the piece, and the close states it after the piece.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/track_hash.svg"
<figcaption>Each chunk's unit hash, stored in its index entry, folds into the track's hash in item order. The head and
the close of each piece state the hash before and after it. A range hash folds only the chunks of a range, from the
entries alone.</figcaption>
</figure>

The track hash depends only on the track's chunks and records, in order. A track written alone or among other
tracks, in one piece or in many, ends with the same hash. A recording relayed or stored again block for block keeps
that hash.

The hash takes in each chunk whole: its items, its head and how its columns are coded. Two tracks with the same hash
therefore hold the same chunks, and so the same data. A different hash proves less. The same items cut into other
chunks, or coded with another codec, hash differently. Only a read of the items then tells whether the data differ.

A track's definition also names its [[track owner]], 32 bytes that ww_tape does not interpret. Two devices of the
same type recorded into one tape have tracks with the same keys, and the owner tells them apart. Each such track has
its own item numbers and its own hash.

## The hash of a range

A copy of the last piece alone cannot compare whole track hashes with the archive's chain. It compares a range
instead. [`TapeReader::range_hash`] folds the unit hashes of the chunks that hold any item of a range, from 32
zero bytes, and returns a [`RangeHash`]. The result depends only on those chunks, so the archive's chain and the
copied piece give the same hash for the same range:

```rs
--8<-- "examples/example_drive/src/fault.rs:range"
```

??? example "What the example uses here: `frame_items`, `SameFrames`"

    ```rs
    --8<-- "examples/example_drive/src/fault.rs:frame_items_item"

    --8<-- "examples/example_drive/src/fault.rs:same_frames_type_item"
    ```

For the frames of minutes 51 to 53, items 3 060 000 to 3 179 999 of `can0`, the range hash folds 120 chunks. Each
unit hash comes from its index entry, so the fold reads no payload: the chain's reader reads about 577 KB in 15 reads.
The hashes in the chain and in the last piece alone are equal. A program that analysed those frames can store the
range hash beside its result. Anyone can later check that a file still holds that input.

A fold from the index gives the hash that the writer stated there. [`RangeHash::stated`] counts the chunks folded that
way, all 120 here. Only verify proves the stated hashes against the chunks' bytes, and every read of a chunk checks its
own. Where the index is damaged or missing, the reader reads the chunks and computes the same hash. It counts the
bytes that no block explains in `skipped`, so a fold over damage never passes for a whole one.

[When bytes go bad](damage.md) shows what still reads when an archived piece has a flipped bit.
