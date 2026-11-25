---
level: advanced
---

# Reading a growing piece

A piece is not always finished when a reader opens it. A writer may still write it, or may have closed it and opened
it again to write on. A recorder that closes its file every minute is such a writer, and a viewer may open the file in
between. The viewer should read every closed minute and the whole blocks of the minute being written.

## The close in force

A writer may close a piece, open it again later and write on after the close. Each such session ends with a close
record and a tail of its own, so one piece can hold several closes ([Closing more than
once](format.md#closing-more-than-once)). The stretch after a close starts with an append head.

The reader takes the last close whose tail is whole and follows it directly: the [[close in force]]. Bytes after it
form an [[unclosed tail]]: a writer still writing, or one that a crash stopped. The reader lists their whole blocks as
it lists those of an unclosed piece, and never reports them as damage.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/growing_piece.svg"
<figcaption>A piece closed twice and written on again. The second close is the close in force, and the blocks after
its tail form the unclosed tail, which grows while the writer writes.</figcaption>
</figure>

The reader tells where each part lies:

- `PieceInfo::close` holds the close in force, and [`TapeReader::closes`] lists it with every close before it,
  oldest first.
- [`PieceInfo::closed_end`] gives the offset where the close in force ends, at the end of its tail.
- [`PieceInfo::is_closed`] is true when nothing follows that tail.
- [`TapeReader::unclosed`] gives the range of the unclosed tail.
- [`TapeReader::closed_entries`] gives the index entries that the close in force covers.
- [`TapeReader::close_summary`] reads the summary block that the close in force names, when it names one.

A test closes a piece once and writes three more blocks after it. The reader then sees one close and an unclosed tail
from the close's end to the end of the bytes:

```rust
--8<-- "ww_tape_tests/tests/it/reclose.rs:close_in_force"
```

## Noticing that a tape grew

A reader sees the bytes that its source held when it opened. A writer that appends later changes nothing the reader
holds, so the reader goes on reading a consistent prefix.

To notice growth, compare the size of the source with the length the reader opened. When the size is larger, open
the source again to read the new bytes:

```rust
--8<-- "ww_tape_tests/tests/it/reclose.rs:growth"
```

## Checking each close

[`TapeReader::verify`] checks each stretch of the piece against the close that ends it, oldest first. The result says
whether every check held, and which bytes no close covers yet:

```rust
--8<-- "ww_tape_tests/tests/it/reclose.rs:verify"
```

Once the writer closes the piece again, the same check covers two stretches and leaves no byte unchecked.

## Following a chain while it is written

A reader in another process than the writer follows a [[chain]] with [`ChainFollower`]. It hands out each [[block]]
once the block checks, and remembers where the valid prefix of the [[piece]] ends. When the file grows, it reads on
from there. A block the writer is still writing, or a [[torn end]] that a crash left, is read again at the next look.
So a block is never handed out half.

A writer that resumes after a crash cuts back the torn end and writes on in the same piece. It also cuts back blocks
that wait for the next block: puts that belong with it, and a piece's end before its tail. The follower holds such
blocks back until the block they wait for checks. It never hands out a block that a resume took back. At the tail of
a piece whose [[close]] names the next piece, it moves on to that piece's file.

Three pieces build on the follower:

- [`TrackFollower`] gives the items of one [[track]], decoded as their [[chunks|chunk]] land, and reports a jump in
  item numbers as lost items.
- [`TimeMerge`] merges several chains, one per capture process, by time. An item comes out once every chain has shown
  that it holds nothing older, so the merge waits for the slowest chain. A chain that stays quiet longer than a set
  wait is left out, and its older items come out marked late.
- [`TapeMux::watch`] wakes a reader in the writer's own process each time the writing thread places more, with no
  polling.
