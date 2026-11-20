# Resuming a writer

A crash leaves a piece without its close and tail, often with a torn block at its end. This page shows how a writer
reopens such a piece and how a whole chain resumes.

The producer's part is small: it gives its calls again, and the piece ends with the same bytes as a write without the
crash. The page assumes [Writing a tape](writing.md) and [Syncing, cuts and relays](syncing_and_cuts.md).

## Resuming after a crash

`TapeWriter::resume_on(sink, options)` reopens the unclosed piece in a sink and goes on writing the same piece. For a
file, `ww_tape_fs::writer::resume(path, options)` opens the file `NAME.partial` that `writer::create` wrote and resumes
it.

The call returns the writer and a `Resumed`, which says:

- how many bytes the resume kept (`kept_bytes`);
- how many bytes it dropped (`dropped_bytes`);
- where the reader stopped taking blocks (`cut`).

A resume works in three steps:

1. It finds the piece by its tail, else from the newest checkpoint and a scan after it, else by a scan from the start.
   It looks as a reader would, and a scan stops at the first bad block.
2. It keeps every byte up to the end of the last good block. It cuts off the rest only when that is a torn end.
3. It replays the kept bytes and rebuilds the writer as it was.

A torn end may hold an end of piece whose tail is missing. It may also hold blocks marked `FLAG_WITH_NEXT` at its
start, which step 2 cuts off because the block they belong with did not survive.

### What counts as a torn end

A torn end is what a crash leaves of the last write: whole blocks, then a record that the bytes stop inside of. That
record has fewer bytes than a block header, or a header that names more bytes than are left.

Blank bytes (zeros) up to the end count as a torn end too. A file holds them where it grew but its data was never
written.

Anything else in the bytes to cut is damage. The resume then refuses with `ResumeError::Damaged`, naming where the
damage starts.

Damage is a whole record that fails its cyclic redundancy check (CRC), or bytes that start no record. A whole tail
that does not end the close record before it is damage too.

A record that names more bytes than are left is damage too when its bytes show it was whole. A block start may lie
inside them, or they may read as a good block once its length field names them.

For example, one flipped bit in the last tail makes the tail fail its CRC, so the reader takes the close before it.
The resume keeps every byte: the close record that readers took before the flip stays.

The check reads only the bytes. Damage in many places at once can make a whole record look torn, and the resume then
cuts it.

### What the replay rebuilds

The replay rebuilds the writer's state from the kept bytes:

- the content hash;
- the index entries;
- the track ranges;
- the record of what lies near;
- the session and the next sequence number.

The replay walks the bytes as a reader's verify does, and takes a good block wherever the walk meets one. In bytes that
no block explains, it tells two cases apart:

- A block that the index lists is kept by its entry, as a damaged block, when it ends before the next good block.
- Other bytes are junk: the replay keeps them and never lists them.

A damaged block kept by its entry stands for what the entry says. A damaged chunk goes into its track's hash by the unit
hash of its entry.

The records of a damaged records block cannot go into the hash. The close therefore marks the hash of each track that
may hold records as incomplete.

### When a resume refuses

Step 2 cuts off only a torn end. Where cutting could lose more, the resume refuses and leaves the bytes as they are:

| Refusal | When |
|---|---|
| `ResumeError::Damaged` | the bytes to cut hold damage rather than a torn end: a whole record that fails its check, bytes that start no record, or a tail that does not end its close |
| `ResumeError::GoodBlockAfterCut` | a good block of the piece's sessions lies behind the bytes the reader did not take; with no block kept, a good block of any session counts |
| `ResumeError::LookSpent` | the look for good blocks ran out before the end of the bytes: it reaches at most `RESUME_LOOK_MAX` bytes far and reads at most `RESUME_PROBES_MAX` places whole, so it cannot tell that the rest holds no good block |
| `ResumeError::HashWalkSpent` | the kept bytes claim so many blocks that do not read that a reader's content-hash walk stops trying blocks before their end (`WALK_MISS_PER_BYTE_MAX`), so a block written after them would not verify |

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/resume.svg"
<figcaption>What a resume keeps. The puts at the end belong with the block of data that was torn, so they go with it;
the producer gives that call again, and it writes the same puts and the same block.</figcaption>
</figure>

### Giving the calls again

The producer then gives again every call from the block that `TapeWriter::next_seq` reports, with the same definitions.
A call that placed several counted blocks, such as a chunk with deferred blocks in its gap, is given again whole.

