# ww_tape features and roadmap

This file is the one place where the status of ww_tape's features is kept. It lists what works, what is being built and what is planned.

## How to use this file

- **Status** of each item:
  - ✅ done
  - 🚧 in progress or partly done (the item says what is missing)
  - 📋 planned
  - 💡 idea, not committed to
  - ⛔ blocked (the item says on what)
  - 🔍 probably done or obsolete, to check before closing
  - 🧪 done and tested on the host, still to check on hardware
  - 🐛 known bug, not yet fixed (the item says how to hit it)
- **Target** is the release an item is planned for. `v0.5` is WireWeaver's next release; no tag means not scheduled.
- **IDs** carry their area's prefix (`FMT-1`, `WRITE-6`) and are stable: never renumber or reuse one. A new item takes the next free number of its area.
- The former `STD` and `TAPE` ids are listed under [Former ids](#former-ids).
- Use the ID in commit messages, CHANGELOG entries and code comments such as `// TODO(FMT-1): ...` in the source.
- When talking to people, name an ID with a short slug from its title, such as `FMT-4 index-close-tail`, never bare.
- IDs of other areas (`TR`, `API`, `INT`) belong to [WireWeaver](https://github.com/vhrdtech/wire_weaver). So do `ww_tap`, `ww_tape_bundle` and `tests/tape_identity`.
- Each area starts with a line on what it covers. Done items come first, then open ones by target.
- An item has a short heading, then a few plain lines on what works today and where to read more. History stays in git and the CHANGELOG.
- When work finishes, update its item in the same change: mark it ✅ and add a pointer to a page, crate or test. Never delete a done item.
- An item that turns out obsolete moves to [Dropped and superseded](#dropped-and-superseded) with a one-line reason.
- A new idea goes into its area with 💡 as its status. Small gaps in the code stay as `TODO` comments; only gaps that limit users or block a feature get an item.

## Format (`FMT`)

The bytes of blocks, records, chunks, definitions and the index, in the crate `ww_tape`.

- ✅ **FMT-1 Tape blocks and records** · `v0.5`
  - A tape is a run of blocks. Each block has a 32-byte header, records and a CRC-32C, and is never padded.
  - A record holds a track, a time delta, a length and the item bytes.
  - A block of a later version or of an unknown kind is checked and skipped by its length, not taken for damage.
  - Nothing in a block knows its producer. WireWeaver's link tap (TR-14) writes tapes, and eventfull uses them as its file format (CORE-19).
  - Open: splitting a recording into files by size or time, and preallocation. Zeros at the end already read as the end.
  - Read more: [Blocks](docs/blocks.md#blocks). Tests cut and flip every byte; fuzz target `tape-reader`.
- ✅ **FMT-2 Clocks, clock maps and typed items** · `v0.5`
  - A record's time is a signed 64-bit tick count of its track's clock. Times are deltas per clock, so a block reads alone and holds no wall time.
  - A clock has a stable key and an exact rate and resolution. A clock map turns the ticks of one clock into another's, with a named rounding.
  - A typed item carries any WireWeaver value by its type. A track names its clock, its dimensions and its item type.
  - Read more: [Clocks and ticks](docs/records_and_clocks.md#clocks-and-ticks).
- ✅ **FMT-3 Chunk blocks** · `v0.5`
  - A chunk block holds one stretch of one track's events or samples, read in place. Its head places it without an index.
  - The head has its own CRC, and the payload has a CRC for every 64 KiB. A flipped bit in a series chunk loses one segment.
  - An aligned file puts every chunk payload on a 4 KiB boundary. Small blocks fill the gaps before them.
  - Read more: [Chunks from columns](docs/columns.md). Golden file `v1-chunks.hex`; fuzz target `tape-chunk`.
- ✅ **FMT-4 Index, close and tail** · `v0.5`
  - A checkpoint index every 4 MiB lists the blocks since the one before, so a crash costs at most the open chunk.
  - A closed piece ends with a final index sorted by track, a close block and a fixed tail found from the end of the file.
  - The close holds a BLAKE3 hash of the piece's content. Offsets count from the piece, so files placed back to back keep their indexes.
  - Read more: [Index, close and tail](docs/format.md#index-close-and-tail). Golden file `v1-index.hex`; fuzz target `tape-index`.
- ✅ **FMT-5 Track identity by name path** · `v0.5`
  - A track's key hashes the names of its resource, array indices and the keys of keyed arrays. Two runs that number things differently get the same ids (API-67).
  - A resource renamed with `#[moved_from]` keeps its key (INT-17). A resource moved to another trait still gets a new key.
  - The bundle side is WireWeaver's crate `ww_tape_bundle`.
  - Read more: [Track identity](docs/format.md#track-identity). Tests in WireWeaver's `tests/tape_identity`.
- ✅ **FMT-6 Series tracks** · `v0.5`
  - A track is either events or a series. A series track states the type of one value, its period in ticks per sample and the scale of the stored numbers.
  - The period is an exact ratio. A batch of fixed-point values becomes chunk columns without a float.
  - A series track has no records: each chunk carries its own grid and scale (FMT-10).
  - Read more: [Series](docs/series.md). Golden file `v1-series.hex`; fuzz target `tape-series`.
- ✅ **FMT-7 Sessions, raw media and duplicates** · `v0.5`
  - Every block carries a session tag, and seq numbers count per session. Stale blocks of another session end the data.
  - Blank bytes are 0x00 or 0xFF. A blank or damaged stretch that the session's next block follows is a gap, not the end.
  - The same block twice is a duplicate: the same bytes are dropped, and with other bytes the later one wins and is reported.
  - Records and definition bodies may be stored or lz4. Codec and element numbers kept for later are refused today.
  - Read more: [Where the data ends on a raw card](docs/raw_media.md).
- ✅ **FMT-8 Closed pieces end on 4 KiB** · `v0.5`
  - A closed piece is a multiple of 4 KiB long. Pieces clone back to back into one file on btrfs and XFS with nothing between them.
  - The writer pads the close block, which states the padding. Blocks inside a piece stay unaligned.
  - Read more: [The 4 KiB end and cloning](docs/pieces.md#the-4-kib-end-and-cloning).
- ✅ **FMT-9 Definitions and state near their use** · `v0.5`
  - Definitions are plain records on track 0: the producer, the layout, bundles, clocks, maps, tracks and typed items. Each has a slot, and the newest per slot counts.
  - The application holds its definitions and passes them with each write. Data of a track it doesn't hold is refused.
  - A state item is put again only where the same item doesn't lie close behind. A first write gets spaced copies.
  - Adding 10 000 tracks one by one takes 60 milliseconds in all. A 256 MiB piece with 10 000 tracks in use holds 1.85 % puts and copies.
  - The index lists state records, and the reader looks state up at any place, damage included.
  - Read more: [Definitions on track 0](docs/blocks.md#definitions-on-track-0), [State tracks](docs/tape.md#state-tracks-and-near-use). Fuzz target `tape-state-hostile`.
- ✅ **FMT-10 Series chunks carry their own grid and scale**
  - A series chunk's head carries its grid and scale, so a chunk times and scales its samples alone.
  - A timeline of a track is built from chunk heads in any order. It checks each chunk against its neighbours and says where the gaps are.
  - Building the timeline reads only the heads: 335 bytes per 1 MiB chunk, unaligned.
  - Read more: [Grids](docs/series.md#grids-where-samples-are-in-time). Fuzz targets `tape-series` and `tape-series-hostile`.
- ✅ **FMT-11 Track relations**
  - A track's items can relate to another track's items by name, such as a source, a parent or a context. The writer gives the meaning, and ww_tape gives the blocks.
  - Stored values are a chunk column. A relation that never decreases is searched by lower bound, and the writer keeps it in order across cuts and resumes.
  - A caller may accept a break in such a relation. The tape marks it, and readers refuse a decrease that isn't marked.
  - Kept for later, not built: postings and dictionary blocks and values stored only in the index.
  - Read more: [Relation columns](docs/format.md#relation-columns). Tests `relations.rs`; fuzz targets `tape-times` and `tape-index`.
- ✅ **FMT-12 What a track's items are**
  - A track's definition says what its items are (`Items`): a WireWeaver stream's values, a ShrinkWrap type, plain bytes, numbers, or a foreign format. A foreign format has a name, a media type and a version, and may carry a schema in the tape. Numbers are of an element the definition describes. The bytes are stored as given.
  - `Items` is evolvable: a variant a newer ww_tape adds reads as unknown with the rest of the definition intact. A relay stores such events as given.
  - A series' items are `Numbers` too: any width LSB or MSB first, floats of another layout, a zero offset that summaries keep exact.
  - Read more: [What a track's items are](docs/tracks.md#what-a-tracks-items-are), [Items of a track](docs/format.md#items-of-a-track), [Sample codecs](docs/format.md#sample-codecs). Tests `raw.rs`, `iq8.rs`, `series.rs`; fuzz targets `tape-series`, `tape-samples`, `tape-samples-hostile`.

## Writer (`WRITE`)

Writing pieces and chains, resuming them, sinks in memory, in the crate `ww_tape`.

- ✅ **WRITE-1 Pieces and chains** · `v0.5`
  - One recording is cut into complete pieces at chunk boundaries. A cut happens by bytes, chunks or age, and never leaves a piece empty.
  - Each piece names the one before by its id and content hash. A chain check reports gaps, broken links, an open end and a lost first piece.
  - Read more: [Anatomy of a piece](docs/anatomy.md), [Pieces and chains](docs/pieces.md).
- ✅ **WRITE-2 Resume after a crash** · `v0.5`
  - A writer reopens an unclosed piece and keeps every block up to the last one written. The resumed piece ends with the bytes of a write that never stopped.
  - A chain resume also reads the closed pieces back. It handles a crash between a cut's close and the next piece.
  - Writers write through a sink and chains through a store of tapes, so the same code writes files and memory.
  - Read more: [When the power fails](docs/crashes.md). Tests cut pieces and chains at every block and at random bytes; fuzz target `tape-resume`.
- ✅ **WRITE-3 Writes say where they landed**
  - Each write of a chunk or a block returns the offset and length where the block landed. A chain also returns the piece it landed in.
  - Tests read each returned offset back (`placed.rs`).
- ✅ **WRITE-4 Writer checks in proportion to their cost**
  - The writer checks the head of a chunk it encodes, not the encoded bytes. A caller's own block is read once with its CRCs checked.
  - On a 1 MiB series chunk this costs 466 µs past the encoding, down from 857 µs.
  - Read more: [What the writer checks](docs/writing.md#what-the-writer-checks).
- ✅ **WRITE-5 Pieces in memory**
  - A closed piece can be taken out of memory, and readers holding it read on.
  - A memory sink reserves room for a whole piece, so a reader waits at most 0.16 ms while a 256 MiB piece is written.
  - Open: resuming a chain in memory whose first pieces were taken out.
  - Read more: [Reading while the writer writes](docs/live_feeds.md#reading-while-the-writer-writes).
- ✅ **WRITE-6 Pieces in one file, track owners and hashes**
  - A chain writes a file per piece or all pieces into one file. Each session that opens the file adds a piece.
  - A sink is the only writer of its bytes. A second writer is refused.
  - Tracks of one identity are told apart by an owner. A running hash per track is the same alone or interleaved, in one piece or many.
  - Read more: [Writing a chain into one file](docs/pieces.md#writing-a-chain-into-one-file), [Track owners and hashes](docs/track_hashes.md). Fuzz target `tape-resume`.

- ✅ **WRITE-7 A piece closes more than once**
  - A close ends a write session, not the piece. A writer resumed on a closed piece appends after its tail and closes again.
  - Nothing before the append moves. Each close hashes the close before it and the bytes since, so each close still checks its prefix.
  - Readers take the last close whose tail is whole. Bytes after it read as an unclosed tail, never as damage.
  - A resume cuts back only a torn end and refuses damage. A close with nothing appended writes no byte.
  - Read more: [Reading a growing piece](docs/growing_pieces.md), [The close in force](docs/format.md#the-close-in-force). Fuzz target `tape-reclose`.

- ✅ **WRITE-8 Pieces removed on purpose**
  - A close names pieces of its chain removed on purpose: sequence number, id and close hash, at most `REMOVED_PIECES_MAX` (65536).
  - `TapeWriter::record_removed` appends them to the chain's last piece after its close; every later close of the piece names them again.
  - `check_chain` links over the hole (`ChainLink::Removed`) when both ends match the rows. A head cut the rows cover reads as `head_removed`.
  - Rows live in the piece that holds them: removing that piece later means naming them again (`ChainReport::removed`).
  - Read more: [Pieces removed on purpose](docs/pieces.md#pieces-removed-on-purpose). Golden `v1-removed.hex`.

- ✅ **WRITE-9 A new piece carries its quiet tracks' definitions**
  - A cut puts the definition of every track the new head carries, with its clocks, maps and bundles, right after the head. Dropping older pieces keeps a quiet track's kind, clock and label.
  - Blocks of at most 4 KiB, each with the clocks and maps of its own tracks: a damaged block costs only its tracks. Bundles come once, first.
  - A call whose definitions fit no piece under the index policy is refused (`InputError::CarriedEntries`), nothing written, no cut. A resume that kept only the head puts them with the next call.
  - A new piece carries every definition while the chain's tracks fit the index policy's state entries. A cut is never refused for them.
  - Past the room it carries each once: the call's tracks first, then clocks and maps, then the newest tracks. The tracks with the oldest data are left out: the head marks them, the close counts them and a reader names them (`DefinitionsAt::not_carried`).
  - A cut made for a call keeps room for its records. An admitted call so fits the piece a cut made for it: data on a track left out writes on, and that piece holds data.
  - A state track gets a head row from its first put. A cut carries its newest item beside its definition, so a state put once outlives older pieces. About 275 to 330 bytes per carried state (`just bench-carried`).
  - What carrying costs a piece, measured by `just bench-carried`: [What they cost](docs/carried_definitions.md#what-they-cost).
  - Read more: [The definitions a new piece carries](docs/carried_definitions.md). Tests `quiet.rs`.

- ✅ **WRITE-10 A piece's layout and producer survive damage to its first blocks**
  - Every call that puts a definition or a state item uses the layout and the producer. Each gets track 0's copy at the copy spacing (1 MiB) and is put again past `near`, in that call's block of puts.
  - Losing a piece's first blocks loses neither: the reader takes the newest record. A scan past the newest checkpoint takes its gap rules from the copy.
  - A raw-media walk that starts past lost first sectors knows no gap rules until the copy; `OpenOptions::layout` gives them there.
  - A copy that finds the piece's state entries full is skipped, never refusing a call; such a piece names them once.
  - Cost: 523 bytes in a 28 MiB piece of the drive, two records and their index entries; none in a piece under 1 MiB.
  - Read more: [When bytes go bad](docs/damage.md#the-first-4-kib-lost). Tests `head_copies.rs`.
- 🚧 **WRITE-11 The definitions number tracks and clocks**
  - An application defines a track or a clock without a number (`TrackSpec`, `ClockSpec`); `Definitions` gives the lowest number not taken and returns it.
  - The same track identity (key, dimensions, owner) or clock key gets its number back, also after a close.
  - A number never goes to another track or clock.
  - A resume reads the numbers of the piece's definitions (`Resumed::numbers`), so the application's definitions keep them (`Definitions::with_numbers`).
  - A reader finds a track's number by its id (`DefinitionsAt::track_number`).
  - Missing: a chain resume that starts a new piece reads no closed piece, so its numbers start anew.
  - Read more: [Number, key and id](docs/tracks.md#number-key-and-id), [The definitions an application holds](docs/definitions.md#the-definitions-an-application-holds).

## Reader (`READ`)

Reading by position, checks and hashes, hostile bytes, in the crate `ww_tape`.

- ✅ **READ-1 Positional reader** · `v0.5`
  - The reader reads at offsets from any byte source: memory, a file or HTTP range requests in a browser. It finds pieces from their tails, one small read per piece.
  - It opens an unclosed piece from its newest checkpoint. It needs no file system and no threads, and builds for wasm32.
  - Read more: [Reading a tape](docs/reading.md).
- ✅ **READ-2 No panic on file bytes** · `v0.5`
  - Every reader takes damaged or hostile bytes without a panic, an overflow or an unbounded allocation.
  - A number read from a block reaches a length or a loop only through a named cap. The caps are listed in one table in the crate docs.
  - Clippy's panic-class lints are denied, and each exception names its invariant.
  - Every decoder has a fuzz target that builds valid blocks with hostile fields. Their seeds replay in `just test`.
  - Read more: [What the checks cannot do](docs/damage.md#what-the-checks-cannot-do).
- ✅ **READ-3 A range hash from the index**
  - A range hash folds each chunk by the hash its index entry states, without reading the chunk. It is the writer's statement until a check proves it.
  - The content check replays the piece and compares each index entry with its block and each close row with its track.
  - Read more: [Folding from the index](docs/track_hashes.md#folding-from-the-index). Fuzz target `tape-index`.
- ✅ **READ-4 Index entries checked on every path**
  - Entries of a piece are sorted by offset. Entries that name one block twice or overlap are refused, wherever they came from.
  - A final index out of order is refused when read, so every call that gives entries agrees.
  - Read more: [Finding blocks through the index](docs/reading.md#finding-blocks-through-the-index).
- 🚧 **READ-5 Typed numbers in the API**
  - Track and clock numbers are `TrackNum` and `ClockNum`, 1 and up, each exactly a `UNib32` on the wire; a definition naming 0 doesn't decode. A record names its track as a `RecordTrack`, track 0 being the definitions.
  - Missing: types for the other numbers of the API (ids, keys, seqs, item numbers, sizes).
  - Read more: the crate docs of `ww_tape` (`TrackNum`, `ClockNum`, `RecordTrack`).

## Files and locks (`FS`)

Everything that names a path, in the crate `ww_tape_fs`.

- ✅ **FS-1 Files, file sinks and locks** · `v0.5`
  - A new file is created exclusively under its final name; the file and its directory are synced after the create. Whether a piece is closed is read from its close and tail.
  - A sync policy syncs data by block count or time. A failed write or sync ends the file, never retried.
  - A file sink holds the kernel's lock on its open file (`flock`; a share mode on Windows), so a second writer is refused. A crash leaves nothing behind; a fork handler keeps the lock out of a forked child.
  - A file on a network or user-space file system (NFS, SMB, FUSE) gets a typed warning, `FsWarning`; writing goes on.
  - An error names the step, the file and the kind, with the system's error as its source.
  - Read more: [Files and syncing](docs/files.md), [Writing a file durably](docs/format.md#writing-a-file-durably).

## Codecs (`CODEC`)

Column codecs with no notion of a block, in the crate `ww_tape_codecs`.

- ✅ **CODEC-1 Sample codecs** · `v0.5`
  - A sample column holds one element type, such as `I16`, `F32` or `ComplexI16`, in one or more channels. Any range of samples decodes in place.
  - Raw device formats are stored as sent: big-endian and packed 10, 12 and 24 bits, converted on read.
  - Integers code as deltas, bit-packed in groups of 128. Damage costs only its groups, and a payload column can be one lz4 block.
  - Read more: [Sample codecs](docs/format.md#sample-codecs). Golden file `v1-samples.hex`; fuzz target `tape-samples`.
- ✅ **CODEC-2 Event times** · `v0.5`
  - Starts are implicit, from a period, or a linear fit per group of 128 with bit-packed residuals. Ends are absent, the next start, or packed durations.
  - Times are searched and read in place. A damaged column marks only the bad groups' items suspect.
  - The bit-pack kernels have AVX-512, AVX2 and NEON paths picked at run time, each checked against scalar.
  - Read more: [Event times](docs/columns.md#event-times). Golden file `v1-times.hex`; fuzz target `tape-times`.
- ✅ **CODEC-3 Payloads** · `v0.5`
  - Item payloads are a column of bit-packed lengths and a column of raw or lz4 bytes, whichever is smaller.
  - A damaged column is salvaged group by group. Only a bad group's items are marked.
  - Read more: [Payloads](docs/columns.md#payloads). Golden file `v1-payloads.hex`; fuzz target `tape-payloads`.
- ✅ **CODEC-4 ALP for floats** · `v0.5`
  - ALP codes floats that began as decimals as bit-packed integers, after Afroozeh, Kuffó and Boncz (SIGMOD 2024). Other floats are coded by their front bits.
  - It is lossless, and a group never takes more than 7 bytes over raw. The same input gives the same bytes on every platform.
  - Fixed point stays smaller than any float codec measured.
  - Read more: [Samples: fixed point first](docs/sizes.md#samples-fixed-point-first). Fuzz target `tape-samples`.
- ✅ **CODEC-5 One fast CRC-32C**
  - Every block kind checks its CRC-32C with one table, or with the CPU's CRC instruction where there is one.
  - On x86-64 the instruction reaches 31 to 37 GiB/s on large buffers. The ARMv8 path runs in CI.
  - Tests check every kernel against the table, at lengths past 2^29.
  - Read more: [Checks at three levels](docs/damage.md#checks-at-three-levels).
- 💡 **CODEC-6 Fast sample encoders**
  - `DELTA` and the packed codecs encode through a scalar 64-bit path, about 3.6 ns per value for `ComplexI16`, while `DELTA` decodes at 0.65 ns.
  - On one core the encoder is the writer's limit. Packed at 12 bits, the writer takes 157 MS/s of IQ and the codec 58 % of its time (`just bench-rf-core`; [How fast a radio capture records](docs/sizes.md#how-fast-a-radio-capture-records)).
  - Vector kernels for 16-bit values, as the decoders and summaries have, would lift it. The bytes must stay the same.

## Summaries (`SUM`)

Summary blocks, nodes and kernels, in the crates `ww_tape_summary` and `ww_tape_codecs`.

- ✅ **SUM-1 Summary blocks** · `v0.5`
  - A summary block stores a level of a track's summary tree. Nodes run over item numbers, so the nodes of pieces line up.
  - An event tree holds first and last tick, flags and latest end per node. A series summary holds minimum, maximum, sums and counts per channel.
  - Every statistic merges in item order to the bits the writer stores. An overview of a billion items reads about 630 KB.
  - Read more: [Summaries](docs/summaries.md). Golden file `v1-summary.hex`; fuzz target `tape-summary`.
- ✅ **SUM-2 Summaries across a change of scale** · `v0.5`
  - A node holding samples of two scales keeps them at their common scale. Minimum and maximum stay exact.
  - A value past its type or a scale that isn't a ratio of integers is a typed error, never a silent mix.
  - Read more: [How nodes merge](docs/summaries.md#how-nodes-merge).
- ✅ **SUM-3 Summary kernels in one pass** · `v0.5`
  - A batch of samples is summarised in one pass, fast enough to keep up with ingest. Any order of addition gives the same result.
  - Kernels for AVX2 and AVX-512 are picked at run time and checked against the scalar model.
  - Read more: [Summaries](docs/sizes.md#summaries); `just bench-summaries` measures them.
- 💡 **SUM-4 Minimum and maximum at the element's width**
  - A series node stores its minimum and maximum as 8 bytes per component, whatever the element: a `ComplexI16` node takes 32 bytes for them.
  - The RF capture's nodes take 40 bytes each, 1.1 % of its piece; at the element's width they would take 16.
  - A format change: the summary block's version and its readers change with it.

## Book (`BOOK`)

The pages in `docs/`.

- ✅ **BOOK-1 A book of its own**
  - The pages build as a Zensical site of their own. A broken link or anchor fails `just book-check`, which runs in CI.
  - The book links the WireWeaver and logring books and includes neither. It is published from main to https://tape.vhrd.tech/ once CI passes.
  - Read more: [Tapes](docs/index.md).
- 🚧 **BOOK-2 A book that teaches**
  - Done: `[[term]]` marks a glossary term and ``[`Item`]`` links code, both with a card on hover; a stale name fails `just book-check`.
  - Done: six parts in dependency order, a reading level on every page, and doctests of a page's `rust` blocks.
  - Done: the chapter "When the power fails", its figures drawn from a real crash and its code from a test.
  - Done: the running example as code (BOOK-3).
  - Done: Part I, "Tapes" and "A first tape": the drive's hour as a tape, and a program that writes and reads its first minute.
  - Done: Part II, how a tape is built: five chapters with the drive's case, tested code and seven new figures.
  - Done: Part III, writing: five chapters with every writing call the drive makes in tested code, and eight new figures.
  - Done: Part IV, reading: five chapters that find the drive's fault in code, with six new figures.
  - Done: Part V, damage and raw media, with the drive's archive and card, four new figures and the lessons appendix.
  - Done: the owner's read: one term each for record, item and sample; event times and samples on pages of their own.
  - Done: the chapter "Sizes and speeds", every number from a bench report in `docs/bench/`.
  - Done: data figures as eventfull screenshots in `docs/shots/`: the drive's hour, the fault, the zoom and the RF minute. `just book-shots` copies them; eventfull's tests keep them current (eventfull TL-79).
  - Open: autorange and damage as screenshots, and log and temp rows in them, once eventfull draws them.
  - Open: the chapters still stubs, tested code in every guide chapter.
  - Read more: [For writers](docs/writers.md), [Glossary](docs/glossary.md).

- ✅ **BOOK-3 The running example as code**
  - `ww_tape_example` records the drive motor controller's hour: log lines, mode, CAN frames, phase current with summaries and temperature, in a chain of pieces.
  - `Drive::scaled` shrinks the data and the byte policies for figures; a crash at any byte resumes to the bytes of the run without it.
  - `cargo run -p ww_tape_example` prints the story's numbers; `tests/story.rs` checks them and that the full hour is the same every run.
  - Damage to the last piece and the run on a raw card over an earlier run; `tests/damage.rs` pins what each costs.
  - `rf` records an RF capture, ComplexI16 IQ at 20 MS/s with summaries, as the book's case for high rates; `tests/rf.rs` pins its size per codec.
  - Read more: [ww_tape_example](ww_tape_example/README.md).

## Repository (`REPO`)

Crates, checks, fuzzing and benches.

- 🚧 **REPO-1 Crates by what they touch**
  - Done: `ww_tape_codecs` holds the codecs, and `ww_tape` writes and reads tapes on sinks and sources without std. `ww_tape_summary` builds summaries, and `ww_tape_fs` holds everything that names a path.
  - Done: the writer, chain and resume build with alloc only, and the codecs build for wasm32 without std.
  - Open: owned definition records without std, which need owned types in `shrink_wrap` and `ww_version`.
  - Open: the sample, time and payload codecs on slices, memory tapes in segments, a verify-on-read setting, integrity levels and a seal.
  - Read more: [The crates](docs/tape.md#the-crates).
- ✅ **REPO-84 spirit_level checks in `just lint`**
  - `just lint` and CI run `spl check` with every lint of `spirit_level.toml`. The prose lints read the book, the READMEs, AGENTS.md, the CHANGELOG and this file.
  - Every crate inherits the workspace lints, the panic-class set included.
- ✅ **REPO-85 Benches that rerun the book's numbers**
  - Every bench binary takes `--json FILE`, `--color`, `--version` and shell completion through `ww_tape_bench`. Each table records the machine and the load average before and after it.
  - `just bench-book` reruns every number of "Sizes and speeds" into `docs/bench/`; `ww_tape_figures` draws the page's charts and tables from those reports, and a test keeps them current.
  - Read more: [Sizes and speeds](docs/sizes.md).

## Dropped and superseded

Nothing yet.

## Former ids

The entries came from WireWeaver with `STD` ids, and new ones took `TAPE` ids. Each now has an id of its area.

| Former | Now |
| --- | --- |
| STD-42 | FMT-1 |
| STD-117 | FMT-2 |
| STD-118 | FMT-3 |
| STD-122 | FMT-4 |
| STD-125 | FMT-5 |
| STD-127 | FMT-6 |
| STD-129 | FMT-7 |
| STD-134 | FMT-8 |
| STD-135 | FMT-9 |
| TAPE-3 | FMT-10 |
| TAPE-7 | FMT-11 |
| STD-123 | WRITE-1 |
| STD-132 | WRITE-2, FS-1 |
| TAPE-2 | WRITE-3 |
| TAPE-4 | WRITE-4 |
| TAPE-6 | WRITE-5 |
| TAPE-8 | WRITE-6 |
| TAPE-14 | WRITE-7 |
| STD-124 | READ-1 |
| STD-131 | READ-2 |
| TAPE-9 | READ-3 |
| TAPE-10 | READ-4 |
| STD-119 | CODEC-1 |
| STD-120 | CODEC-2 |
| STD-126 | CODEC-3 |
| STD-130 | CODEC-4 |
| TAPE-5 | CODEC-5 |
| STD-121 | SUM-1 |
| STD-128 | SUM-2 |
| STD-133 | SUM-3 |
| TAPE-13 | BOOK-1 |
| TAPE-12 | REPO-1 |
