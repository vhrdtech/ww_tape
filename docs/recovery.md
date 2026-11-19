# Damage and recovery

Recordings fail in ordinary ways. Power goes out in the middle of a write, a cable is pulled, an SD card is reused
without being erased, or a sector goes bad on an old disk. A tape is built so that each of these failures costs the
bytes it touched and nothing else. A reader then says what was lost instead of guessing.

This page explains how the format finds damage, how a reader reads past it, and how a writer continues after a crash.
It closes with what the format does not protect against. It assumes the [overview](index.md) and refers to chunks from
[Events and series](events_and_series.md).

## Checksums at every level

Damage has to be found before it can be contained. A tape checks its bytes at four levels:

| Check | Covers | Damage costs |
|---|---|---|
| block CRC-32C | each whole block | that block |
| head CRC-32C | the head of a chunk or index block | the head alone; the block's place stays known |
| segment CRC-32C | each 64 KiB segment of a chunk's payload | one segment of one chunk |
| content hash, BLAKE3 | every byte of a finished piece before its finish block | nothing; it proves the piece is unchanged |

The head CRC matters most. A chunk's **head** gives the chunk's length, its track, its item numbers and ticks, and one
CRC per **segment**, a fixed stretch of the payload. When the payload is damaged but the head is good, a reader still
trusts the length, so it finds the next block. It also knows exactly which items were hit. A flipped bit in a 4 MiB
series chunk costs one 64 KiB segment of samples.

Writers use 64 KiB segments. The head states the segment size, and a reader takes any size from 4 KiB to 16 MiB.

Every CRC is CRC-32C. Writers and readers compute it with the CPU's CRC instructions where they exist: SSE4.2 on x86-64
and the CRC extension on aarch64. Elsewhere, such as in a browser or on a microcontroller, a table computes it. The
result is the same, so the choice never shows in the file.

The checks detect damage; they do not correct it. Correction is the medium's job: the error correction of a disk or a
flash controller, or a file system. The block kind `BlockKind::Parity` is reserved for parity blocks, but no writer
writes them.

## Reading past damage

`FileBlocks` walks the blocks of a byte stream in order. What it does at damage depends on its **recovery mode**, a
`Recovery` value passed to `FileBlocks::with_recovery`:

| Mode | At damage | Use |
|---|---|---|
| `Recovery::Prefix` (the default) | stops and reports where | reading what a crash left; the end of a file |
| `Recovery::Resync` | reports the damage and reads on | salvaging a damaged file |

### Prefix: the longest valid prefix

