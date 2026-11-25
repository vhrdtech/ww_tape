---
level: basic
---

# Beyond a torn block

Most crashes tear one block, which the [[resume]] of [When the power fails](crashes.md) cuts off. Some crashes leave
more behind: a group of puts half written, or a chain caught between two pieces. Damage may also look like a torn end.
What does a resume do then, and how does a [[producer]] know that its own resume works?

## A torn group of puts

Some puts only mean something together. A sensor's calibration has a table for each of its two axes and a version
that names both. A [[reader]] that saw the new table of one axis with the old one of the other would correct the
readings wrongly.

`TapeWriter::put_group` puts such [[state items]] as one group. The group below holds two tables of 40 KiB and their
version, too large for one [[block]] of puts:

```rust
--8<-- "ww_tape_tests/tests/it/crashes.rs:torn_group"
```

The writer marks every block of the group but the last with `FLAG_WITH_NEXT`, as it marks puts before a block of data.
The flags `FLAG_GROUP` and `FLAG_GROUP_START` say in addition that the blocks are a group and where it starts. A group
may also put or delete keys of a keyed [[state track]], such as the entries of a settings store.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/torn_group.svg"
<figcaption>A crash in the last block of a group. The two blocks before it are whole, but the group lacks its end, so
the resume drops them with the torn block. Given again, the group writes the bytes of a run without the
crash.</figcaption>
</figure>

A crash inside the group leaves marked blocks at the end of the piece and no last one. A reader of the piece before
the resume takes none of them, and a follower hands out none. The resume drops them and says so in
`Resumed::dropped_run`, so the application knows that the group is lost:

```rust
--8<-- "ww_tape_tests/tests/it/crashes.rs:torn_group_resume"
```

Every reader thus sees either the calibration before the group or all of the group, never a mix. The [format
reference](format.md#groups-of-puts) lists what a writer checks before it writes a group.

## When a resume refuses

A [[resume]] cuts off what a crash leaves and nothing more. A crash leaves a [[block]] that stops short, or zeros from a
sector boundary to the end. A disk writes a 512-byte sector whole or not at all, so nothing else is torn. Anything else
past the good blocks is damage, and the resume refuses and leaves the bytes as they are:

| Refusal | Why |
|---|---|
| [`ResumeError::Damaged`] | the bytes to cut hold damage, not a torn end: a whole block that fails its check |
| [`ResumeError::GoodBlockAfterCut`] | a good block lies past where the reader stopped, so cutting would lose it |
| [`ResumeError::LookSpent`] | the look for good blocks past that point ran out before the end of the bytes |
| [`InputError::Layout`], [`InputError::Producer`] | the options name another layout or producer than the file |

The second case is the one to know. Suppose a sector of the disk went bad in the drive's file a few blocks before the
end, and then the power failed. The [[reader]]'s scan stops at the bad sector, but good blocks follow it, so cutting
there would lose them. The resume names the first good block and refuses. The file still reads past the damage with
[`Recovery::Resync`], as [Reading past damage](reading_past_damage.md) explains.

A crash after the [[close]] needs no new data. A closed [[piece]] goes on after its close, and a close with nothing
written after it adds no byte. A crash in the middle of the close opens the piece again, and the next close writes the
end anew. The [format reference](format.md#resuming-after-a-crash) tells a [[torn end]] from damage byte by byte.

## Resuming a chain

The drive's run is long, so the recorder writes it as a [[chain]] of [[pieces]] in a directory: `00000000.wwt`,
`00000001.wwt` and so on. The power failed while it wrote piece 2. On restart the recorder calls [`chain::resume`] with
the directory, the [[owner id]], the chain number and its options.

The chain reads pieces 0 and 1 back from their [[tails]] and reopens `00000002.wwt` as a single file resumes. The
recorder then goes on from the first [[block]] the chain lacks, exactly as before. A crash at a piece boundary resumes
as well. It may come between closing a piece and creating the next, or inside the head of the next:

| How the last piece ended | A piece per file | All pieces in one file |
|---|---|---|
| unclosed | resumed | resumed |
| closed naming a next piece: the crash came during a cut | the next piece starts | the next piece starts |
| closed, naming no next piece | written on after its close | written on after its close |
| its head is damaged | refused | refused |

A chain therefore gains a piece only at a [[cut]]. A recorder that opens the chain again goes on in its last piece.

## Trying crashes in a test

A crash is easy to rehearse, because a [[resume]] works on any [[sink]]. A [`MemSink`] keeps a [[piece]] in memory. A
test writes a small piece there and copies each prefix of its bytes into a new sink. Each prefix is what a crash at that
byte would leave. It resumes the sink with [`TapeWriter::resume_on`], gives the lost calls again and compares the result
with the piece written whole:

```rust
--8<-- "ww_tape_tests/tests/it/crashes.rs:every_byte"
```

The crate's own tests cut pieces at every block boundary the same way, and a [[producer]] can test its own resumes so.
