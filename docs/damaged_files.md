# Reading a damaged file

A file can be damaged by a torn write, a bad sector or a reused card. A reader of such a file reads every good block it
can and reports the rest.

This page explains how a reader walks past damage, what a damaged file still gives, and what the format does not
protect against. It assumes [Damage and recovery](recovery.md), which describes the checksums.

## Reading past damage

`FileBlocks` walks the blocks of a byte stream in order. What it does at damage depends on its **recovery mode**, a
`Recovery` value passed to `FileBlocks::with_recovery`:

| Mode | At damage | Use |
|---|---|---|
| `Recovery::Prefix` (the default) | stops and reports where | reading what a crash left; the end of a file |
| `Recovery::Resync` | reports the damage and reads on | salvaging a damaged file |

### Prefix: the longest valid prefix

Prefix reads the longest valid prefix of the bytes. It stops at the first of these blocks:

- a block with a bad magic, length or cyclic redundancy check (CRC);
- a block cut short;
- a block whose sequence number goes backwards;
- a good block of another [session](raw_media.md#sessions-and-stale-data) where no session may start. Such a block is
  stale data.

Prefix reports where it stopped and why (`FileError::Cut`).

This is what a crash leaves: good blocks, then a torn one. For example, the writer of a file lost power halfway through
a 1 MiB chunk.

That file reads as every block before that chunk. One `FileError::Cut` at the chunk's offset follows them.

Not every bad stretch is the end. Bytes that are all 0x00 or all 0xFF are blank, and a medium may leave blank or bad
stretches between good data.

A blank or bad stretch is a **gap** when the session's data goes on after it. The stretch must also be no longer than
the layout's longest gap.

Prefix reports a gap (`FileError::Gap`) and reads on.

[Blank space and gaps](raw_media.md#blank-space-and-gaps) explains when a medium has gaps.

### Resync: reading on after damage

Resync reads on past damage. It reports a bad block (`FileError::Bad`) and looks for the next block magic one byte
after the bad block's start.

Any byte may start a block, also in a file aligned for chunk payloads. Resync therefore takes a block only when its
CRCs match.

Resync hands out a chunk with a good head and a damaged payload; it doesn't skip it. `Block::is_intact` is false, and
`Chunk::bad_segments` names the segments that failed.

The walk then goes on after the chunk by its length.

Resync reads every block Prefix reads. In a stretch that Prefix reads past as a gap, Resync hands out the good blocks it
meets.

These include a session that starts in the gap and the blocks after it. Past the gap, Resync goes on with the session
it had, as Prefix does.

#### Conflicts

Resync has no sequence number check, so a repeated sequence number needs another guard. Resync remembers the last
65536 data blocks it handed out (`RESYNC_SEQS_MAX`) by session and sequence number.

A good block may have the session and sequence number of one of them but other bytes. Such a block is a **conflict**
(`FileError::Conflict`, naming both offsets).

Resync never hands out a conflict as a block of its own. A copy with the same bytes is no conflict.

Resync hands out the first block of a conflicting pair before it meets the second one. An application that keeps what
a Resync walk hands out therefore needs one of two things:

- it holds blocks back since the last damage;
- it can take a block back.

### What a walk reports

Each report is a `FileError` variant:

| Report | Mode | Meaning |
|---|---|---|
| `Cut` | Prefix | the bytes from this offset on are left out, for the reason given; the walk ends |
| `Gap` | both | a blank or bad stretch the session's data goes on after; the walk goes on |
| `Replaced` | Prefix | a later copy of the block before it, with other bytes, replaces it ([Duplicates](raw_media.md#duplicates)) |
| `Bad` | Resync | bytes at this offset are not a good block; the walk skipped to the next magic |
| `Truncated` | Resync | the bytes end inside a block |
| `Conflict` | Resync | two good blocks with one session and sequence number but other bytes |
| `Stale` | Resync | a good block of another session where no session may start; skipped |
| `Refused` | both | a block of an integrity level this reader doesn't handle; not damage, stepped over by its length |

### Salvaging the items of a damaged events chunk

Inside a damaged events chunk, a reader can still **salvage** items: read every item it can and mark the rest
**suspect**. `EventTimes::salvage` and `Payloads::salvage` do this.

The chunk stores the time codec and the payload lengths per group of 128 items. Each group's record must agree with
its neighbours:

- its offsets chain;
- its ticks are in order;
- its widths are in range.

The reader marks a group that breaks the chain as suspect, and only that group's items.

The cost depends on where the damage lands:

- A flipped bit in one residual of the time column damages one item.
- A flipped bit in a group record damages one group or two.
- A payload column coded with lz4 is the exception. One bad byte makes every item of the chunk suspect, since the
  column decodes as a whole.

## What a damaged file still gives

The three cases below cover where damage lands in a file. Here a stretch is lost when it no longer reads: it was
overwritten or zeroed, or its sectors went bad.

### The first 4 KiB are lost

These bytes hold the piece head, the first definitions and perhaps the first data blocks.

A closed file opens from its tail as usual. The final index lists every block, and every block outside the lost
range reads.

Each definition comes from its newest record that reads. That record is one of these:

- a copy written at a use 1 MiB or more after the definition's first write;
- a put of the definition near a later block of its data.

The records and chunks inside the lost range are lost, and the content hash no longer matches.

The head named the previous piece in a chain, so the link back to that piece can't be checked. The link forward still
can.

An unclosed file with at least one checkpoint opens from its newest checkpoint, and the same holds.

An unclosed file may be shorter than one checkpoint interval (about 4 MiB). The reader scans such a file from its
start, and the scan stops at the damage.

`TapeReader` then lists nothing after the damage. A Resync walk reads every good block after it.

### The last 4 KiB are lost

These bytes hold the tail and the close, and often part of the final index. The reader finds no tail and reads the
file as unclosed.

The reader finds the newest checkpoint and scans up to the damage. Every data block before the damage reads.

The content hash and the link forward to the next piece are lost. The file reports as unclosed instead of complete.

### One block in the middle is lost

The cost depends on the kind of the lost block:

| Lost block | Cost |
|---|---|
| events chunk, payload damaged | the items of the groups it hits are marked suspect; the rest read |
| events chunk, head damaged | the chunk's items; the index still says which track, items and ticks they were |
| series chunk | one 64 KiB segment per hit with a good head; the whole chunk with a bad head |
| records block | its records; the index entry says how many, the gap in sequence numbers how many blocks |
| summary | nothing; a summary is derived from its track's data, and `ww_tape_summary` builds it again |
| definitions or puts | nothing once a copy or a later put of each of its items reads; until then, what it held |
| checkpoint index | in an unclosed piece, the piece up to the next good checkpoint is scanned instead |
| final index | in a closed piece, the lists of blocks: `TapeReader::entries` fails, while `track_entries` still reads a track whose list and the index head are good |

In a closed file a reader reads blocks by the final index. A lost block in the middle, other than the final index,
therefore costs that block alone.

In an unclosed file the same holds before the newest checkpoint. After it, the reader's scan stops at the lost block,
and a Resync walk reads what follows.

## Limits

These limits follow from the design. They fall into three groups.

### Limits of detection and salvage

- **No correction.** Checksums detect damage; they don't repair it. The parity block kind is reserved, and nothing
  writes or reads it.
- **A damaged chunk head costs the chunk.** The head CRC protects the payload's segment CRCs, not the head itself.
- **lz4 payload columns.** One bad byte makes every item of the chunk suspect.
- **Small shifts.** A flipped bit in a salvaged group's record may shift a first tick, slope or duration minimum too
  little to break an order. The reader then doesn't mark the group, and the group reads shifted.
- **The content hash needs a closed piece.** An unclosed or cut-off piece has no hash to check.
- **32-bit session tags.** Stale data matches the current session by chance with probability 2⁻³² per stale session.

### Limits of reading and resuming

- **Unclosed small files.** The reader scans a file without a checkpoint yet, under about 4 MiB, from its start. The
  scan stops at the first bad block, and only a Resync walk reads past it.
- **Definitions near the start.** A definition's copy is written at a use at least 1 MiB after its first write. An
  unclosed file shorter than that holds each definition once, so one bad sector can take it.
- **Chunks without definitions.** Chunks then still place themselves by track id, item number and tick. Without clock
  rates and item types, though, a reader can't turn ticks into seconds or bytes into values.
- **Lost state in a scanned stretch.** The state lookups (`TapeReader::state_at`, `definitions_at`) go by index
  entries. A damaged block where a scan stopped has no known kind and no entries.
- **The cut in a state lookup.** The piece reports such a block as its cut. The lookups take the newest record before
  it and name the stretch past the cut as one the index doesn't cover (`Lookup::uncovered`).
- **A damaged final index.** `TapeReader::entries` fails on a closed piece whose final index doesn't read;
  `track_entries` still gives each track whose own list reads. The reader doesn't use the piece's checkpoints instead.
- **A damaged stretch stops a resume.** A resume cuts only a torn end. Damage with good blocks behind it leaves the
  piece unclosed; a Resync walk reads it.

### Limits of media, codecs and clocks

- **Multi-level cell (MLC) paired pages.** A power cut on MLC flash can corrupt an earlier, already synced page.
  That page is paired with the page being programmed.
- **Paired pages are the medium layer's task.** Keeping unsynced writes away from synced pages is the medium layer's
  task, not the format's.
- **Bursty nanosecond times cost more than varints.** Random access costs about 1.4 bytes per event on such data ([What
  it costs](events_and_series.md#what-it-costs)).
- **Fine ticks need epochs.** A 64-bit tick at 1 ps spans about 106 days; longer captures are cut into clock epochs.
- **Reserved, not built.** These features have reserved numbers but no implementation yet:
    - a payload column holding one shrink_wrap `Series<T>`;
    - run-coded discrete samples;
    - struct samples with a column per field;
    - outer codecs on chunk columns;
    - integrity levels other than full checking;
    - preallocated space.
- **Reserved numbers are refused.** Today's readers refuse the reserved numbers, so files that use them later won't be
  misread.

## See also

- [Damage and recovery](recovery.md): checksums, and resuming after a crash.
- [Sessions and raw media](raw_media.md): sessions, stale data, gaps and duplicates.
- [Format reference](format.md#sessions-media-and-damage): sessions, media, gaps and state records as a reference.