Prefix reads the longest valid prefix of the bytes. It stops at the first block with a bad magic, length or CRC, and at
a block cut short. It also stops at a sequence number that goes backwards, and at a good block of another session where
no session may start, which is **stale data** ([below](#sessions-and-stale-data)). It reports where it stopped and why
(`FileError::Cut`).

This is what a crash leaves: good blocks, then a torn one. For example, a file whose writer lost power halfway through
a 1 MiB chunk reads as every block before that chunk, followed by one `FileError::Cut` at the chunk's offset.

Not every bad stretch is the end. A blank or bad stretch no longer than the layout's longest gap, with the session's
data going on after it, is a **gap**: Prefix reports it (`FileError::Gap`) and reads on. [Blank space and
gaps](#blank-space-and-gaps) explains when a medium has gaps.

### Resync: reading on after damage

Resync reads on past damage. It reports a bad block (`FileError::Bad`) and looks for the next block magic one byte
after the bad block's start. Any byte may start a block, also in a file aligned for chunk payloads, so a block is taken
only when its CRCs match.

A chunk with a good head and a damaged payload is handed out, not skipped. `Block::is_intact` is false and
`Chunk::bad_segments` names the segments that failed. The walk then goes on after the chunk by its length.

Resync reads every block Prefix reads. In a stretch that Prefix reads past as a gap, Resync hands out the good blocks it
meets, also a session that starts there and the blocks after it. Past the gap, it goes on with the session it had, as
Prefix does.

Resync has no sequence number check, so a repeated sequence number needs another guard. Resync remembers the last 65536
data blocks it handed out (`RESYNC_SEQS_MAX`) by session and sequence number. A good block with the session and
sequence number of one of them but other bytes is a **conflict** (`FileError::Conflict`, naming both offsets). It is
never handed out as a block of its own. A copy with the same bytes is no conflict.

The first block of a conflicting pair was handed out before the second one is met. A caller that keeps what a salvage
hands out therefore either holds blocks back since the last damage or can take one back.

### What a walk reports

Each report is a `FileError` variant:

| Report | Mode | Meaning |
|---|---|---|
| `Cut` | Prefix | the bytes from this offset on are left out, for the reason given; the walk ends |
| `Gap` | both | a blank or bad stretch the session's data goes on after; the walk goes on |
| `Replaced` | Prefix | a later copy of the block before it, with other bytes, replaces it ([Duplicates](#duplicates)) |
| `Bad` | Resync | bytes at this offset are not a good block; the walk skipped to the next magic |
| `Truncated` | Resync | the bytes end inside a block |
| `Conflict` | Resync | two good blocks with one session and sequence number but other bytes |
| `Stale` | Resync | a good block of another session where no session may start; skipped |
| `Refused` | both | a block of an integrity level this reader doesn't handle; not damage, stepped over by its length |

### Salvaging the items of a damaged events chunk

Inside a damaged events chunk, a reader can still **salvage** items: read every item it can and mark the rest
**suspect**. `EventTimes::salvage` and `Payloads::salvage` do this. The time codec and the payload lengths are stored
per group of 128 items. Each group's record must agree with its neighbours: offsets that chain, ticks in order, widths
in range. A group that breaks the chain is marked suspect, and only its items are.

For example, a flipped bit in one residual of the time column damages one item. A flipped bit in a group record damages
one group or two. A payload column coded with lz4 is the exception: one bad byte makes every item of the chunk suspect,
since the column decodes as a whole.

## Writing so a crash costs little

`TapeWriter` writes a file the way databases write their logs. The file functions of the crate `ww_tape_fs`
(`writer::create`, `chain::create`) do this through a `FileSink`:

1. The sink creates the file as `NAME.partial` and syncs the file and its directory, so the name can't vanish in a
   crash.
2. The writer syncs the data by a `SyncPolicy`: never, every N blocks, or at most an interval apart (1 s by default).
   What a crash can lose is bounded by the policy.
3. At the finish the writer writes the final index, the finish block (padded so the piece ends on 4 KiB) and the tail.
   The sink then renames the file to `NAME` and syncs the directory again.

A failed write or sync ends the file. The writer reports it and refuses every later write (`WriteError::Failed`). It
never retries, because after a failed `fsync` Linux may drop the unwritten pages, and a second `fsync` then succeeds
without them. A file that still ends in `.partial` was never finished.

A file has one writer at a time. A file sink holds an exclusive lock on the lock file `NAME.lock` beside it, and a
second writer of the same file gets `FsError::Locked`. The lock belongs to the writer's process, so a crash releases
it: the lock file a crash leaves behind holds no lock, and the next writer takes it.

### Resuming after a crash

A **resume** reopens an unfinished piece and continues it as if the crash never happened. `ww_tape_fs::writer::resume`
takes the path `NAME` and the writer options, and reopens `NAME.partial`. `TapeWriter::resume_on` does the same on any
other sink, such as a `MemSink`. Calling `writer::create` again instead fails: it refuses a `.partial` file that holds
bytes (`FsError::Exists`).

The resume reads the piece the way `TapeReader` does. It starts from the newest checkpoint and scans the blocks after
it up to the first bad block. It keeps every good block up to the end of the piece's data and cuts off the rest:

- a torn block, the last one the crash interrupted;
- the end of a piece (final index, finish block) when the crash came inside it;
- trailing puts marked `FLAG_WITH_NEXT`, which belong with a block that didn't survive.

The resume then replays every block it keeps, so the writer knows again what it put and where. A damaged block that the
index still lists stays in the piece. It counts as a newer record of each slot it held, item unknown, so the next put
of such a slot writes the item again.

The producer then gives the calls again, from the block that `TapeWriter::next_id` names on. It passes the same writer
options it wrote the piece with (`TapeWriter::options` returns them) and the same definitions. The resumed piece then
ends with the same bytes, and the same content hash, as one written without the crash. `Resumed`, returned beside the
writer, says how many bytes were kept and dropped and where the reader stopped.

The writer holds no definitions of its own, so a resume needs none from the file. The application passes its
definitions with each call, as before the crash. The writer puts what each block needs again wherever the same item
doesn't lie close behind.

Two cases need no new data:

- A piece whose finish and tail read whole is complete: the crash came after the finish, before the rename. The resume
  cuts off junk after the tail, renames the piece, writes nothing and returns `WriteError::Ended`.
- A piece cut inside its end is opened again. Its final index and finish block are cut off, and the next finish pads
  the piece to 4 KiB again.

### When a resume refuses

A resume cuts off a torn end and nothing more. Where cutting would lose good data, or where it can't tell, it refuses
and leaves the bytes as they are:

| Error | Why |
|---|---|
| `ResumeError::NoPiece` | there is no unfinished piece; `ww_tape_fs` reports `FsError::NoPiece` with the path |
| `ResumeError::GoodBlockAfterCut { offset }` | a good block of the piece lies past where the reader stopped |
| `ResumeError::LookSpent { offset }` | the look for good blocks ran out before the end of the bytes |
| `ResumeError::HashWalkSpent { end }` | the content-hash walk of the bytes kept spent its budget; new blocks would not verify |
| `InputError::Layout`, `InputError::Producer` | the options name another layout or producer than the piece |

For example, a sector goes bad in the middle of the stretch after the newest checkpoint, and the power then fails.
The reader's scan stops at the bad sector, but good blocks follow it. Cutting there would lose them, so the resume
refuses with `GoodBlockAfterCut`, naming the first good block. A Resync walk still reads every good block of the file.

The look past where the reader stopped is bounded. It reads at most 128 MiB (`RESUME_LOOK_MAX`), twice the longest
block, and checks at most 4 places whole (`RESUME_PROBES_MAX`). Crafted bytes therefore cost a bounded read, and a
resume that runs out refuses with `LookSpent` instead of cutting.

The resume checks the options against the piece before it cuts or writes anything. The newest layout and producer the
piece names must be the ones the options give; each error names both. Options with another chunk alignment would place
blocks the way the file says it doesn't, and a reader trusts the layout when it looks for the end of the data. The
layout's gap settings need no check, because the writer never leaves a gap.

A damaged block the resume keeps leaves the piece checkable. The finish block marks the hash of each track whose data
that block may hold as incomplete (`TrackRange::incomplete`), and the piece still verifies.

### Resuming a chain of pieces

`ww_tape_fs::chain::resume` resumes a chain of pieces in a directory, and `PieceChain::resume_on` on any store of
tapes. The chain reads its finished pieces back from their tails and resumes the unfinished last one.

For example, a recorder loses power while writing piece 3 of a chain. On restart it calls `ww_tape_fs::chain::resume`
with the directory, the owner, the chain number and the chain options. The chain reads pieces 0 to 2 back from their
tails, reopens `00000003.wwt.partial`, and the recorder goes on from the first block the file lacks.

Two crashes at a piece boundary resume too:

- A crash between finishing one piece and creating the next: the chain creates the next piece, as the cut would have.
- A crash inside the head of the next piece: the piece starts again.

A chain of a file per piece whose last piece ended complete or aborted is refused with `WriteError::Ended`. A chain
kept in memory from which `MemTapes::remove` took pieces is not resumed.

A chain written into one file (`PieceTapes::One`) resumes the same way. The chain finds the pieces by their tails from
the end of the file. It then resumes the unfinished last piece behind the finished ones, or starts a new piece when the
last piece is finished. The finished pieces keep every byte. A last piece whose head is damaged is refused
(`WriteError::Index`): keeping its blocks would leave them outside the chain, and dropping them would lose more than a
torn end.

### Testing crashes in memory

A `MemSink` keeps a piece in memory, and its tape (`MemSink::tape`) reads back as a byte source. A test writes a piece,
cuts the bytes at any offset, and resumes or reads what is left. The tests cut pieces at every block boundary and at
bytes in between, and check that each resumes to the bytes of an uninterrupted write. [Writing a tape](writing.md)
describes the sinks and stores of tapes.

### Example: a recorder that lost power

A recorder on a PC writes a tape of several tracks, 50 MiB so far, with the default sync interval of 1 s. The power
fails. On disk is `capture.wwt.partial`: every block up to the last sync, and perhaps part of the block being written.

A reader opens it. The file has no tail, so the reader looks backwards from the end for the newest checkpoint index,
which is at most about 4 MiB back. From that checkpoint it follows the chain of checkpoints back and so lists every
block before it, without reading them. It then scans the blocks after the newest checkpoint, one by one, and stops at
the torn block, which fails its CRC. The reader reports the piece as unfinished, cut at that offset (`PieceInfo::cut`).

The recording loses the torn block and whatever the writer had not yet synced: at most about one second of data.
Everything else reads, including the definitions and the summary blocks written so far. The content hash can't be
checked, because the finish block was never written. That is how a reader knows the file was not closed.

When the recorder starts again, it calls `ww_tape_fs::writer::resume` with `capture.wwt` and its writer options. The
resume cuts off the torn block and returns the writer. The recorder gives its calls again from the first block the
file lacks, and the finished `capture.wwt` reads as if the power had never failed.

## Sessions and stale data

On a file, the end of the data is the end of the file. On a raw medium, such as an SD card or raw NAND flash written
without a file system, it is not. The card may hold an older, longer recording behind the current one, and that
recording's blocks are as valid as the new ones.

A **session** is one writer's unbroken append: a producer start, a device boot. Every block of a session carries the
same 32-bit **session tag** in its header, and its sequence numbers count from 0. A writer picks the tag at random or
from a seed, or derives it from the piece id when the same input must give the same bytes. A tag is never 0 nor
FFFFFFFFh, so a blank header never names a session.

A reader keeps to one session at a time. A good block of another session is stale, unless a new session may start at
that block. Where a new session may start depends on the **medium**, a `Medium` value the reader is opened with:

- In a file (`Medium::File`), a new session may start at a records block that starts with a definition (another
  producer appended its blocks) or at any piece head (files concatenated).
- On a raw medium (`Medium::Raw`), a new session starts only at a piece head that names the session just ended as its
  previous session. Stale data from an earlier recording names some other session and is never taken, even when it
  starts with a piece head of its own.

A block is judged by its session before its format version. Stale data written by an older firmware in another format
is therefore the end of the data, like any other stale data, not a piece of that format.

A raw medium is therefore read forward, from its start or from a hint the medium layer gives. It is never read from its
end, where a stale tail could pass for the newest piece.

A finished piece is a multiple of 4 KiB long on a raw medium too, so a piece that starts on a 4 KiB boundary ends on
one. The layout's `gap_max` and `resume_align` still say where a writer goes on after a blank or bad stretch. A piece
appended after a finished one starts on a resume place whenever `resume_align` divides 4 KiB, as a 512-byte sector
does.

### Blank space and gaps

**Blank** bytes are all 0x00 or all 0xFF: what an erased or preallocated medium reads, depending on its kind. No block
starts with either, so a blank stretch running to the end of the file or medium is the end of the data.

A blank or damaged stretch in the middle is not always the end. A NAND writer whose page program failed moves on to the
next erase block, and a writer padding a NAND page at a sync leaves 0xFF behind. The **layout**, a record among the
definitions, says how far a writer may skip and where it resumes:

| Medium | Longest gap (`gap_max`) | Resumes at a multiple of (`resume_align`) |
|---|---|---|
| file (`Layout::UNALIGNED`) | 0: the first bad stretch is the end | 1 byte |
| SD card or eMMC (`Layout::on_sd_card`) | 4 MiB | 512 bytes, a sector |
| raw NAND (`Layout::on_raw_nand`) | two erase blocks | one page |

Past a blank or bad stretch, a reader reads one block header at the first non-blank byte and at each resume place
within the longest gap. A good block of the current session that goes on with the data there makes the stretch a gap,
reported with its length, and the walk goes on. When there is none, the stretch is the end of the data.

The format bounds what a reader pays per gap. A valid layout allows at most 64 MiB of gap with at most 8192 resume
places, and a reader checks at most 4 whole blocks per gap. On a raw medium, blank space longer than the gap is the end,
and nothing after it is read: an erased 64 GB card costs one gap's worth of header reads. A Resync walk looks once per
stretch too, and only up to where Prefix ends. It reads up to one gap and a block ahead, 128 MiB at most
(`RESYNC_AHEAD_MAX`).

### Duplicates

A writer that retried a write may leave the same block twice. The same session and sequence number twice is a
**duplicate**, not a cut. With the same bytes, Prefix drops the second copy. With other bytes, Prefix takes the later
copy and reports the replacement (`FileError::Replaced`). Resync reports such a pair as a conflict instead, because in
damaged bytes it can't tell a copy from a rewrite.

### Example: a logger card recorded twice

A data logger records to an SD card as a raw medium. Its first recording, session A, filled 100 MiB. The card is
reused: session B writes 30 MiB from the start, then the power fails during a write.

A reader walks forward from the start of the card. It takes B's blocks up to the torn one, then probes for B's next
block within 4 MiB. It finds only A's old blocks, which carry A's session tag and don't start with a piece head naming
B. They are stale, so the data ends at the torn block, and none of A's 70 remaining MiB is mistaken for B's.

When the logger boots again, it finds the same end of data and writes its next session after it, starting with a piece
head that names B as the previous session. That session, C, is taken by the next reader; A's leftovers beyond C stay
stale.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/sessions.svg"
<figcaption>The card of the example. Above, after the power failure: B's blocks end at the torn one, and A's older
blocks behind it carry another session tag. Below, after the next boot: session C starts with a piece head naming B,
so a reader goes on into C and stops at A's leftovers again.</figcaption>
</figure>

## What a damaged file still gives

The three cases below cover where damage lands in a file. "Lost" means unreadable: overwritten, zeroed or bad sectors.

**The first 4 KiB are lost.** These hold the piece head, the first definitions and perhaps the first data blocks.

- A finished file opens from its tail as usual. The final index lists every block, and every block outside the lost
  range reads. Each definition comes from its newest record that reads: a copy written at a use 1 MiB or more after
  its first write, or a put of it near a later block of its data. The records and chunks inside the lost range are
  lost, and the content hash no longer matches. The link back to the previous piece in a chain can't be checked, since
  the head named it; the link forward still can.
- An unfinished file with at least one checkpoint opens from its newest checkpoint, and the same holds.
- An unfinished file shorter than one checkpoint interval (about 4 MiB) is scanned from its start, and the scan stops at
  the damage: `TapeReader` lists nothing after it. A Resync walk reads every good block after the damage.

**The last 4 KiB are lost.** These hold the tail and the finish, and often part of the final index. The reader finds no
tail and reads the file as unfinished: it finds the newest checkpoint and scans up to the damage. Every data block
before the damage reads. The content hash and the link forward to the next piece are lost, and the file reports as
unfinished instead of complete.

**One block in the middle is lost.** The cost depends on its kind:

| Lost block | Cost |
|---|---|
| events chunk, payload damaged | the items of the groups it hits are marked suspect; the rest read |
| events chunk, head damaged | the chunk's items; the index still says which track, items and ticks they were |
| series chunk | one 64 KiB segment per hit with a good head; the whole chunk with a bad head |
| records block | its records; the index entry says how many, the gap in sequence numbers how many blocks |
| summary | nothing; a summary is derived from its track's data, and `ww_tape_summary` builds it again |
| definitions or puts | nothing once a copy or a later put of each of its items reads; until then, what it held |
| checkpoint index | in an unfinished piece, the piece up to the next good checkpoint is scanned instead |
| final index | in a finished piece, the lists of blocks: `TapeReader::entries` fails, while `track_entries` still reads a track whose list and the index head are good |

In a finished file a reader reads blocks by the final index, so a lost block in the middle, other than the final index,
costs that block alone. In an unfinished file the same holds before the newest checkpoint. After it, the reader's scan
stops at the lost block, and a Resync walk reads what follows.

## What tape and flash media taught the format

Tape drives and flash memory solved these problems long before; the format takes their answers where they fit. Each
point below names its source.

- **A tag per write session.** LTO marks every data set with a write pass number that grows whenever data is
  overwritten, so a reader tells current data from residue (ECMA-319, §13.5.16). UBI stamps an image sequence number on
  every eraseblock for the same reason (Linux `ubi-media.h`). The session tag does this job. It is random instead of a
  counter, because a wiped card or a device without stored state has no counter to continue.
- **Nothing after the end is data.** LTO declares data recorded after the end-of-data set inaccessible and invalid
  (ECMA-319, §13.4.3). The format has no end-of-data block, since a crash leaves none; blank space and the session tag
  mark the end instead.
- **Erased is not always zero.** NAND erases to 0xFF (Micron TN-29-19). An SD card reads 0 or 1 after an erase, by
  vendor (SD Physical Layer Simplified Specification 9.10, §4.3.5.1), and eMMC reports which (JESD84-A441). So blank is
  0x00 or 0xFF.
- **A blank unit isn't the end.** A NAND writer that hits a failed program moves on and leaves erased pages behind;
  UBIFS looks past such units when it scans for the end of written data (Linux `fs/ubifs/scan.c`, `recovery.c`). The
  layout's gap and resume alignment say how far to look. Sizes follow the media: SD allocation units, up to 64 MB (SD
  9.10, §4.13.1), and NAND erase blocks and pages (ONFI 6.0, §3.1).
- **Program a NAND page once.** Programming the same part of a page twice without an erase gives undefined contents
  (ONFI 6.0, §6.7.1.25), and Micron's MLC example allows one program per page (Micron TN-29-19). A writer on raw NAND
  pads the rest of the page at each sync, and readers take the padding as a gap.
- **Rewrites: the later copy wins.** LTO rewrites an interrupted data set in its entirety, and the later write counts
  (ECMA-319, §15.4). The same block twice in a tape is a duplicate, and the later copy is taken.
- **Indexes that point at themselves and back.** An LTFS index carries a self pointer and a pointer to the index before
  it, and an index whose self pointer doesn't match is taken as data (LTFS Format Specification 2.5.1, §5.4.2, §5.4.3).
  A tape's index blocks carry the same two offsets, and a reader checks the self offset against where it found the
  block.
- **Never grow incompressible data.** SLDC switches to a scheme that copies bytes, so incompressible data grows by about
  0.05 % instead of 12.5 % (ECMA-321, §7.2). A writer that codes a block's body keeps the plain body when coding doesn't
  shrink it.
- **Correction belongs to the medium.** ONFI leaves error correction to the host controller, per page (ONFI 6.0,
  §6.7.1.26). A tape only detects what that correction missed.

## Limits

These limits follow from the design:

- **No correction.** Checksums detect damage; they don't repair it. The parity block kind is reserved, and nothing
  writes or reads it.
- **Unfinished small files.** A file without a checkpoint yet, under about 4 MiB, is read by a scan from its start,
  which stops at the first bad block. Only a Resync walk reads past it.
- **Definitions near the start.** A definition's copy is written at a use at least 1 MiB after its first write. An
  unfinished file shorter than that holds each definition once, so one bad sector can take it. Chunks then still place
  themselves by track id, item number and tick. Without clock rates and item types, though, a reader can't turn ticks
  into seconds or bytes into values.
- **Lost state in a scanned stretch.** The state lookups (`TapeReader::state_at`, `definitions_at`) go by index
  entries. A damaged block where a scan stopped has no known kind and no entries. The piece reports it as its cut, and
  the lookups take the newest record before it and name the stretch past the cut as one the index doesn't cover
  (`Lookup::uncovered`).
- **A damaged final index.** `TapeReader::entries` fails on a finished piece whose final index doesn't read;
  `track_entries` still gives each track whose own list reads. The piece's checkpoints are not used in its place.
- **A damaged chunk head costs the chunk.** The head CRC protects the payload's segment CRCs, not the head itself.
- **lz4 payload columns.** One bad byte makes every item of the chunk suspect.
- **Small shifts in a salvaged group.** A flipped bit in a group record that moves a first tick, slope or duration
  minimum by too little to break an order goes unmarked: the group reads shifted.
- **A damaged stretch stops a resume.** A resume cuts only a torn end. Damage with good blocks behind it leaves the
  piece unfinished; a Resync walk reads it.
- **32-bit session tags.** Stale data matches the current session by chance with probability 2⁻³² per stale session.
- **MLC paired pages.** On MLC NAND, a power cut can corrupt an earlier, already synced page paired with the one being
  programmed. Keeping unsynced writes away from synced pages is the medium layer's task, not the format's.
- **The content hash needs a finished piece.** An unfinished or cut-off piece has no hash to check.
- **Bursty nanosecond times cost more than varints.** Random access costs about 1.4 bytes per event on such data ([What
  it costs](events_and_series.md#what-it-costs)).
- **Fine ticks need epochs.** A 64-bit tick at 1 ps spans about 106 days; longer captures are cut into clock epochs.
- **Reserved, not built.** A payload column holding one shrink_wrap `Series<T>`, run-coded discrete samples, struct
  samples with a column per field, outer codecs on chunk columns, integrity levels other than full checking, and
  preallocated space. Today's readers refuse the reserved numbers, so files that use them later won't be misread.

## See also

- [Format reference](format.md#sessions-media-and-damage): sessions, media, gaps and state records as a reference.
- [Writing a tape](writing.md): the writer, its sinks and stores of tapes, and the producer's side of a resume.
