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
- An item moved to another area takes a new id there. Its old id is never reused and gets a row under [Former ids](#former-ids), as do the `STD` and `TAPE` ids of the first entries.
- Use the ID in commit messages, CHANGELOG entries and code comments such as `// TODO(FMT-1): ...` in the source.
- When talking to people, name an ID with a short slug from its title, such as `FMT-4 index-close-tail`, never bare.
- IDs of other areas (`TR`, `API`, `INT`) belong to [WireWeaver](https://github.com/vhrdtech/wire_weaver). So do `ww_tap`, `ww_tape_bundle` and `tests/tape_identity`.
- Each area starts with a line on what it covers. Done items come first, then open ones by target.
- An item has a heading line that names the feature and two to five plain sentences on what works today. A last line says where to read more. An open part takes one sentence. History stays in git and the CHANGELOG.
- When work finishes, update its item in the same change: mark it ✅ and add a pointer to a page, crate or test. Never delete a done item.
- An item that turns out obsolete moves to [Dropped and superseded](#dropped-and-superseded) with a one-line reason.
- A new idea goes into its area with 💡 as its status. Small gaps in the code stay as `TODO` comments; only gaps that limit users or block a feature get an item.

## Format (`FMT`)

The bytes of blocks, records, chunks, sessions and the index, in the crate `ww_tape`.

- ✅ **FMT-1 Tape blocks and records** · `v0.5`

  A tape is a run of blocks, each with a 32-byte header, its records and a CRC-32C, never padded. A record holds a track, a time delta, a length and the item's bytes, stored raw or as lz4. A block of a later version or an unknown kind is checked and skipped by its length, not taken for damage. Nothing in a block names its producer: WireWeaver's link tap (TR-14) and eventfull (CORE-19) both write tapes.

  Open: splitting a recording into files by size or time, and preallocating them.

  Read more: [Blocks](docs/blocks.md#blocks). Tests cut and flip every byte; fuzz target `tape-reader`.

- ✅ **FMT-3 Chunk blocks** · `v0.5`

  A chunk block holds one stretch of one track's events or samples, read in place. Its head places it in the track without an index. The head has a CRC of its own, and the payload has one for every 64 KiB, so a flipped bit costs one segment. In an aligned file every chunk payload starts on a 4 KiB boundary, and small blocks fill the gaps.

  Read more: [Chunks from columns](docs/columns.md). Golden file `v1-chunks.hex`; fuzz target `tape-chunk`.

- ✅ **FMT-4 Index, close and tail** · `v0.5`

  A checkpoint index every 4 MiB lists the blocks since the one before, so a crash costs at most the open chunk. A closed piece ends with a final index sorted by track, a close block and a fixed tail. A reader finds the tail from the end of the file. The close holds a BLAKE3 hash of the piece's content. Offsets count from the start of the piece, so pieces placed back to back keep their indexes.

  Read more: [Index, close and tail](docs/format.md#index-close-and-tail). Golden file `v1-index.hex`; fuzz target `tape-index`.

- ✅ **FMT-6 Series tracks** · `v0.5`

  A track holds either events or a series. A series track states the type of one value, its period and the scale of the stored numbers. The period is an exact ratio of ticks per sample. A batch of fixed-point values becomes chunk columns without a float. A series track has no records: each chunk carries its own grid and scale (FMT-10).

  Read more: [Series](docs/series.md). Golden file `v1-series.hex`; fuzz target `tape-series`.

- ✅ **FMT-7 Sessions, gaps and duplicates on raw media** · `v0.5`

  Every block carries a session tag, and seq numbers count per session, so stale blocks of an earlier session end the data. Blank bytes are 0x00 or 0xFF. A blank or damaged stretch followed by the session's next block is a gap, not the end. A block seen twice with the same bytes is dropped; with other bytes the later one wins and is reported.

  Read more: [Where the data ends on a raw card](docs/raw_media.md).

- ✅ **FMT-8 Closed pieces padded to 4 KiB** · `v0.5`

  A closed piece is a multiple of 4 KiB long. Pieces so clone back to back into one file on btrfs and XFS, with nothing between them. The writer pads the close block, which states its padding. Blocks inside a piece stay unaligned.

  Read more: [The 4 KiB end and cloning](docs/pieces.md#the-4-kib-end-and-cloning).

- ✅ **FMT-10 Series chunks carry their own grid and scale**

  A series chunk's head carries its grid and scale, so the chunk times and scales its samples alone. A track's timeline is built from chunk heads in any order: it checks each chunk against its neighbours and names the gaps. Building it reads only the heads, 335 bytes per 1 MiB chunk in an unaligned file.

  Read more: [Grids](docs/series.md#grids-where-samples-are-in-time). Fuzz targets `tape-series` and `tape-series-hostile`.

- ✅ **FMT-11 Track relations**

  A track's items can relate to another track's items by a named relation, such as a source, a parent or a context. The writer gives the meaning; ww_tape stores the values as a chunk column. A relation that never decreases is searched by lower bound, and the writer keeps it in order across cuts and resumes. A caller may accept a break in that order: the tape marks it, and readers refuse a decrease that is not marked.

  Open: postings and dictionary blocks, and values stored only in the index.

  Read more: [Relation columns](docs/format.md#relation-columns). Tests `relations.rs`; fuzz targets `tape-times` and `tape-index`.

- ✅ **FMT-13 Sample flags in series chunks**

  A series chunk keeps its samples' flag words as runs of consecutive samples sharing a word, in a `Flags` column of codec `FLAG_RUNS`. Only a chunk with a flagged sample has the column, so an unflagged series keeps its bytes. A chunk holds at most `FLAG_RUNS_MAX` (4096) runs; the writer completes it before one more. Readers take the runs per chunk (`Chunk::flag_runs`, `SeriesSamples::flag_runs`), and summaries derive their flags from them.

  Read more: [Sample flags](docs/format.md#sample-flags). Golden file `v1-series.hex`; tests `series_flags.rs`, `mux_resume.rs`; fuzz targets `tape-flags`, `tape-flags-hostile`.

- ✅ **FMT-14 Integrity levels and a seal**

  A piece names its level in its head: `Full` computes every CRC and hash, `CrcOnly` no hashes, `None` no payload CRCs either. Below `Full` the hashes hold zero and each index entry is marked unhashed; at `None` each chunk is marked `FLAG_PAYLOAD_UNCHECKED` and reads as `Checked::Unprotected`. Chunk head CRCs and every structure block stay checked, so framing, resume and salvage work at every level. Verify checks the structure, the chain check links by id (`ChainLink::Unproven`), and `range_hash` hashes the chunks' bytes.

  A seal raises a closed piece to `Full`, byte for byte what a `Full` writer writes from the same calls. It comes as `TapeWriter::seal_on`, `MemPieces::seal` (a sealed copy swapped in), `ww_tape_fs::writer::seal` (in place, each piece head written last). Tests: golden pieces per level, the fuzz target `tape-seal`, `tape-resume` at every level, and the bench `levels`.

  Read more: [Fewer checks, sealed later](docs/integrity.md).

- 🚧 **FMT-15 Grids: series tracks with fixed axes**

  A `GridDef` takes up to 31 fixed axes beside time, each cut into tiles, with regular or listed coordinates and a downsample flag. A series is the grid of none and keeps its bytes. A chunk holds one tile, and its head carries the tile's coordinates. Its index entry packs them into a tile number: bit fields of ⌈log2(tiles)⌉ bits per axis in a u128. `TapeReader::grid_tiles` finds the chunks of a time range and a box of cells from the index alone.

  Index entries are 128 bytes: the fields before keep their offsets and bytes 120..128 are reserved for every kind. Each index block declares its entry length, and readers read the first 120 bytes of each entry, so pieces of 120-byte entries still read.

  A `GridHandle` (`TapeMux::grid`, `grid_box`) takes a time step of the grid or of a box of its tiles. It splits the step into the open chunk of each tile. The tiles' chunks are completed together, by steps, bytes or latency.

  Edge tiles hold the cells left over, and a tile no box holds is never written. A tile's time step that fits no chunk or block is refused, never split. A series is the grid of no fixed axes and writes the bytes it wrote before.

  Each tile has a time pyramid of its own. An item is a time step, and a node holds the statistics of the tile's cells over its time run, per channel. A tile's summary block carries header flag `FLAG_TILE` and the tile's coordinates after the scale. Its index entry carries the tile number. Series summary blocks keep their bytes.

  `TapeReader::grid_summaries` finds a level's blocks of the tiles in a box. `region_timeline` merges their nodes into the box's timeline, weighting each tile by cells times steps. A tile partly in the box counts whole. A resumed mux rebuilds the tile trees as it does the series trees.

  Space overviews serve a zoomed-out view. Level k halves every downsampled axis k times and keeps the other axes and time. Each cell holds the min, max and mean per channel. One overview chunk per level follows each tile chunk, so levels stop where a tile extent stops halving evenly. `grid_tiles` takes the level from its box's layout, and `TileLayout::level_for` picks the level for a pixel size.

  Open: a book chapter.

  Read more: [Fixed axes and tiles](docs/format.md#fixed-axes-and-tiles), [Summaries of tiles](docs/format.md#summaries-of-tiles), [Space overviews](docs/format.md#space-overviews), [Index entries](docs/format.md#index-entries), [Typed handles encode on their own thread](docs/writing_thread.md#typed-handles-encode-on-their-own-thread). Golden files `v1-index.hex`, `v1-grid-{1,2,5,31}.hex`, `v1-grid-summary.hex`, `v1-overview.hex`; tests `grids.rs`, `mux_grids.rs`, `mux_grid_summaries.rs`, `mux_grid_overviews.rs`, `reserved.rs`, `grid::tests`, `index::tests`, `tile_tests`; fuzz targets `tape-grid`, `tape-grid-hostile`, `tape-index`, `tape-summary`, `tape-summary-hostile`.

## Tracks, clocks and definitions (`TRACK`)

Clocks, track identity, definitions on track 0, state, what items are and how tracks are numbered, in the crate `ww_tape`.

- ✅ **TRACK-1 Clocks, clock maps and typed items** · `v0.5`

  A record's time is a signed 64-bit tick count of its track's clock. Times are deltas per clock, so a block reads alone and holds no wall time. A clock has a stable key and an exact rate and resolution. A clock map turns one clock's ticks into another's with a named rounding. A track names its clock, its dimensions and its item type, and a typed item carries any WireWeaver value by that type.

  Read more: [Clocks and ticks](docs/records_and_clocks.md#clocks-and-ticks).

- ✅ **TRACK-2 Track identity by name path** · `v0.5`

  A track's key hashes the names of its resource, its array indices and the keys of keyed arrays. Two runs that number things differently so get the same ids (API-67). A resource renamed with `#[moved_from]` keeps its key (INT-17); one moved to another trait gets a new key. WireWeaver's crate `ww_tape_bundle` computes the names from an API bundle.

  Read more: [Track identity](docs/format.md#track-identity). Tests in WireWeaver's `tests/tape_identity`.

- ✅ **TRACK-3 Definitions on track 0, state near its use** · `v0.5`

  Definitions are plain records on track 0: the producer, the layout, bundles, clocks, maps, tracks and typed items. Each has a slot, and the newest record per slot counts. The application holds its definitions and passes them with each write; data of a track it does not hold is refused. A state item is put again only where the same item does not lie close behind. The index lists state records, so a reader looks state up anywhere, damage included.

  Read more: [Definitions on track 0](docs/blocks.md#definitions-on-track-0), [State tracks](docs/tape.md#state-tracks-and-near-use); `just bench-tracks` measures the cost. Fuzz target `tape-state-hostile`.

- ✅ **TRACK-4 What a track's items are**

  A track's definition says what its items are (`Items`): a WireWeaver stream's values, a ShrinkWrap type, plain bytes, numbers or a foreign format. A foreign format has a name, a media type and a version, and may carry its schema in the tape. Numbers are of an element the definition describes, of any width and byte order; series samples are numbers too. `Items` is evolvable: a variant from a newer ww_tape reads as unknown, and the rest of the definition stays intact.

  Read more: [What a track's items are](docs/tracks.md#what-a-tracks-items-are), [Items of a track](docs/format.md#items-of-a-track). Tests `raw.rs`, `iq8.rs`, `series.rs`; fuzz targets `tape-series`, `tape-samples`, `tape-samples-hostile`.

- ✅ **TRACK-5 Definitions carried into each new piece**

  A cut puts the definition of every track the new piece's head carries right after the head, with its clocks, maps and bundles. Dropping older pieces so keeps a quiet track's kind, clock and label. The carried definitions come in blocks of at most 4 KiB with their own tracks' clocks and maps, so damage costs only those tracks. Past the index policy's room the tracks with the oldest data are left out: the head marks them and a reader names them (`DefinitionsAt::not_carried`). A state track's newest item is carried beside its definition.

  Read more: [The definitions a new piece carries](docs/carried_definitions.md), [What they cost](docs/carried_definitions.md#what-they-cost) (`just bench-carried`). Tests `quiet.rs`.

- ✅ **TRACK-6 Layout and producer copied through the piece**

  Every call that puts a definition or a state item also keeps track 0's copy of the layout and the producer, spaced 1 MiB apart. Losing a piece's first blocks so loses neither, and a scan past the newest checkpoint takes its gap rules from the copy. A raw-media walk that starts before the first copy gets them from `OpenOptions::layout`. A piece under 1 MiB holds no copy.

  Read more: [When bytes go bad](docs/damage.md#the-first-4-kib-lost). Tests `head_copies.rs`.

- 🚧 **TRACK-7 Track and clock numbers given by the definitions**

  An application defines a track or a clock without a number (`TrackSpec`, `ClockSpec`), and `Definitions` gives it the lowest number not taken. The same track identity (key, dimensions, owner) or clock key gets its number back, also after a close. A number never goes to another track or clock. A resume reads the numbers back (`Resumed::numbers`, `Definitions::with_numbers`), a chain's from every piece present, counting unread ones (`Resumed::numbers_unread`). A track or clock defined only in a removed piece loses its number, and a reader finds a track's number by its id (`DefinitionsAt::track_number`).

  Open: `TapeWriter::resume_on` alone, outside a chain, reads a closed piece's numbers only from its head's puts and after its last close.

  Read more: [Number, key and id](docs/tracks.md#number-key-and-id), [The definitions an application holds](docs/definitions.md#the-definitions-an-application-holds), `ww_tape_tests/tests/it/chain_numbers.rs`.

- ✅ **TRACK-8 Keyed state tracks**

  A keyed state track (`TrackRepeat::Keyed`) holds many slots, one per key of at most 1024 bytes, the newest record per key in force. `put_key` and `delete_key` put a key's value or end it, on the writer, a chain and the mux's `StateHandle`, near its use per key. The index lists each record by its key's first 8 bytes, length and 64-bit hash. A key's records and a prefix's live keys so are found without reading blocks (`key_entries`, `key_slots`). `key_at`, `key_history` and `keys_at` read the records, and the full key decides a hash collision.

  A cut carries every live key into the new piece and never leaves a keyed track out. It marks the head row (`TrackStart::keys_carried`) when the writer knew every key. A piece read alone then holds the keys in force at its start, also after a chain cut by itself or resumed after a crash.

  A reader names a piece without them (`KeysAt::not_carried`). A put that would make the carry too large is refused naming the limit (`InputError::CarryFull`). So is a key with another live key's id (`DescribeError::KeyCollision`).

  Read more: [Keyed state tracks](docs/format.md#keyed-state-tracks), [State entries](docs/format.md#state-entries). Golden files `v1-keyed.hex`, `v1-keyed-carried.hex`; tests `keyed.rs`, `keyed_carry.rs`, `state::tests`, `index::tests`; fuzz targets `tape-keyed`, `tape-keyed-hostile`.

## Writer (`WRITE`)

Writing pieces and chains, resuming them, and sinks in memory, in the crate `ww_tape`.

- ✅ **WRITE-1 Pieces and chains** · `v0.5`

  A recording is cut into complete pieces at chunk boundaries, by bytes, chunks or age, and a cut never leaves a piece empty. Each piece names the one before by its id and content hash. A chain check reports gaps, broken links, an open end and a lost first piece.

  Read more: [Anatomy of a piece](docs/anatomy.md), [Pieces and chains](docs/pieces.md).

- ✅ **WRITE-2 Resume after a crash** · `v0.5`

  A writer reopens an unclosed piece and keeps every block up to the last whole one. The resumed piece ends with the same bytes as a write that never stopped. A chain resume also reads its closed pieces back, and handles a crash between a cut's close and the next piece. Writers write through a sink and chains through a store of tapes, so the same code writes files and memory.

  Read more: [When the power fails](docs/crashes.md). Tests cut pieces and chains at every block and at random bytes; fuzz target `tape-resume`.

- ✅ **WRITE-3 Each write returns where it landed**

  Each write of a chunk or a block returns the offset and length of the block it wrote. A chain also returns the piece the block landed in.

  Read more: tests `placed.rs` read each returned offset back.

- ✅ **WRITE-4 Writer checks in proportion to their cost**

  The writer checks the head of a chunk it encodes, not the encoded bytes. A block the caller encoded is read once, with its CRCs checked. On a 1 MiB series chunk this costs 466 µs past the encoding.

  Read more: [What the writer checks](docs/writing.md#what-the-writer-checks).

- ✅ **WRITE-5 Pieces in memory**

  A memory sink reserves room for a whole piece, so a reader waits at most 0.16 ms while a 256 MiB piece is written. A closed piece can be taken out of memory, and readers holding it read on.

  Open: resuming a chain in memory whose first pieces were taken out.

  Read more: [Reading while the writer writes](docs/live_feeds.md#reading-while-the-writer-writes).

- ✅ **WRITE-6 A chain in one file, track owners and running hashes**

  A chain writes one file per piece, or all its pieces into one file, where each session that opens the file adds a piece. A sink is the only writer of its bytes, and a second writer is refused. Tracks of one identity are told apart by an owner. A running hash per track is the same whether the track is written alone or interleaved, in one piece or many.

  Read more: [Writing a chain into one file](docs/pieces.md#writing-a-chain-into-one-file), [Track owners and hashes](docs/track_hashes.md). Fuzz target `tape-resume`.

- ✅ **WRITE-7 Appending to a closed piece**

  A close ends a write session, not the piece: a writer resumed on a closed piece appends after its tail and closes again. Nothing before the append moves, and each close hashes the close before it and the bytes since. Readers take the last close whose tail is whole and read bytes after it as an unclosed tail, never as damage. A resume cuts back only a torn end and refuses damage.

  Read more: [Reading a growing piece](docs/growing_pieces.md), [The close in force](docs/format.md#the-close-in-force). Fuzz target `tape-reclose`.

- ✅ **WRITE-8 Pieces removed on purpose**

  A close can name pieces of its chain that were removed on purpose, by sequence number, id and close hash. `TapeWriter::record_removed` appends these rows to the chain's last piece, and every later close of that piece names them again. The chain check links over the hole (`ChainLink::Removed`) when both ends match the rows. Removing the piece that holds the rows means naming them again.

  Read more: [Pieces removed on purpose](docs/pieces.md#pieces-removed-on-purpose). Golden file `v1-removed.hex`.

- ✅ **WRITE-16 Resume after removed pieces**

  A chain resumes from its lowest piece at hand, so retention and a writer that restarts work together. A hole in the middle is walked over when the closes at hand name its pieces as removed on purpose. Any other hole is refused. `PieceChain::remove` removes closed pieces in one call. It names them in the piece written, closes and syncs it (`TapeWriter::close_and_go_on`), then takes the pieces out of the store.

  Read more: [Removing pieces while the chain is written](docs/pieces.md#removing-pieces-while-the-chain-is-written). Tests `ww_tape_tests/tests/it/chain_removal.rs`.

## Writing from many threads (`MUX`)

The writing thread and its handles, in the crate `ww_tape_mux`.

- ✅ **MUX-1 One writing thread, handles on any thread**

  A `TapeMux` owns a chain and its definitions on a writing thread; producers on other threads send through handles. `ChunkSender` sends one track's chunks, `RecordsHandle` records, `RelayHandle` another producer's blocks and `StateHandle` puts on change. All share one queue bounded by bytes, where each handle waits, fails with its unit back, or drops and counts. The writing thread does the puts, cuts, syncs, checkpoints and the close, and a failed write reaches every handle. A mux dropped unclosed leaves its piece unclosed, and `TapeMux::resume` goes on from it.

  Read more: [Writing from many threads](docs/writing_thread.md). Tests `mux.rs`, `item_gap.rs`, `mux_multi.rs`.

- ✅ **MUX-2 Typed handles that encode on the producer's thread**

  `EventsHandle` takes the items of one events track and `SeriesHandle` samples on a grid, each checked at the call. Each encodes its chunks, hashes their payload and computes the leaves of the track's summaries on its own thread. The writing thread joins the leaves into one summary tree per track and piece. A resumed mux rebuilds the trees of the torn piece from its kept chunks. The chain then ends with the bytes of a run without the crash.

  Open: a resumed chain's track starts come from the caller (`first_item`), not yet from the mux.

  Read more: [Typed handles encode on their own thread](docs/writing_thread.md#typed-handles-encode-on-their-own-thread); `just bench-handles` measures them. Tests `mux_handles.rs`, `split_tests.rs`, `placed.rs`, `mux_resume.rs`, `series_flags.rs`.

- ✅ **MUX-3 Async waits for the mux and the followers**

  Every call of the mux, its handles and the followers that waits has an `_async` twin. Threads and tasks wait in the same line, in arrival order, and a blocking call parks its thread on the same future. No crate depends on an async runtime: the writing thread, or a follower's own waiting thread, wakes the task. A send dropped while it waits leaves the line, and a typed handle sends the chunks a dropped push completed on its next call.

  Open: a typed handle's drop still sends blocking, so async code ends a handle with `finish_async`.

  Read more: [Threads and tasks](docs/writing_thread.md#threads-and-tasks). Tests `mux_async.rs` on tokio's two schedulers and smol.

## Reader (`READ`)

Reading by position, checks and hashes, hostile bytes and live chains, in the crates `ww_tape` and `ww_tape_fs`.

- ✅ **READ-1 Positional reader** · `v0.5`

  The reader reads at offsets from any byte source: memory, a file or HTTP range requests in a browser. It finds pieces from their tails with one small read per piece, and opens an unclosed piece from its newest checkpoint. It needs no file system and no threads, and builds for wasm32.

  Read more: [Reading a tape](docs/reading.md).

- ✅ **READ-2 No panic on hostile bytes** · `v0.5`

  Every reader takes damaged or hostile bytes without a panic, an overflow or an unbounded allocation. A number read from a block reaches a length or a loop only through a named cap. The crate docs list the caps in one table. Clippy's panic-class lints are denied, and each exception names its invariant. Every decoder has a fuzz target that builds valid blocks with hostile fields, and their seeds replay in `just test`.

  Read more: [What the checks cannot do](docs/damage.md#what-the-checks-cannot-do).

- ✅ **READ-3 Range hashes from the index**

  A range hash folds each chunk by the hash its index entry states, without reading the chunk. It is the writer's statement until a content check proves it. The content check replays the piece and compares each index entry with its block and each close row with its track.

  Read more: [Folding from the index](docs/track_hashes.md#folding-from-the-index). Fuzz target `tape-index`.

- ✅ **READ-4 Index entries checked on every path**

  A piece's index entries are sorted by offset. Entries that name one block twice or overlap are refused wherever they came from. A final index out of order is refused when it is read, so every call that gives entries agrees.

  Read more: [Finding blocks through the index](docs/reading.md#finding-blocks-through-the-index).

- ✅ **READ-5 Typed numbers in the API**

  Track and clock numbers, keys and ids are types of their own: `TrackNum`, `ClockNum`, `TrackKey`, `TrackId` and `ClockKey`. Each is exactly a `UNib32` or a `u64` on the wire. An item's number on its track is an `ItemNum`, and a set of relations is a `RelationSet`. Byte sizes in options and limits are exact_units `Bytes<T>`, and counts there are `Count<_, u64>`. Offsets, seqs and plain counters stay bare, and a number read from a file stays a `FromFile<T>` (READ-2).

  Read more: the crate docs of `ww_tape`.

- ✅ **READ-6 Followers of a live chain**

  `ww_tape_fs::follow::ChainFollower` reads a chain while another process writes it, piece after piece, and hands out only blocks that check. A torn end is read again once a resume has cut it back, and a block the resume took back is never handed out. `TrackFollower` decodes one track and reports item-number gaps as lost items. `TimeMerge` merges chains by time, and `TapeMux::watch` shows readers in the writer's process what is written and synced.

  Read more: [Following a chain while it is written](docs/growing_pieces.md#following-a-chain-while-it-is-written). Tests `follow.rs` (a writer process killed and resumed), `progress.rs`.

## Files and locks (`FS`)

Everything that names a path, in the crate `ww_tape_fs`.

- ✅ **FS-1 Files, file sinks and locks** · `v0.5`

  A new file is created exclusively under its final name, then the file and its directory are synced. A sync policy syncs data by block count or time, and a failed write or sync ends the file without a retry. A file sink holds the kernel's lock on its open file, so a second writer is refused and a crash leaves nothing behind. A file on a network or user-space file system gets a typed warning (`FsWarning`), and writing goes on.

  Read more: [Files and syncing](docs/files.md), [Writing a file durably](docs/format.md#writing-a-file-durably).

- ✅ **FS-2 Verifying and range-hashing pieces in parallel**

  Each piece's `verify` or `range_hash` is independent of every other piece's. `ww_tape_fs::check` gives both over
  many piece files at once:

  - `verify_each` and `range_hash_each`: one piece at a time.
  - `verify_each_parallel` and `range_hash_each_parallel`: the same pieces over rayon's thread pool, behind the
    cargo feature `parallel`, the same results in the same order (a test proves it).

  A bench measured this first: pieces in parallel won clearly, over one piece at a time and over BLAKE3's own
  multithreaded hashing of one piece.

  Read more: [Checking many pieces at once](docs/check_pieces.md). Tests
  `ww_tape_tests/tests/it/check_parallel.rs`.

## Codecs (`CODEC`)

Column codecs with no notion of a block, in the crate `ww_tape_codecs`.

- ✅ **CODEC-1 Sample codecs** · `v0.5`

  A sample column holds one element type, such as `I16`, `F32` or `ComplexI16`, in one or more channels. Any range of samples decodes in place. Raw device formats are stored as sent, big-endian or packed in 10, 12 or 24 bits, and converted on read. Integers code as deltas, bit-packed in groups of 128, so damage costs only its groups.

  Read more: [Sample codecs](docs/format.md#sample-codecs). Golden file `v1-samples.hex`; fuzz target `tape-samples`.

- ✅ **CODEC-2 Event times** · `v0.5`

  An event's start is implicit, from a period, or from a linear fit per group of 128 with bit-packed residuals. Its end is absent, the next start, or a packed duration. Times are searched and read in place, and a damaged column marks only the bad groups' items as suspect. The bit-pack kernels have AVX-512, AVX2 and NEON paths, each checked against the scalar one.

  Read more: [Event times](docs/columns.md#event-times). Golden file `v1-times.hex`; fuzz target `tape-times`.

- ✅ **CODEC-3 Payloads** · `v0.5`

  Item payloads are a column of bit-packed lengths and a column of bytes, stored raw or as lz4. The writer picks the codec per track: by default whichever is smaller, raw always for payloads that don't compress, or a named codec. A later codec is a new entry in a registry, never a format break. A damaged column is salvaged group by group, and only a bad group's items are marked.

  Read more: [Payloads](docs/columns.md#payloads), [Adding a codec](docs/format.md#adding-a-payload-codec). Golden file `v1-payloads.hex`; fuzz target `tape-payloads`.

- ✅ **CODEC-4 ALP for floats** · `v0.5`

  ALP codes floats that began as decimals as bit-packed integers, after Afroozeh, Kuffó and Boncz (SIGMOD 2024). Other floats are coded by their front bits. The codec is lossless, a group never takes more than 7 bytes over raw, and the same input gives the same bytes on every platform. Fixed point stays smaller than any float codec measured.

  Read more: [Samples: fixed point first](docs/sizes.md#samples-fixed-point-first). Fuzz target `tape-samples`.

- ✅ **CODEC-5 CRC-32C with hardware kernels**

  Every block kind checks its CRC-32C with one table, or with the CPU's CRC instruction where there is one. On x86-64 the instruction reaches 31 to 37 GiB/s on large buffers, and the ARMv8 path runs in CI. Tests check every kernel against the table.

  Read more: [Checks at three levels](docs/damage.md#checks-at-three-levels).

- 💡 **CODEC-6 Vector kernels for the sample encoders**

  `DELTA` and the packed codecs encode through a scalar 64-bit path, while their decoders have vector kernels. On one core the encoder limits the writer: packed at 12 bits, the codec takes 58 % of the writer's time on IQ. Vector kernels for 16-bit values would lift that limit, and the bytes must stay the same.

  Read more: [How fast a radio capture records](docs/sizes.md#how-fast-a-radio-capture-records) (`just bench-rf-core`).

## Summaries (`SUM`)

Summary blocks, nodes and kernels, in the crates `ww_tape_summary` and `ww_tape_codecs`.

- ✅ **SUM-1 Summary blocks** · `v0.5`

  A summary block stores one level of a track's summary tree. Nodes run over item numbers, so the nodes of different pieces line up. An event tree holds the first and last tick, flags and latest end per node. A series summary holds minimum, maximum, sums and counts per channel. Every statistic merges in item order to the bits the writer stores, and an overview of a billion items reads about 630 KB.

  Read more: [Summaries](docs/summaries.md). Golden file `v1-summary.hex`; fuzz target `tape-summary`.

- ✅ **SUM-2 Summaries across a change of scale** · `v0.5`

  A node holding samples of two scales keeps them at their common scale, so minimum and maximum stay exact. A value past its type, or a scale that is not a ratio of integers, is a typed error rather than a silent mix.

  Read more: [How nodes merge](docs/summaries.md#how-nodes-merge).

- ✅ **SUM-3 Summary kernels in one pass** · `v0.5`

  A batch of samples is summarised in one pass, fast enough to keep up with ingest, and any order of addition gives the same result. Kernels for AVX2 and AVX-512 are picked at run time and checked against the scalar model.

  Read more: [Summaries](docs/sizes.md#how-fast-summaries-keep-up); `just bench-summaries` measures them.

- 💡 **SUM-4 Minimum and maximum at the element's width**

  A series node stores its minimum and maximum in 8 bytes per component, whatever the element. A `ComplexI16` node so spends 32 bytes on them. At the element's width the RF capture's nodes would take 16 bytes for them instead of 40 bytes. It is a format change: the summary block's version and its readers change with it.

## Book (`BOOK`)

The pages in `docs/`.

- ✅ **BOOK-1 The book as a site of its own**

  The pages build as a Zensical site of their own, and a broken link or anchor fails `just book-check` in CI. The book links the WireWeaver and logring books and includes neither. It is published from main to https://tape.vhrd.tech/ once CI passes.

  Read more: [Tapes](docs/index.md).

- 🚧 **BOOK-2 A book that teaches the format**

  The book runs in six parts in dependency order, each page with a reading level, from a first tape through building, writing, reading and damage. Glossary terms and code links carry a card on hover, and a stale name fails `just book-check`. The pages' `rust` blocks run as doctests, their figures are drawn from real blocks, and their numbers come from bench reports. A format reference with numbered sections holds the detail.

  Open: the chapters that are still stubs, tested code in every guide chapter, and screenshots of autorange and damage once eventfull draws them.

  Read more: [For writers](docs/writers.md), [Glossary](docs/glossary.md).

- ✅ **BOOK-5 Fields and images over time**

  A chapter explains grid tracks: fixed axes, tiles and the box query. It covers the time pyramid of a region and the space overviews, with the level a viewer picks. It also shows where per-tile overviews end. Two cases run in tests: `example_thermal`, a plate of sensors, and `example_images`, an image sequence in three tile sizes. Every number on the page comes from their runs.

  Read more: [Fields and images over time](docs/grids.md), [example_thermal](examples/example_thermal/README.md), [example_images](examples/example_images/README.md). Tests `thermal.rs`, `images.rs`.

- ✅ **BOOK-6 The book kit from its own repository**

  The book pins `vhrd_zensical` in the justfile; `vz prep` writes the code index, the cards and the footer stamp,
  replacing spirit_level's book extension. The lexers are now `vz-console` and `vz-rust`, and the long forms of
  `abbreviation-first-use` live in `docs/abbreviations.toml`, read by the kit, not `spirit_level.toml`.

  Read more: [For writers](docs/writers.md).

## Example and cases (`EX`)

The book's running example and its other cases: `example_drive`, `example_rf`, `example_ethernet` and `example_wideband`, with what they share in `example_capture`.

- ✅ **EX-1 The drive's hour as code**

  `example_drive` records the drive motor controller's hour: log lines, mode, CAN frames, phase current with summaries and temperature, in a chain of pieces. `Drive::scaled` shrinks the data and the byte policies for figures, and a crash at any byte resumes to the bytes of the run without it. The RF capture (`example_rf`) records ComplexI16 IQ at 20 MS/s as the book's case for high rates. `cargo run -p example_drive` prints the story's numbers, and the tests pin them.

  Read more: [example_drive](examples/example_drive/README.md), [example_rf](examples/example_rf/README.md). Tests `story.rs`, `damage.rs`, `rf.rs`.

- ✅ **EX-2 A 10 Gbps link and a 1 GS/s receiver**

  `example_ethernet` records a protocol analyser on both directions of a 10 Gbps link: frames as raw-bytes events, decoded headers as tracks related to their frames. `example_wideband` records a 1 GS/s receiver in channels, each a series track with its own producer thread, over several muxes and chains. `just bench-ethernet` and `just bench-wideband` measure how much of the link and the band one PC keeps.

  Read more: [Keeping what matters of a 10 Gbps link](docs/protocol_analyser.md), [Keeping the bursts of a fast band](docs/wideband.md). Tests `ethernet.rs`, `wideband.rs`.

- ✅ **EX-3 The example writes through the mux**

  `Drive::record` writes through a `TapeMux` from one thread in the story's fixed order, so the placement is the same every run. Each source uses the handle that fits it, and `Crashed::resume` resumes the mux to the bytes of the whole run. The book's writer code comes from this path, and `example_drive::handles::record` runs one thread per source.

  Read more: [The drive with one thread per source](docs/writing_thread.md#the-drive-with-one-thread-per-source). Tests `handles.rs`.

- ✅ **EX-4 Capture into memory, keep what matters**

  `example_capture::window` runs a capture into chains of pieces in memory, each written by a mux of its own. An analyser on a thread per chain reads each piece as it closes. The pieces it keeps, with their neighbours, are copied to a store of their own, and the rest leave memory unwritten.

  `example_ethernet::ram` captures the 10 Gbps link in a chain per port and keeps the pieces around each TCP reset. It writes the resets it finds to a results chain. `example_wideband::ram` captures a 250 MS/s receiver and keeps the pieces around each burst, found by energy detection on the summary nodes alone. `just bench-ethernet` and `just bench-wideband` measure the capture's pace, the triage's speed and the bytes kept.

  Read more: [Keeping what matters of a 10 Gbps link](docs/protocol_analyser.md), [Keeping the bursts of a fast band](docs/wideband.md). Tests `window.rs`, `ethernet_ram.rs`, `wideband_ram.rs`.

- ✅ **EX-5 A crate per case, its pipeline a library**

  Each case is a crate of its own: `example_drive` the book's running example, `example_rf`, `example_ethernet` and `example_wideband`. What they share, the memory window and the RF signal, is in `example_capture`, so no case depends on another. `window::start` and each case's `start` return a running pipeline. It hands out its memory window, the pieces in memory and each mux's watch, so a viewer follows it live. Each case has a headless binary whose `main` reads like a block diagram of its threads.

  Read more: [The pipeline](docs/protocol_analyser.md#the-pipeline), [example_capture](examples/example_capture/README.md). Tests `window.rs`.

- ✅ **EX-6 Payload compression measured on the cases**

  `example_ethernet` records its `frames` payloads raw, as its payloads are random like encrypted traffic, and `Link::payload(PayloadMix::Plain)` makes a synthetic plaintext mix whose payloads compress. `just bench-ethernet` measures both mixes with `PayloadCodec` Raw and Auto: the bytes kept and the CPU time a frame takes. `just bench-wideband` measures the bytes and CPU time a sample takes under each sample codec.

  Read more: [Keeping what matters of a 10 Gbps link](docs/protocol_analyser.md), [Keeping the bursts of a fast band](docs/wideband.md). Test `ethernet.rs`.

- 🚧 **EX-8 Rayon on the cases' captured data**

  `example_ethernet`'s triage decodes the chunks of a closed piece on a rayon pool. `RamCapture::triage_threads` sets its size, by default 2 threads a chain. `just bench-ethernet` runs the largest capture with each size of the pool. rayon is a dependency of the example crates only. The kept pieces' copy and `example_wideband`'s triage stay sequential.

  Read more: [Keeping what matters of a 10 Gbps link](docs/protocol_analyser.md). Test `ethernet_ram.rs`.

## Repository (`REPO`)

Crates, checks, fuzzing and benches.

- ✅ **REPO-84 spirit_level checks in `just lint`**

  `just lint` and CI run `spl check` with every lint of `spirit_level.toml`. The prose lints read the book, the READMEs, AGENTS.md and the CHANGELOG. Every crate inherits the workspace lints, the panic-class set included.

- ✅ **REPO-85 Benches that rerun the book's numbers**

  Every bench binary takes `--json FILE`, `--color`, `--version` and shell completion through `ww_tape_bench`, and records the machine and its load with each table. `just bench-book` reruns every number of "Sizes and speeds" into `docs/bench/`. `ww_tape_figures` draws the page's charts and tables from those reports, and a test keeps them current.

  Read more: [Sizes and speeds](docs/sizes.md).

- 🚧 **REPO-1 Crates by what they touch**

  `ww_tape` writes and reads tapes on sinks and sources without std, `ww_tape_codecs` holds the codecs and `ww_tape_summary` the summaries. `ww_tape_fs` holds everything that names a path. The writer, chain and resume build with alloc only, and the codecs build for wasm32 without std.

  The codecs work on slices. Memory tapes keep their bytes in extents that never move, and `MemPieces::drop_front` frees a tape's oldest pieces. A reader checks payload CRCs as `OpenOptions::check_payloads` says: by default a file is checked and a memory tape of this process is not.

  Open: std-free definition records (owned types in `shrink_wrap` and `ww_version`), integrity levels, a seal.

  Read more: [The crates](docs/tape.md#the-crates).

## Dropped and superseded

Nothing yet.

## Former ids

The first entries came from WireWeaver with `STD` ids, and later ones took `TAPE` ids. Entries moved between areas keep a row here too.

| Former | Now |
| --- | --- |
| STD-42 | FMT-1 |
| STD-117 | TRACK-1 |
| STD-118 | FMT-3 |
| STD-122 | FMT-4 |
| STD-125 | TRACK-2 |
| STD-127 | FMT-6 |
| STD-129 | FMT-7 |
| STD-134 | FMT-8 |
| STD-135 | TRACK-3 |
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
| FMT-2 | TRACK-1 |
| FMT-5 | TRACK-2 |
| FMT-9 | TRACK-3 |
| FMT-12 | TRACK-4 |
| WRITE-9 | TRACK-5 |
| WRITE-10 | TRACK-6 |
| WRITE-11 | TRACK-7 |
| WRITE-12 | MUX-1 |
| WRITE-13 | MUX-2 |
| WRITE-14 | MUX-3 |
| BOOK-3 | EX-1 |
| BOOK-4 | EX-2 |
| WRITE-15 | EX-3 |