What the writer adds depends only on the bytes before it, the calls and the options. The resumed piece therefore ends
with the same bytes, and the same content hash, as a piece written without the crash.

A put is idempotent, so the producer simply puts again what it uses. It needs no record of which puts reached the disk.

The writer holds no definitions, so a resume needs none from the file. The application passes its own, as before the
crash.

### Options and the end of a piece

A resume checks the options against the piece before it cuts or writes anything. Pass the options that the piece was
written with, which `TapeWriter::options` returns.

The resume refuses a layout or a producer other than the newest one the piece kept (`InputError::Layout`,
`InputError::Producer`).

With another layout or producer, the writer would place blocks other than the way the file says.

Three cases at the end of a piece differ:

- A piece whose last close and tail read whole is closed. The resume cuts off a torn append after the tail and writes
  on after it, an append head first.
- A piece that a cut closed names the next piece in its close. The resume gives it its name and returns
  `ResumeError::Continued`.
- A piece cut off inside its close record or tail is opened again. Its final index and close are cut off, and the
  next close pads the piece to 4 KiB again.

[Closing more than once](format.md#closing-more-than-once) shows the bytes of a piece written on after its close.

A good block after the tail of a closed piece refuses the resume (`ResumeError::GoodBlockAfterCut`), unless an append
head leads to it. A damaged append head is such a case: the resume never cuts off the blocks after it.

## Resuming a closed piece

A resume of a closed piece never reads the blocks before its close in force. An append to a large piece therefore
costs about as much as an append to a small one.

The writer's state at the close comes from what the close names:

- the close record: each track's range, hash and counted records, and the session and seq of its block id;
- the piece head, and the layout and producer put right after it, which the options are checked against;
- for a piece holding chunks, the index chain and each track's last chunk, for the newest relation values.

An index chain that does not read leaves those tracks unread. The next chunk of such a track then needs a break
the caller accepts (`TapeWriter::accept_relation_break`), so damage before the close never stops an append.

The record of what lies near starts empty. The writer thus puts again, after the append head, the definitions the
blocks after it need.

Only the close record, its tail and the bytes after them are walked and replayed. A crash during an append resumes
from the same close, so the piece ends with the same bytes as an append without the crash.

## Resuming a chain

`PieceChain::resume_on` (`ww_tape_fs::chain::resume` for files) does the same for a chain. It reads the closed pieces
back from their tails and then deals with the last piece.

A closed piece that a crash left as `NAME.partial` gets its name. What happens next depends on how the last piece
ended:

| Last piece | Chain of a tape per piece | Chain in one tape (`PieceTapes::One`) |
|---|---|---|
| unclosed | resumed | resumed |
| closed naming a next piece: the crash came during a cut | the next piece starts | the next piece starts |
| closed or aborted, naming no next piece | written on after its close | written on after its close |
| its piece head is damaged | refused | refused |

A chain therefore gains a piece only at a cut: a session that opens it again goes on in its last piece. The resume
refuses a last piece with a damaged piece head.

The blocks after that head would form a piece with no link to the chain. [Damage and
recovery](recovery.md#resuming-after-a-crash) follows a recorder through a power cut.

### Monotone relations after a resume

A chain resume also restores the newest value of each **monotone relation**: a per-item value that a track declares
never to decrease. It takes the range relation's newest value from every closed piece's index entries, which it reads
anyway.

The other relations' values are in chunks. For each track, the resume reads one chunk of at most `MAX_BLOCK_LEN` (64
MiB): the track's last chunk in the newest closed piece that holds one.

A resume of a chain of N tracks therefore reads up to N chunks besides each piece's head, close, tail and index.

The writer cannot vouch for an order it cannot read. Where that chunk does not read, it refuses the track's next chunk
(`DescribeError::RelationUnread`).

Only the caller can go on. The call `TapeWriter::accept_relation_break(defs, track, relation, from)` checks the track's
next chunk against `from` for that relation.

It marks the chunk as a [relation break](format.md#tape-blocks) (`FLAG_RELATION_BREAK`), which readers see.

The break waits for the track's next chunk, also across cuts. A crash before that chunk drops the break, and the resumed
writer refuses again.

The writer marks a chunk that it encodes itself. A caller's block carries the flag (`mark_relation_break`) exactly when
a break waits, and is refused otherwise (`InputError::Flags`).
