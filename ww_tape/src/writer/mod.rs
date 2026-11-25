//! [TapeWriter]: blocks appended to a sink, a file or memory, durable the way databases make their logs durable.
//!
//! - A new file (`ww_tape_fs`) is created exclusively under its final name; the file and its directory are synced
//!   after it is created, so the name can't vanish in a crash. Whether it is closed is read from its bytes (its close
//!   and tail), never from its name.
//! - The data is synced by a [SyncPolicy] (none, every N blocks, or at most an interval apart, by default 1 s), like
//!   SQLite's `synchronous` or Postgres' `synchronous_commit`: what a crash can lose is bounded by the policy.
//!   [std::fs::File::sync_data] is `fdatasync` on Linux, `fcntl(F_FULLFSYNC)` on macOS (a plain `fsync` there doesn't
//!   flush the drive's cache) and `FlushFileBuffers` on Windows.
//! - A failed write or sync is the end of the file: the writer stops, reports it and refuses every later write. It
//!   never retries: after a failed `fsync` Linux may drop the dirty pages and a second `fsync` succeeds without them
//!   (Postgres "fsyncgate", 2018).
//!
//! A reader takes the longest valid prefix of a file ([crate::Recovery::Prefix]), so whatever a crash leaves reads as
//! the file up to the last block that made it. Preallocating space (`fallocate`, read back as the end since zeros never
//! start a block) would go into [TapeWriter::write_block]; it is not built.
//!
//! The writer indexes what it places: a checkpoint [index](crate::index) block every few MiB ([IndexPolicy]), so a
//! reader after a crash rescans only the stretch after the newest one. [TapeWriter::close] ends the file as a
//! [piece](crate::piece): the final index sorted by track, the close record with the BLAKE3 content hash of
//! everything before it, and the tail.
//!
//! The application keeps the definitions of what it works on ([Definitions]) and passes them with every call that
//! writes data or puts an item; the writer checks the data against them and keeps no table of its own. State is put
//! ([state](crate::state)): before a block of data of a track the writer puts the track's definition and what it
//! names, the application puts a state track's items ([TapeWriter::put]) at each use, and each is written only when
//! the same item doesn't lie close behind in the piece, or as a copy ([near]). The layout and the producer are the
//! writer's own ([WriterOptions]), put at each piece head. A call that needs a definition or puts an item uses them
//! too, so they are copied and put again as track 0's definitions are: a piece whose first blocks are lost still
//! names them. [TapeWriter::cut_with] ends a piece and starts the next one of the chain; [crate::PieceChain] cuts by
//! size, chunk count or time.
//!
//! The writer acts around every block of data it places, whoever made it (the caller, or the writer from its
//! deferred blocks): before it, the definitions it puts; after it, a checkpoint when due (in an aligned file, in the
//! gap before the next chunk). So what the writer adds is a function of the blocks before it and the calls since, and
//! [TapeWriter::resume_on] rebuilds the writer of an unclosed piece from its bytes alone ([resume]).
//!
//! A writer puts its bytes in a [TapeSink]: memory ([crate::sink::MemSink], with `std`) or a file (`FileSink` of the
//! crate `ww_tape_fs`).

use crate::TrackId;
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::time::Duration;

use crate::clock::ClockRef;
use crate::index::{IndexPolicy, Indexer, RECORDS_TRACK_ID, STATE_TRACK_ID, TrackIdent};
use crate::open::{RecordRef, write_blocks_of};
use crate::piece::{
    Close, CloseReason, ContentHash, Integrity, OwnerId, PIECE_ALIGN, PieceHead, PieceId,
    PieceIdent, TAIL_LEN, Tail, TrackHash, TrackStart, UnitHash, close_len, close_padding,
    hash_block, piece_head_len, write_close, write_piece_head,
};
use crate::reader::wide;
use crate::sink::{PieceStore, TapeSink, sink_err};
use crate::state::{ItemHash, Slot};
use crate::{
    BlockId, BlockKind, ClockTime, Definitions, Describe, DescribeError, IndexError, Layout,
    Producer, RecordTrack, STATE_ITEM_MAX, SessionTag, TrackNum, TypedItem,
};
use alloc::collections::{BTreeMap, BTreeSet};

mod carried;
mod error;
pub(crate) mod grids;
mod group_records;
pub(crate) mod kept;
mod keyed;
pub mod near;
mod relations;
mod relay;
pub mod resume;
mod seal;
mod twice;
pub use carried::CARRIED_BLOCK_LEN;
pub use error::{
    CarryLimit, CutError, GroupRefusal, InputError, RemovedRefusal, ResumeError, WriteError,
};
pub use kept::KEPT_BYTES_MAX;
pub use near::NearPolicy;
use near::Rule;
pub(crate) use relations::Newest;
pub use relay::{RELAY_GROUPS_MAX, RELAY_HELD_BYTES_MAX, RELAY_RECORD_OVERHEAD};
pub use resume::{RESUME_LOOK_MAX, RESUME_PROBES_MAX, Resumed};
pub use seal::{SealError, Sealed};

/// When a [TapeWriter] syncs the data to the disk, besides at [TapeWriter::close].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncPolicy {
    /// Only at the end: a crash may lose everything the OS hadn't written yet.
    None,
    /// After every N blocks.
    EveryBlocks(u32),
    /// When blocks are waiting and the last sync is this long ago, at a block or at [TapeWriter::tick].
    Interval(Duration),
}

impl Default for SyncPolicy {
    fn default() -> Self {
        Self::Interval(DEFAULT_SYNC_INTERVAL)
    }
}

/// The header flags a caller's block of `kind` may carry when written as it is, not as puts: on a chunk the marks of
/// a unit, a relation break or a grid start, each checked where due. The other bits are reserved, retired or the
/// writer's own.
fn caller_flags(kind: BlockKind) -> u16 {
    match kind {
        // a chunk of a unit as another writer made it ([TapeWriter::write_chunk_unit])
        BlockKind::Events => crate::FLAG_RELATION_BREAK | crate::FLAG_WITH_NEXT,
        BlockKind::Series => crate::FLAG_GRID_START | crate::FLAG_WITH_NEXT,
        _ => 0,
    }
}

/// The session of a new writer: random with `std` ([SessionTag::random]); without it none, so the writer refuses to
/// write until [TapeWriter::set_session] or a piece head ([TapeWriter::begin_piece]) gives it one.
fn new_session() -> SessionTag {
    #[cfg(feature = "std")]
    return SessionTag::random();
    #[cfg(not(feature = "std"))]
    return SessionTag::NONE;
}

/// Default of [SyncPolicy::Interval].
pub const DEFAULT_SYNC_INTERVAL: Duration = Duration::from_secs(1);

/// How a [TapeWriter] places what it adds: what [TapeWriter::resume_on] needs to rebuild one, and what a
/// [crate::PieceChain] gives each of its pieces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriterOptions {
    pub sync: SyncPolicy,
    pub index: IndexPolicy,
    /// [TapeWriter::set_layout]: how the file's blocks are placed, put at each piece head and copied; its alignment
    /// is 0 or [crate::ALIGNMENT].
    pub layout: Layout,
    /// [TapeWriter::set_producer]: the producer named at each piece head and copied.
    pub producer: String,
    /// [TapeWriter::set_near_policy].
    pub near: NearPolicy,
    /// [TapeWriter::set_clock]: when intervals are up (the system clock with `std`).
    pub clock: ClockRef,
    /// Which CRCs and hashes the pieces this writer starts get ([Integrity]), named in each piece head. A piece the
    /// writer goes on with (a resume, [TapeWriter::begin_piece]) keeps the level its head names.
    pub integrity: Integrity,
}

impl Default for WriterOptions {
    fn default() -> Self {
        Self {
            sync: SyncPolicy::default(),
            index: IndexPolicy::default(),
            layout: Layout::UNALIGNED,
            producer: String::new(),
            near: NearPolicy::default(),
            clock: ClockRef::default(),
            integrity: Integrity::Full,
        }
    }
}

/// Appends blocks to a [TapeSink], a file by default, see the [module docs](self).
pub struct TapeWriter<S: TapeSink> {
    sink: S,
    policy: SyncPolicy,
    unsynced: u32,
    clock: ClockRef,
    /// When the writer last synced, by [Self::clock]
    last_sync: Option<u64>,
    /// [Self::bytes] at the last sync
    synced: u64,
    failed: bool,
    blocks: u64,
    bytes: u64,
    /// Length of the file before this writer appended to it
    start: u64,
    /// How blocks are placed; its alignment (0 or [crate::ALIGNMENT]) is where chunk payloads go
    layout: Layout,
    /// The producer named at each piece head
    producer: String,
    /// The layout and the producer are put: after the piece head, or before the first block of a file without one
    head_put: bool,
    /// The layout and the producer as put ([Self::ensure_head]), copied at a use as track 0's definitions are; none
    /// before they are put
    head_items: Vec<Candidate<'static>>,
    /// The first copies due where the call's run starts, found by [Self::find_first_copies], and the slots it looked
    /// at: buffers a call reuses
    sweep: Vec<Candidate<'static>>,
    sweep_slots: Vec<Slot>,
    /// Seq of the next block placed by the writer: after the last block written
    next_seq: u64,
    /// Session of the blocks the writer makes: its own, or the session of the last counted block written
    session: SessionTag,
    /// Small blocks waiting for the gap before an aligned chunk payload, each with the definitions it needs
    deferred: Vec<Deferred>,
    deferred_bytes: usize,
    /// Reused for encoding chunk blocks
    chunk_buf: Vec<u8>,
    /// Reused for a chunk's column specs, the definitions it needs and those chosen: a chunk allocates none of them
    chunk_specs: Vec<crate::ColumnSpec>,
    chunk_needs: Vec<Candidate<'static>>,
    chunk_chosen: Vec<bool>,
    /// What is placed in the piece, for its checkpoints, final index and close
    indexer: Indexer,
    /// A checkpoint waits for the gap before the next aligned chunk
    index_due: bool,
    /// The last block written belongs with the block after it ([FLAG_WITH_NEXT](crate::FLAG_WITH_NEXT)): the writer
    /// places none of its own blocks until the unit ends
    unit_open: bool,
    /// The open run is a caller's: its last block, data the caller gave, is marked
    /// [FLAG_WITH_NEXT](crate::FLAG_WITH_NEXT). A put now goes inside it, marked, never ending it
    caller_run: bool,
    /// The chunks of the open unit so far, all marked [FLAG_WITH_NEXT](crate::FLAG_WITH_NEXT); 0 with none open
    unit_chunks: usize,
    /// The blocks of the open run so far, all marked [FLAG_WITH_NEXT](crate::FLAG_WITH_NEXT); 0 with none open
    run_blocks: usize,
    /// Each producer's group of puts held until its last block ([Self::put_block]); carried over a cut
    relay_groups: relay::RelayGroups,
    /// The blocks of definitions written twice at once and the copies due ([twice])
    twice: twice::Twice,
    /// The copies due are being written
    twice_writing: bool,
    last_checkpoint: Option<u64>,
    /// The piece's content hash so far: BLAKE3 of its bytes, each chunk payload by its hash; untouched below
    /// [Integrity::Full]
    hash: blake3::Hasher,
    /// The level of this piece, as its head names it; [Integrity::Full] for a file without a piece head
    level: Integrity,
    /// The level of the pieces this writer starts ([WriterOptions::integrity])
    level_next: Integrity,
    /// The piece head written first, and its track starts
    head: Option<(PieceHead, Vec<TrackStart>)>,
    /// The state records of the piece the writer remembers
    near: near::Near,
    near_policy: NearPolicy,
    /// The newest item of each state track, for the next cut to carry
    kept: kept::Kept,
    /// Chunk blocks counted in this piece
    chunks: u64,
    /// The last counted block written: its id, length, CRC and offset, to tell a block written again
    last_counted: Option<(BlockId, usize, [u8; 4], u64)>,
    /// Id of the last block [Self::adopt_block] took, as its producer numbered it
    adopted: Option<BlockId>,
    /// A counted records or chunk block of data is in the piece
    holds_data: bool,
    /// Bytes puts hashed and copied
    work: PutWork,
    /// The relations of the tracks defined in the piece, for each chunk's range
    relations: relations::Relations,
    /// The grid of each series track's newest chunk, for the chunks' grid start marks
    grids: grids::Grids,
    /// Most tracks with data in the piece: [crate::piece::TRACKS_PER_PIECE_MAX], smaller in tests
    pub(crate) tracks_max: usize,
    /// Most tracks the next piece's head carries: [crate::piece::HEAD_TRACKS_MAX], smaller in tests
    pub(crate) head_rows_max: usize,
    /// The close record the writer writes on after, when the piece was closed before
    closed_at: Option<ClosedAt>,
    /// The append head of a resumed closed piece, accounted for but not yet written: it goes out before the first
    /// block, so a writer that closes again with nothing appended leaves the bytes as they were
    append_head: Option<[u8; crate::piece::APPEND_HEAD_LEN]>,
    /// The pieces of the chain the next close names as removed, ascending by seq: those of the close in force, and
    /// those [Self::record_removed] added
    removed: Vec<crate::piece::RemovedPiece>,
    /// [Self::record_removed] added a piece since the close in force: a close writes even with nothing else appended
    removed_added: bool,
    /// The definitions of the tracks the piece head carries are not in the piece yet: the next call given
    /// definitions puts them first ([Self::ensure_carried])
    carried_due: bool,
    /// What the piece's carry takes of [Self::closed_bytes]: the piece head, the layout and the producer, the
    /// definitions, state and keys it carries ([Self::put_carried]), and what a chain counts as carry
    /// ([PieceChain::as_carry](crate::piece::PieceChain::as_carry)). A cut policy counts the bytes past it
    /// ([Self::data_bytes]).
    carried: u64,
    /// The piece head's run of blocks goes on: each block placed so far after the head belongs with the next
    head_run: bool,
    /// The tracks the piece's carry put first ([Self::want] of the call a cut was made for): a cut for them again,
    /// with nothing past the carry, would make the same piece
    carried_for: Vec<TrackNum>,
    /// The tracks of the newest call that wrote or was to write data or a put: the next cut carries their
    /// definitions first ([Self::want])
    wanted: Vec<TrackNum>,
    /// The close in force of a resumed closed piece, before which the writer hasn't read the keys yet, the track
    /// each number stood for there, and the keys of the pieces before it ([Self::load_keys])
    keys_base: Option<(u64, BTreeMap<TrackNum, TrackIdent>, kept::Kept)>,
    /// The tracks known when the writer first met bytes that may hold keyed records of tracks it doesn't know, or
    /// those of the piece head's rows when the head says the pieces before met such bytes
    /// ([PieceHead::unlisted_keys_unknown]): the keys of every other keyed track are doubted, and the next head and
    /// this piece's close say so. None while no such bytes were met.
    unlisted: Option<BTreeSet<TrackIdent>>,
    /// The state records of the group of puts met last, held until it ends whole ([group_records])
    group_records: group_records::GroupRecords,
}

/// What a cut starts the next piece with: this piece's head, the next head's rows and the definitions of their tracks.
struct NextRows {
    head: PieceHead,
    tracks: Vec<TrackStart>,
    carried: Vec<Vec<Candidate<'static>>>,
    /// [Closed::states_unknown]
    states_unknown: usize,
    /// [Closed::keys_unknown]
    keys_unknown: usize,
}

/// A close record of the piece a writer writes on after: where it and its tail are, and its hash.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct ClosedAt {
    pub(crate) offset: u64,
    pub(crate) tail: u64,
    pub(crate) hash: ContentHash,
}

/// Bytes a writer's puts hashed and copied since it started ([TapeWriter::put_work]): what deciding and writing them
/// costs.
///
/// A definition's hash is kept with it ([Definitions]), so a block of data whose definitions all lie close behind costs
/// none of either.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct PutWork {
    /// Bytes of items hashed: a caller's items put, a relayed block's records.
    pub hashed: exact_units::Bytes<u64>,
    /// Bytes of items copied into blocks of puts: those written.
    pub copied: exact_units::Bytes<u64>,
}

impl PutWork {
    fn hash(&mut self, len: usize) {
        self.hashed = exact_units::Bytes::new(self.hashed.get().saturating_add(wide(len)));
    }

    fn copy(&mut self, len: usize) {
        self.copied = exact_units::Bytes::new(self.copied.get().saturating_add(wide(len)));
    }
}

/// State entries the layout and the producer take after a piece head ([IndexPolicy::state_entries_max] is at least
/// this).
pub const HEAD_ENTRIES: usize = 2;

/// What a closed piece is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Closed {
    /// BLAKE3 of the piece's bytes before its close record.
    pub hash: ContentHash,
    /// Bytes of the piece, its tail included.
    pub len: u64,
    pub ident: Option<PieceIdent>,
    /// The next piece's id when the piece was [cut](TapeWriter::cut_with).
    pub next: Option<PieceId>,
    /// Where the piece starts in its file or memory tape: 0 for a piece of its own, past the pieces before it in
    /// a file of several.
    pub start: u64,
    /// Every track of the chain so far (carried over by the piece head, or with data in the piece), ascending id
    /// then owner: its [hash](crate::TrackHash) after this piece and where its items go on. After a cut, the next
    /// head's rows: [TrackStart::not_carried] marks a track whose definition the next piece doesn't carry.
    pub tracks: Vec<TrackStart>,
    /// After a cut, the state tracks whose definition the next piece carries without their newest item: the writer
    /// didn't hold it, forgotten past [NearPolicy::kept_bytes_max] or lying only in a piece before a resume. The next
    /// put of such a track writes its item. 0 for a piece not cut.
    pub states_unknown: usize,
    /// After a cut, the keyed state tracks the next piece carries without knowing every live key: a record of
    /// theirs didn't read, or lies only in a piece before a resume. Their rows aren't marked
    /// [TrackStart::keys_carried], so a reader of the next piece alone names their keys as not carried. 0 for a
    /// piece not cut.
    pub keys_unknown: usize,
}

impl Closed {
    /// How many tracks' definitions the next piece doesn't carry, its cut having no room for them
    /// ([TrackStart::not_carried]); 0 for a piece not cut.
    pub fn not_carried(&self) -> usize {
        self.tracks.iter().filter(|t| t.not_carried).count()
    }
}

/// Where [TapeWriter::write_chunk], [TapeWriter::write_block] or [TapeWriter::adopt_block] placed the block it was
/// asked to write.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Placed {
    /// Where the block starts, from the start of the piece.
    pub offset: u64,
    /// The whole block's length.
    pub len: u32,
    /// The chunk's unit hash ([TrackHash::chunk_unit]) as this writer states it in the chunk's index entry and folds
    /// it into its track's hash; [UnitHash::NONE] for a block that is no chunk. Stated, not verified: the same value
    /// a reader takes from the index entry, so a writer in the same process can fold range hashes without reading
    /// the piece back.
    pub unit: UnitHash,
}

/// A chunk of a unit [TapeWriter::write_chunk_unit] writes: its head, its columns and, when the producer hashed it,
/// the hash of its payload ([columns_payload_hash](crate::piece::columns_payload_hash)).
#[derive(Copy, Clone, Debug)]
pub struct UnitChunk<'a> {
    /// The chunk's head.
    pub head: &'a crate::ChunkHead,
    /// Its columns, as [TapeWriter::write_chunk] takes them.
    pub columns: &'a [(crate::ColumnSpec, &'a [u8])],
    /// The hash of its payload, as [TapeWriter::write_chunk_hashed] takes it; None to hash it on the writer.
    pub payload_hash: Option<[u8; 32]>,
}

/// Most bytes of a block of definitions or of puts the writer makes; a longer record gets a block of its own.
pub const DEFINITION_BLOCK_LEN: usize = 64 << 10;

/// Most bytes of the items of one group of puts ([TapeWriter::put_group]).
pub const GROUP_BYTES_MAX: usize = 64 << 20;

const _: () = {
    // a group's puts of items of MAX_RECORD_HEAD bytes or more: a block holds at least half of
    // DEFINITION_BLOCK_LEN of items, or one longer item; shorter items may take more blocks (InputError::RunTooLong)
    let group_blocks = 2 * GROUP_BYTES_MAX / DEFINITION_BLOCK_LEN + 1;
    assert!(
        group_blocks < crate::RUN_BLOCKS_MAX,
        "a group of GROUP_BYTES_MAX bytes of items of MAX_RECORD_HEAD bytes or more fits a run"
    );
    assert!(
        2 * UNIT_CHUNKS_MAX < crate::RUN_BLOCKS_MAX,
        "a unit's chunks fit a run with their puts"
    );
};

/// The buffers a chunk is written through ([TapeWriter::write_chunk]), kept by the writer between calls.
struct ChunkBufs {
    specs: Vec<crate::ColumnSpec>,
    needs: Vec<Candidate<'static>>,
    chosen: Vec<bool>,
}

/// How the blocks of puts a call writes end ([FLAG_WITH_NEXT](crate::FLAG_WITH_NEXT)).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum PutsEnd {
    /// The last block is unmarked: the puts stand on their own.
    Last,
    /// Every block is marked: a block of data they serve follows.
    WithNext,
    /// The last block is unmarked, every block is marked [FLAG_GROUP](crate::FLAG_GROUP) and the first one
    /// [FLAG_GROUP_START](crate::FLAG_GROUP_START): a group of puts.
    Group,
}

/// A put of a group ([TapeWriter::put_group]): `item` on state track `track` at `ticks` of its clock, or a key's
/// value or deletion on a keyed one.
#[derive(Copy, Clone, Debug)]
pub struct GroupPut<'a> {
    /// The state track.
    pub track: TrackNum,
    /// The time of the put, in ticks of the track's clock.
    pub ticks: i64,
    /// The item, or a key's value or deletion on a keyed track.
    pub item: GroupItem<'a>,
}

/// What a put of a group puts ([GroupPut]).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum GroupItem<'a> {
    /// The item of a state track that isn't keyed, as [TapeWriter::put] puts it.
    State(&'a [u8]),
    /// A key's value, or its deletion when the value is None, on a keyed state track, as [TapeWriter::put_key] and
    /// [TapeWriter::delete_key] put them.
    Key(crate::KeyedItem<'a>),
}

impl GroupItem<'_> {
    /// Bytes of the record's item: a key's head, key and value ([crate::KeyedItem::len]).
    pub fn len(&self) -> usize {
        match self {
            Self::State(item) => item.len(),
            Self::Key(k) => k.len(),
        }
    }

    /// Whether the item has no bytes: a key's item never.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The key, for a key's value or deletion.
    fn key(&self) -> Option<&[u8]> {
        match self {
            Self::State(_) => None,
            Self::Key(k) => Some(k.key),
        }
    }
}

impl<'a> GroupPut<'a> {
    /// The put of `item` on state track `track` at `ticks`.
    pub fn state(track: TrackNum, ticks: i64, item: &'a [u8]) -> Self {
        Self {
            track,
            ticks,
            item: GroupItem::State(item),
        }
    }

    /// The put of value `value` of key `key` on keyed state track `track` at `ticks`.
    pub fn key(track: TrackNum, ticks: i64, key: &'a [u8], value: &'a [u8]) -> Self {
        Self {
            track,
            ticks,
            item: GroupItem::Key(crate::KeyedItem {
                key,
                value: Some(value),
            }),
        }
    }

    /// The deletion of key `key` on keyed state track `track` at `ticks`.
    pub fn delete_key(track: TrackNum, ticks: i64, key: &'a [u8]) -> Self {
        Self {
            track,
            ticks,
            item: GroupItem::Key(crate::KeyedItem { key, value: None }),
        }
    }
}

impl GroupPut<'_> {
    /// Refuse `puts` as one group when it names a track twice, or a key of a keyed track twice
    /// ([GroupRefusal::SlotTwice]), or its items take more than [GROUP_BYTES_MAX] bytes ([GroupRefusal::TooLong]):
    /// what [TapeWriter::put_group] checks without the definitions.
    pub fn check(puts: &[GroupPut<'_>]) -> Result<(), GroupRefusal> {
        let mut seen = BTreeSet::new();
        let mut bytes = 0usize;
        for p in puts {
            if !seen.insert((p.track, p.item.key())) {
                return Err(GroupRefusal::SlotTwice { track: p.track });
            }
            bytes = bytes.saturating_add(p.item.len());
        }
        if bytes > GROUP_BYTES_MAX {
            return Err(GroupRefusal::TooLong {
                len: bytes,
                max: GROUP_BYTES_MAX,
            });
        }
        Ok(())
    }
}

/// Most bytes of deferred blocks a [TapeWriter] keeps before writing them out anyway.
pub const DEFERRED_BYTES_MAX: usize = 1 << 20;

/// Most chunks of one unit ([TapeWriter::write_chunk_unit]).
///
/// A grid tile chunk and an overview chunk per level, up to
/// [GRID_OVERVIEW_LEVELS_MAX](crate::grid::GRID_OVERVIEW_LEVELS_MAX), make one. A unit holds back the writer's own
/// blocks (a deferred block, a checkpoint) and a cut, so a chunk marked [FLAG_WITH_NEXT](crate::FLAG_WITH_NEXT) that
/// would make a longer unit is refused ([InputError::UnitTooLong]).
pub const UNIT_CHUNKS_MAX: usize = 17;

/// Blocks of a run kept for a call's own puts and block when it adds first copies
/// ([NearPolicy::copy_first](near::NearPolicy::copy_first)) to it: the copies take at most
/// [RUN_BLOCKS_MAX](crate::RUN_BLOCKS_MAX) less these and the run's blocks so far, and the rest wait for the next calls.
/// A call whose run would pass the cap all the same writes no copy.
pub const FIRST_COPY_RUN_RESERVE: usize = 64;

/// Bytes a deferred block may wait for a gap, from where it was deferred: it is placed before a call would carry the
/// piece past that.
///
/// Its definitions are put when it is deferred and none when it is placed, so a deferred block lies at most its tracks'
/// `near` plus this after them ([TapeWriter::defer_block]).
pub const DEFER_WINDOW: u64 = 1 << 20;

impl<S: TapeSink> TapeWriter<S> {
    /// A writer appending to `sink` (empty, or a writer's start is its length with [Self::create_on] and
    /// [TapeWriter::append_on] alike: offsets in the index are from where the writer starts).
    pub fn create_on(sink: S, policy: SyncPolicy) -> Self {
        let clock = ClockRef::default();
        let now = clock.now_ms();
        Self {
            clock,
            sink,
            policy,
            unsynced: 0,
            synced: 0,
            last_sync: now,
            failed: false,
            blocks: 0,
            bytes: 0,
            start: 0,
            layout: Layout::UNALIGNED,
            producer: String::new(),
            head_put: false,
            head_items: Vec::new(),
            sweep: Vec::new(),
            sweep_slots: Vec::new(),
            next_seq: 0,
            session: new_session(),
            deferred: Vec::new(),
            deferred_bytes: 0,
            chunk_buf: Vec::new(),
            chunk_specs: Vec::new(),
            chunk_needs: Vec::new(),
            chunk_chosen: Vec::new(),
            indexer: Indexer::new(IndexPolicy::default()),
            index_due: false,
            unit_open: false,
            caller_run: false,
            unit_chunks: 0,
            run_blocks: 0,
            relay_groups: relay::RelayGroups::default(),
            twice: twice::Twice::default(),
            twice_writing: false,
            last_checkpoint: now,
            hash: blake3::Hasher::new(),
            level: Integrity::Full,
            level_next: Integrity::Full,
            head: None,
            near: near::Near::default(),
            near_policy: NearPolicy::default(),
            kept: kept::Kept::default(),
            chunks: 0,
            last_counted: None,
            adopted: None,
            holds_data: false,
            work: PutWork::default(),
            relations: relations::Relations::default(),
            grids: grids::Grids::default(),
            tracks_max: crate::piece::TRACKS_PER_PIECE_MAX,
            head_rows_max: crate::piece::HEAD_TRACKS_MAX,
            closed_at: None,
            append_head: None,
            removed: Vec::new(),
            removed_added: false,
            carried_due: false,
            keys_base: None,
            carried: 0,
            head_run: false,
            carried_for: Vec::new(),
            wanted: Vec::new(),
            unlisted: None,
            group_records: group_records::GroupRecords::default(),
        }
    }

    /// Set what `options` say, past the sync policy given at creation.
    pub(crate) fn apply(&mut self, options: &WriterOptions) -> Result<(), WriteError<S::Error>> {
        self.set_layout(options.layout)?;
        self.set_producer(&options.producer);
        self.set_index_policy(options.index)?;
        self.set_near_policy(options.near);
        self.set_clock(options.clock.clone());
        self.level_next = options.integrity;
        Ok(())
    }

    /// Read time from `clock` from now on ([ClockRef]); the intervals start again at its reading now.
    pub fn set_clock(&mut self, clock: ClockRef) {
        self.last_sync = clock.now_ms();
        self.last_checkpoint = self.last_sync;
        self.clock = clock;
    }

    /// The clock the writer reads.
    pub fn clock(&self) -> &ClockRef {
        &self.clock
    }

    /// Ask the sink to make room for `bytes` more ([TapeSink::reserve]).
    pub(crate) fn reserve(&mut self, bytes: u64) {
        self.sink.reserve(bytes);
    }

    /// A writer appending to the bytes already in `sink` in a new session, such as a file opened to append or a
    /// memory tape of [MemPieces::open_whole](crate::MemPieces::open_whole): offsets in the index count from the
    /// sink's length now, and the session's blocks start with its definitions, which a reader takes as the next part
    /// of the bytes.
    ///
    /// Any bytes may be there before, and none are checked: bytes that aren't blocks, a tape cut off by a crash, a
    /// closed tape. The session is a piece of its own after them, without a piece head, found by its tail once
    /// closed. A [TapeReader](crate::TapeReader) shows the bytes before as an unclosed piece (its good blocks up
    /// to [PieceInfo::cut](crate::PieceInfo::cut)), or as the closed pieces they are.
    ///
    /// The writer takes no lock: one writer per sink is the business of whoever owns the sink (`ww_tape_fs`'s file
    /// sink holds a lock on its file).
    pub fn append_on(sink: S, policy: SyncPolicy) -> Result<Self, WriteError<S::Error>> {
        let start = sink
            .size()
            .map_err(sink_err("reading the length of the bytes"))?;
        let mut w = Self::create_on(sink, policy);
        w.start = start;
        Ok(w)
    }

    /// The sink, for its bytes ([crate::sink::MemSink::tape]).
    pub fn sink(&self) -> &S {
        &self.sink
    }

    /// Chunk blocks counted in this piece: what [crate::CutPolicy::chunks] counts.
    pub fn chunks(&self) -> u64 {
        self.chunks
    }

    /// How the writer remembers and copies state records ([near]); it applies from the next block on and carries
    /// over to the next piece of a [cut](Self::cut_with).
    pub fn set_near_policy(&mut self, policy: NearPolicy) {
        self.near_policy = policy;
    }

    /// Bytes of the newest state items the writer keeps for its next cut, against [NearPolicy::kept_bytes_max].
    pub fn kept_bytes(&self) -> usize {
        self.kept.bytes()
    }

    /// Slots the writer remembers in this piece, against [NearPolicy::memory_max].
    pub fn near_usage(&self) -> usize {
        self.near.len()
    }

    /// State entries the piece lists, against [IndexPolicy::state_entries_max].
    pub fn state_entries(&self) -> usize {
        self.indexer.states()
    }

    /// Bytes the writer's puts hashed and copied so far ([PutWork]).
    pub fn put_work(&self) -> PutWork {
        self.work
    }

    /// Whether the piece has room for `puts` more state entries and, with `head`, the layout and the producer: the
    /// head checked before a call writes anything, the puts once chosen, so a refused call writes nothing of its own
    /// (deferred blocks it places were taken on before). A deferred block is placed with no puts, so a cut or a
    /// close never meets the cap.
    /// The tracks of `tracks` (numbers held in `defs`) without a row in the close yet, each once.
    fn new_tracks(&self, defs: &Definitions, tracks: &[TrackNum]) -> BTreeSet<TrackIdent> {
        tracks
            .iter()
            .filter_map(|n| defs.track_identity(*n))
            .map(|(id, owner)| (id, OwnerId(owner)))
            .filter(|k| !self.indexer.has_row(k))
            .collect()
    }

    /// Refuse data on `tracks` (numbers held in `defs`): with [InputError::CarriedEntries] when the definitions it
    /// needs fit no piece under the index policy ([Self::admit]); with [InputError::TooManyTracks] or
    /// [InputError::TooManyCarried] when it would bring the tracks with data in the piece, those of the deferred
    /// blocks counted, past [Self::tracks_max], or the next head past [Self::head_rows_max]: the close and the head
    /// must each fit a block. The tracks become those the next cut carries first ([Self::want]).
    fn track_room(
        &mut self,
        defs: &Definitions,
        tracks: &[TrackNum],
    ) -> Result<(), WriteError<S::Error>> {
        self.want(tracks);
        self.admit_data(defs, tracks)?;
        let mut new = self.new_tracks(defs, tracks);
        if new.is_empty() {
            return Ok(());
        }
        for d in &self.deferred {
            new.extend(d.keys.iter().filter(|k| !self.indexer.has_row(k)));
        }
        if self.indexer.rows().saturating_add(new.len()) > self.tracks_max {
            return Err(WriteError::Input(InputError::TooManyTracks {
                max: self.tracks_max,
            }));
        }
        if self.indexer.next_head_rows(&new) > self.head_rows_max {
            return Err(WriteError::Input(InputError::TooManyCarried {
                max: self.head_rows_max,
            }));
        }
        Ok(())
    }

    /// Refuse a call on `tracks` (numbers held in `defs`) with `puts` records of its own whose state records, the
    /// definitions its tracks need and its puts, don't fit an empty piece with the layout, the producer and the keys
    /// every cut carries ([keyed::carry_entries]) under the index policy ([InputError::CarriedEntries]): no cut could
    /// make room for it. Checked before the piece's own room, so a call refused here never cuts a piece
    /// ([crate::PieceChain]).
    ///
    /// The invariant this keeps with [Self::cut_on] and [carried::fit]: a call admitted here fits the piece a cut
    /// made for it, since that cut carries the keyed tracks and then the call's tracks' definitions first. A piece
    /// that refuses an admitted call with [IndexError::StateEntries] is full, and one cut makes room: data on a track
    /// is never refused for good, and no cut made for a call leaves a piece empty.
    ///
    /// At the key limit ([CarryLimit::StateEntries]) the keyed tracks' carry leaves room for one put alone: a call on
    /// a track whose definitions the carry doesn't hold yet, with its puts, needs more and is refused here until keys
    /// are deleted.
    fn admit(
        &mut self,
        defs: &Definitions,
        tracks: &[TrackNum],
        puts: usize,
    ) -> Result<(), WriteError<S::Error>> {
        self.load_keys(defs, false);
        let max = crate::cap_usize(self.indexer.policy().state_entries_max.get());
        let needed = keyed::carry_entries(&self.kept, defs, tracks)
            .saturating_add(puts)
            .saturating_add(HEAD_ENTRIES);
        if needed > max {
            return Err(WriteError::Input(InputError::CarriedEntries {
                needed,
                max,
            }));
        }
        Ok(())
    }

    /// [Self::admit] data on `tracks` (numbers held in `defs`): the state entries its definitions take
    /// ([carried::entries]). A chain checks it before a cut due by its policy ([crate::PieceChain::write_chunk]).
    pub(crate) fn admit_data(
        &mut self,
        defs: &Definitions,
        tracks: &[TrackNum],
    ) -> Result<(), WriteError<S::Error>> {
        self.admit(defs, tracks, 0)
    }

    fn room(&self, puts: usize, head: bool) -> Result<(), WriteError<S::Error>> {
        let head = if head { HEAD_ENTRIES } else { 0 };
        let n = puts.saturating_add(head);
        self.indexer.room_for_states(n).map_err(WriteError::Index)
    }

    /// Put `item` on state track `track` of `defs` at `ticks` of its clock: the producer uses it now, and puts it at
    /// each use. It is written, after the track's definition and what it names where those don't lie close behind,
    /// unless the newest record of the track is the same item at most the track's `near` behind in the piece and no
    /// copy is due ([near]). Deferred blocks are placed first. Whether the item was written: `false` also for one held
    /// behind a waiting group ([Self::puts_waiting]).
    pub fn put(
        &mut self,
        defs: &Definitions,
        track: TrackNum,
        ticks: i64,
        item: &[u8],
    ) -> Result<bool, WriteError<S::Error>> {
        self.check()?;
        self.want(&[track]);
        let facts = defs
            .track(track)
            .ok_or(DescribeError::UnknownTrack { track })?;
        if facts.keyed {
            return Err(DescribeError::Keyed { track }.into());
        }
        self.put_state(defs, track, &facts, ticks, Slot::State { track }, item)
    }

    /// Put the value `value` of key `key` on keyed state track `track` of `defs` at `ticks` of its clock, as
    /// [Self::put] puts an item: the key's newest record and its copies decide per key. A key is at most
    /// [STATE_KEY_LEN_MAX](crate::STATE_KEY_LEN_MAX) bytes. Whether it was written.
    pub fn put_key(
        &mut self,
        defs: &Definitions,
        track: TrackNum,
        ticks: i64,
        key: &[u8],
        value: &[u8],
    ) -> Result<bool, WriteError<S::Error>> {
        self.put_keyed(
            defs,
            track,
            ticks,
            crate::KeyedItem {
                key,
                value: Some(value),
            },
        )
    }

    /// Put the deletion of key `key` on keyed state track `track` of `defs` at `ticks` of its clock: from there on
    /// the key has no value. Written as [Self::put_key] writes a value. Whether it was written.
    pub fn delete_key(
        &mut self,
        defs: &Definitions,
        track: TrackNum,
        ticks: i64,
        key: &[u8],
    ) -> Result<bool, WriteError<S::Error>> {
        self.put_keyed(defs, track, ticks, crate::KeyedItem { key, value: None })
    }

    /// Put the keyed item `keyed` on keyed state track `track` of `defs`.
    fn put_keyed(
        &mut self,
        defs: &Definitions,
        track: TrackNum,
        ticks: i64,
        keyed: crate::KeyedItem<'_>,
    ) -> Result<bool, WriteError<S::Error>> {
        self.check()?;
        self.load_keys(defs, true);
        self.want(&[track]);
        let facts = defs
            .track(track)
            .ok_or(DescribeError::UnknownTrack { track })?;
        if !facts.keyed {
            return Err(match facts.state {
                Some(_) => DescribeError::NotKeyed { track },
                None => DescribeError::NotState { track },
            }
            .into());
        }
        let (put, slot, item) = Self::key_put(defs, track, keyed)?;
        self.check_keys(defs, &[put])?;
        self.put_state(defs, track, &facts, ticks, slot, &item)
    }

    /// The keyed record of `keyed` on keyed state track `track` of `defs`: the put [keyed::check] checks, its slot
    /// and its item's bytes.
    fn key_put<'k>(
        defs: &Definitions,
        track: TrackNum,
        keyed: crate::KeyedItem<'k>,
    ) -> Result<(keyed::KeyPut<'k>, Slot, Vec<u8>), DescribeError> {
        let len = keyed.key.len();
        let key = crate::KeyId::of(keyed.key).ok_or(DescribeError::KeyTooLong { len })?;
        let mut item = Vec::new();
        keyed
            .encode_into(&mut item)
            .ok_or(DescribeError::KeyTooLong { len })?;
        let ident = defs
            .track_identity(track)
            .map(|(id, owner)| (id, OwnerId(owner)))
            .ok_or(DescribeError::UnknownTrack { track })?;
        let put = keyed::KeyPut {
            track,
            ident,
            item: keyed,
            len: item.len(),
        };
        Ok((put, Slot::Keyed { track, key }, item))
    }

    /// [keyed::check] of `puts` behind the keyed puts that wait for a caller's open unit to end, in their order: a
    /// burst behind a waiting group neither collides nor passes the carry's limits unseen.
    fn check_keys(
        &self,
        defs: &Definitions,
        puts: &[keyed::KeyPut<'_>],
    ) -> Result<(), WriteError<S::Error>> {
        let mut all = Vec::new();
        for w in self.relay_groups.waiting() {
            for c in &w.cands {
                if let (Slot::Keyed { track, .. }, RecordTrack::Data(_)) = (c.slot, c.track)
                    && let Some(ident) = defs
                        .track_identity(track)
                        .map(|(id, owner)| (id, OwnerId(owner)))
                {
                    let bytes = c.item.bytes();
                    let item = crate::KeyedItem::read(bytes).map_err(DescribeError::KeyedItem)?;
                    all.push(keyed::KeyPut {
                        track,
                        ident,
                        item,
                        len: bytes.len(),
                    });
                }
            }
        }
        if all.is_empty() {
            return keyed::check(&self.kept, defs, puts, self.key_limits());
        }
        all.extend(puts.iter().copied());
        keyed::check(&self.kept, defs, &all, self.key_limits())
    }

    /// The limits the keys every cut carries keep to ([keyed::check]).
    fn key_limits(&self) -> keyed::Limits {
        keyed::Limits {
            entries_max: crate::cap_usize(self.indexer.policy().state_entries_max.get()),
            bytes_max: self.kept_bytes_max(),
        }
    }

    /// Put `item` in `slot` of state track `track` (of `facts`) at `ticks`, after what the track needs.
    fn put_state(
        &mut self,
        defs: &Definitions,
        track: TrackNum,
        facts: &crate::open::TrackFacts,
        ticks: i64,
        slot: Slot,
        item: &[u8],
    ) -> Result<bool, WriteError<S::Error>> {
        let policy = facts.state.ok_or(DescribeError::NotState { track })?;
        let clock = facts.clock;
        if item.len() > STATE_ITEM_MAX {
            return Err(DescribeError::TooLong { len: item.len() }.into());
        }
        // a state record gives its track a row in the close and the next head
        self.track_room(defs, &[track])?;
        let mut cands = self.needed(defs, &[track]);
        self.work.hash(item.len());
        cands.push(Candidate {
            slot,
            rule: self.rule(policy),
            track: RecordTrack::Data(track),
            at: Some(ClockTime { clock, ticks }),
            hash: ItemHash::of(item),
            item: Item::Given(item),
        });
        self.admit(defs, &[track], 1)?;
        self.room(0, !self.head_put)?;
        self.place_deferred()?;
        self.ensure_carried(defs)?;
        self.put_last(&cands)
    }

    /// Put `puts` as one group: a reader takes all of them or none. Each is a put on its state track as [Self::put]
    /// makes one, or of a key's value or deletion on a keyed state track as [Self::put_key] and [Self::delete_key]
    /// make one, the near rule deciding whether it is written; the records written and the definitions they need
    /// go in blocks of puts one after the other, each but the last marked [FLAG_WITH_NEXT](crate::FLAG_WITH_NEXT),
    /// keyed records in blocks of their own. A crash inside the group leaves a run of marked blocks at the end of
    /// the piece, which a resume drops whole ([Self::resume_on]); a reader of the piece before the resume and a
    /// follower show none of it. Which puts were written, one flag each.
    ///
    /// While a caller's unit is open ([Self::unit_open]) the group waits whole and is written after the unit's last
    /// block: every flag is then `false`, as for puts the near rule passes over, and [Self::puts_waiting] counts it.
    ///
    /// The group is checked whole before anything is written: every track a state track of `defs`, an item
    /// ([GroupItem::State]) on one that isn't keyed ([DescribeError::Keyed]) and a key ([GroupItem::Key]) on one
    /// that is ([DescribeError::NotKeyed]), each item at most [STATE_ITEM_MAX] bytes, no track and no key of a
    /// keyed track twice ([GroupRefusal::SlotTwice]), the items at most [GROUP_BYTES_MAX] bytes together, a key's
    /// as written ([GroupRefusal::TooLong]), the keys as [Self::put_key] checks them, all puts of the group at once
    /// ([DescribeError::KeyCollision] with a live key or another key of the group, [InputError::CarryFull]), the
    /// piece's room for the state entries of all it writes ([IndexError::StateEntries]:
    /// [crate::PieceChain::put_group] cuts first then, never inside), and its blocks with its definitions at most
    /// [RUN_BLOCKS_MAX](crate::RUN_BLOCKS_MAX) ([InputError::RunTooLong]), which only a group of many items shorter
    /// than [MAX_RECORD_HEAD](crate::MAX_RECORD_HEAD) bytes may pass. The last check runs once the blocks are
    /// encoded: on [InputError::RunTooLong] none of the group is written, but the deferred blocks and the
    /// definitions carried from the piece before may already be.
    pub fn put_group(
        &mut self,
        defs: &Definitions,
        puts: &[GroupPut<'_>],
    ) -> Result<Vec<bool>, WriteError<S::Error>> {
        self.check()?;
        if puts.iter().any(|p| matches!(p.item, GroupItem::Key(_))) {
            self.load_keys(defs, true);
        }
        let mut tracks = Vec::with_capacity(puts.len());
        let mut records = Vec::with_capacity(puts.len());
        let mut key_puts = Vec::new();
        for p in puts {
            let track = p.track;
            let facts = defs
                .track(track)
                .ok_or(DescribeError::UnknownTrack { track })?;
            let policy = facts.state.ok_or(DescribeError::NotState { track })?;
            let (slot, item) = match p.item {
                GroupItem::State(item) if !facts.keyed => {
                    (Slot::State { track }, Item::Given(item))
                }
                GroupItem::State(_) => return Err(DescribeError::Keyed { track }.into()),
                GroupItem::Key(k) if facts.keyed => {
                    let (put, slot, item) = Self::key_put(defs, track, k)?;
                    key_puts.push(put);
                    (slot, Item::Owned(item))
                }
                GroupItem::Key(_) => return Err(DescribeError::NotKeyed { track }.into()),
            };
            let len = item.bytes().len();
            if len > STATE_ITEM_MAX {
                return Err(DescribeError::TooLong { len }.into());
            }
            tracks.push(track);
            records.push((track, p.ticks, facts.clock, policy, slot, item));
        }
        GroupPut::check(puts).map_err(|r| WriteError::Input(InputError::Group(r)))?;
        if puts.is_empty() {
            return Ok(Vec::new());
        }
        self.check_keys(defs, &key_puts)?;
        self.want(&tracks);
        self.track_room(defs, &tracks)?;
        let mut cands = self.needed(defs, &tracks);
        let first = cands.len();
        for (track, ticks, clock, policy, slot, item) in records {
            self.work.hash(item.bytes().len());
            cands.push(Candidate {
                slot,
                rule: self.rule(policy),
                track: RecordTrack::Data(track),
                at: Some(ClockTime { clock, ticks }),
                hash: ItemHash::of(item.bytes()),
                item,
            });
        }
        self.admit(defs, &[], entries_max(&cands))?;
        if self.caller_run {
            self.hold_group(cands.into_iter().map(Candidate::owned).collect())?;
            return Ok(vec![false; puts.len()]);
        }
        self.room(0, !self.head_put)?;
        self.place_deferred()?;
        self.ensure_carried(defs)?;
        let chosen = self.put_chosen(&cands, PutsEnd::Group)?;
        Ok(chosen
            .get(first..)
            .map(<[bool]>::to_vec)
            .unwrap_or_default())
    }

    /// Put the typed item `item` on track 0, after its bundle where `defs` holds it and it doesn't lie close behind:
    /// written unless the same item lies within track 0's `near` ([crate::Layout::definitions]) and no copy is due.
    /// Whether it was written: `false` also for one held behind a waiting group ([Self::puts_waiting]).
    pub fn put_item(
        &mut self,
        defs: &Definitions,
        item: &TypedItem<'_>,
    ) -> Result<bool, WriteError<S::Error>> {
        self.check()?;
        let d = Describe::Item(item.clone());
        let bytes = crate::open::encode(&d)?;
        if bytes.len() > STATE_ITEM_MAX {
            return Err(DescribeError::TooLong { len: bytes.len() }.into());
        }
        let rule = self.definitions_rule();
        let mut cands = Vec::new();
        let bundle: Vec<u8> = item.ty.bundle.iter().filter_map(Result::ok).collect();
        if let Some((slot, _)) = defs.bundle(&bundle)
            && let Some(held) = defs.held(&slot)
        {
            cands.push(Candidate::held(
                slot,
                self.definition_rule(defs, &slot),
                held,
            ));
        }
        self.work.hash(bytes.len());
        let hash = ItemHash::of(&bytes);
        cands.push(Candidate {
            slot: Slot::of_definition(&d, || hash),
            rule,
            track: RecordTrack::Definitions,
            at: None,
            hash,
            item: Item::Owned(bytes),
        });
        self.admit(defs, &[], entries_max(&cands))?;
        self.room(0, !self.head_put)?;
        self.place_deferred()?;
        self.ensure_carried(defs)?;
        self.put_last(&cands)
    }

    /// Put the definitions of `tracks` (numbers held in `defs`) with no data: each track's clock chain, maps, bundles
    /// and its own definition ([Definitions::needs]), so a track that never gets data is still in the piece. Each is
    /// written unless it lies within track 0's `near` and no copy is due, as before data. Gives the tracks no row in
    /// the close: a cut carries them no further. Whether any was written: `false` also when held behind a waiting
    /// group ([Self::puts_waiting]).
    pub fn put_definitions(
        &mut self,
        defs: &Definitions,
        tracks: &[TrackNum],
    ) -> Result<bool, WriteError<S::Error>> {
        self.check()?;
        self.want(tracks);
        for t in tracks {
            defs.track(*t)
                .ok_or(DescribeError::UnknownTrack { track: *t })?;
        }
        let cands = self.needed(defs, tracks);
        if cands.is_empty() {
            return Ok(false);
        }
        self.admit(defs, tracks, 0)?;
        self.room(0, !self.head_put)?;
        self.place_deferred()?;
        self.ensure_carried(defs)?;
        let chosen = self.put_chosen(&cands, PutsEnd::Last)?;
        Ok(chosen.contains(&true))
    }

    /// Put `cands` ([Self::choose]); whether the last one was written.
    fn put_last(&mut self, cands: &[Candidate<'_>]) -> Result<bool, WriteError<S::Error>> {
        let chosen = self.put_chosen(cands, PutsEnd::Last)?;
        Ok(chosen.last().copied().unwrap_or(false))
    }

    /// Put `cands` ([Self::choose]), its blocks ending as `end` says; which of
    /// them were written, one flag each.
    /// Refused with nothing written when the piece has no room for the state entries of those chosen.
    fn put_chosen(
        &mut self,
        cands: &[Candidate<'_>],
        end: PutsEnd,
    ) -> Result<Vec<bool>, WriteError<S::Error>> {
        // a put behind a group waiting for the open unit waits too, so the two keep their order
        if self.caller_run && end == PutsEnd::Last && self.relay_groups.has_waiting() {
            self.hold_waiting(cands.iter().cloned().map(Candidate::owned).collect(), false)?;
            return Ok(vec![false; cands.len()]);
        }
        // inside a caller's open run the puts belong with its next block, so they never end it
        let end = if self.caller_run && end == PutsEnd::Last {
            PutsEnd::WithNext
        } else {
            end
        };
        // a call that needs a definition or puts an item uses the layout and the producer: a copy is due as for a
        // definition, where the piece has room for it
        // and the first copies due go into the call's run, the oldest first, with no use
        let mut head: Vec<Candidate<'_>> = if cands.is_empty() {
            Vec::new()
        } else {
            self.head_items.clone()
        };
        self.with_first_copies(&mut head);
        head.retain(|h| !cands.iter().any(|c| c.slot == h.slot));
        let copies = head.len();
        let mut all = head;
        all.extend_from_slice(cands);
        let mut chosen = self.choose(&all)?;
        // copies are skipped where the piece has no room for their state entries: they never refuse a call. Those of
        // the layout, the producer and the first copies go first, then any other
        if self.room(self.entries(&all, &chosen), false).is_err()
            || !self.run_fits(&all, &chosen)?
        {
            all.drain(..copies);
            chosen = self.choose(&all)?;
        }
        if self.room(self.entries(&all, &chosen), false).is_err() {
            self.choose_into(&all, &mut chosen, false)?;
        }
        self.room(self.entries(&all, &chosen), false)?;
        self.write_chosen(&all, &chosen, end)?;
        self.sweep.clear();
        let head = chosen.len().saturating_sub(cands.len());
        chosen.drain(..head);
        Ok(chosen)
    }

    /// How a put of a record of a state track of `policy` decides.
    fn rule(&self, policy: crate::StatePolicy) -> Rule {
        Rule {
            near: policy.near,
            copies: policy.copies,
            first: self.near_policy.copy_first,
            spacing: self.near_policy.copy_spacing,
            twice: false,
        }
    }

    /// [NearPolicy::kept_bytes_max] as the kept items count it.
    fn kept_bytes_max(&self) -> usize {
        crate::cap_usize(self.near_policy.kept_bytes_max.get())
    }

    /// How a put of a definition decides: track 0's policy, the layout's.
    fn definitions_rule(&self) -> Rule {
        self.rule(self.layout.definitions)
    }

    /// How a put of the definition `defs` holds in `slot` decides: [Self::definitions_rule], written twice at once
    /// where a track asks for it ([Definitions::defined_twice]).
    fn definition_rule(&self, defs: &Definitions, slot: &Slot) -> Rule {
        Rule {
            twice: defs.defined_twice(slot),
            ..self.definitions_rule()
        }
    }

    /// The writer's chunk alignment: 0 or [crate::ALIGNMENT].
    fn alignment(&self) -> u32 {
        self.layout.alignment.get()
    }

    /// The definitions data of `tracks` needs ([Definitions::needs]), each once, as candidates to put: shared with
    /// `defs`, nothing copied or hashed.
    fn needed(&self, defs: &Definitions, tracks: &[TrackNum]) -> Vec<Candidate<'static>> {
        let mut out = Vec::new();
        self.needed_into(defs, tracks, &mut out);
        out
    }

    /// [Self::needed] into `out`, cleared first: a buffer the writer keeps allocates nothing for one track.
    fn needed_into(
        &self,
        defs: &Definitions,
        tracks: &[TrackNum],
        out: &mut Vec<Candidate<'static>>,
    ) {
        out.clear();
        if let [track] = tracks {
            defs.each_need(*track, |slot| {
                if let Some(held) = defs.held(&slot) {
                    out.push(Candidate::held(
                        slot,
                        self.definition_rule(defs, &slot),
                        held,
                    ));
                }
            });
            return;
        }
        let mut seen = BTreeSet::new();
        for t in tracks {
            self.push_needs(defs, defs.needs(*t), &mut seen, out);
        }
    }

    /// Push the definitions held in `slots` not `seen` yet onto `out`, as candidates.
    fn push_needs(
        &self,
        defs: &Definitions,
        slots: impl IntoIterator<Item = Slot>,
        seen: &mut BTreeSet<Slot>,
        out: &mut Vec<Candidate<'static>>,
    ) {
        for slot in slots {
            if let Some(held) = defs.held(&slot)
                && seen.insert(slot)
            {
                out.push(Candidate::held(
                    slot,
                    self.definition_rule(defs, &slot),
                    held,
                ));
            }
        }
    }

    /// Which of `cands` a put writes: each whose slot's newest record isn't the same item within its `near` of
    /// where the next block goes after the blocks written for the chosen ones, or is due a copy there; so a record
    /// skipped lies within `near` of the data or item that follows. Choosing one moves that place on and may choose
    /// more: repeated until nothing changes, at most once per candidate.
    fn choose(&self, cands: &[Candidate<'_>]) -> Result<Vec<bool>, WriteError<S::Error>> {
        let mut chosen = Vec::new();
        self.choose_into(cands, &mut chosen, true)?;
        Ok(chosen)
    }

    /// [Self::choose] into `chosen`, its flags set anew: a buffer the writer keeps allocates nothing. Without
    /// `copies`, none is chosen for a copy spaced apart ([near::Near::writes_new]).
    fn choose_into(
        &self,
        cands: &[Candidate<'_>],
        chosen: &mut Vec<bool>,
        copies: bool,
    ) -> Result<(), WriteError<S::Error>> {
        chosen.clear();
        chosen.resize(cands.len(), false);
        loop {
            let end = self
                .offset()
                .saturating_add(wide(self.chosen_len(cands, chosen)?));
            let mut more = false;
            for (c, pick) in cands.iter().zip(chosen.iter_mut()) {
                let rule = self.rule_now(c);
                let writes = if copies {
                    self.near.writes(&c.slot, c.hash, end, rule)
                } else {
                    self.near.writes_new(&c.slot, c.hash, end, rule)
                };
                if !*pick && writes {
                    *pick = true;
                    more = true;
                }
            }
            if !more {
                return Ok(());
            }
        }
    }

    /// Write the records of `cands` that `chosen` marks, in order. The first records of definitions written twice at
    /// once ([Self::first_twice]) go first, into blocks of their own marked [FLAG_TWICE](crate::FLAG_TWICE): their
    /// copies follow the run they end or belong to ([twice]).
    fn write_chosen(
        &mut self,
        cands: &[Candidate<'_>],
        chosen: &[bool],
        end: PutsEnd,
    ) -> Result<(), WriteError<S::Error>> {
        // nothing chosen writes no block
        if !chosen.contains(&true) {
            return Ok(());
        }
        let first = self.chosen_records(cands, chosen, Part::Twice);
        let own = self.chosen_records(cands, chosen, Part::Own);
        let rest = self.chosen_records(cands, chosen, Part::Rest);
        for (r, _) in first.iter().chain(&own).chain(&rest) {
            self.work.copy(r.item.len());
        }
        let group = if end == PutsEnd::Group {
            crate::FLAG_GROUP
        } else {
            0
        };
        // the blocks written twice at once open the run, a group's first among them: one check counts them all
        let mut blocks = self.encode_puts(&first, crate::FLAG_TWICE | group)?;
        blocks.extend(self.encode_puts(&own, group)?);
        blocks.extend(self.encode_puts(&rest, group)?);
        self.write_put_blocks(blocks, end)
    }

    /// The rule `c` is put by now: one written twice at once whose copy is due ([twice]) has the two records it asks,
    /// and is written again only as a copy spaced apart or past `near`.
    fn rule_now(&self, c: &Candidate<'_>) -> Rule {
        if c.rule.twice && self.twice.copy_due(&c.slot) {
            Rule {
                twice: false,
                copies: exact_units::Count::new(c.rule.copies.get().saturating_add(1)),
                ..c.rule
            }
        } else {
            c.rule
        }
    }

    /// The records of `cands` that `chosen` marks of `part`, as [chosen_records] gives them. None allocates nothing.
    fn chosen_records<'c>(
        &self,
        cands: &'c [Candidate<'_>],
        chosen: &[bool],
        part: Part,
    ) -> Vec<PutRef<'c>> {
        cands
            .iter()
            .zip(chosen)
            .filter(|(c, pick)| **pick && self.part(c) == part)
            .map(|(c, _)| put_ref(c))
            .collect()
    }

    /// The blocks a record of `c` written now goes into ([Part]).
    fn part(&self, c: &Candidate<'_>) -> Part {
        if self.first_twice(c) {
            Part::Twice
        } else if matches!(c.slot, Slot::Own { .. }) {
            Part::Own
        } else {
            Part::Rest
        }
    }

    /// Whether a record of `c` written now is the first of a definition written twice at once ([Rule::twice]).
    fn first_twice(&self, c: &Candidate<'_>) -> bool {
        c.rule.twice && self.near.records_of(&c.slot, c.hash) == 0
    }

    /// Bytes of the blocks of puts [Self::write_chosen] writes for the records of `cands` that `chosen` marks: the
    /// copies come after the run, past where its data lands.
    fn chosen_len(
        &self,
        cands: &[Candidate<'_>],
        chosen: &[bool],
    ) -> Result<usize, WriteError<S::Error>> {
        if !chosen.contains(&true) {
            return Ok(0);
        }
        let first = puts_len(&self.chosen_records(cands, chosen, Part::Twice))?;
        let rest = puts_len(&self.chosen_records(cands, chosen, Part::Rest))?;
        let own = puts_len(&self.chosen_records(cands, chosen, Part::Own))?;
        Ok(first.saturating_add(rest).saturating_add(own))
    }

    /// Whether the blocks of puts [Self::write_chosen] writes for `cands` that `chosen` marks, with one block after
    /// them, fit the open run's [RUN_BLOCKS_MAX](crate::RUN_BLOCKS_MAX).
    fn run_fits(
        &self,
        cands: &[Candidate<'_>],
        chosen: &[bool],
    ) -> Result<bool, WriteError<S::Error>> {
        // each record in blocks of its own, a definition written twice with its copy: when that fits, they do
        let upper =
            cands
                .iter()
                .zip(chosen)
                .filter(|(_, pick)| **pick)
                .fold(0usize, |n, (c, _)| {
                    let one = c
                        .item
                        .bytes()
                        .len()
                        .div_ceil(DEFINITION_BLOCK_LEN / 2)
                        .max(1);
                    n.saturating_add(one.saturating_mul(if c.rule.twice { 2 } else { 1 }))
                });
        if self.run_blocks.saturating_add(upper).saturating_add(1) <= crate::RUN_BLOCKS_MAX {
            return Ok(true);
        }
        let mut n = 0usize;
        for part in [Part::Twice, Part::Own, Part::Rest] {
            n = n.saturating_add(puts_blocks(&self.chosen_records(cands, chosen, part))?);
        }
        Ok(self.run_blocks.saturating_add(n).saturating_add(1) <= crate::RUN_BLOCKS_MAX)
    }

    /// The state entries the records of `cands` that `chosen` marks take: two of a first record written twice at
    /// once, its copy's included.
    fn entries(&self, cands: &[Candidate<'_>], chosen: &[bool]) -> usize {
        cands
            .iter()
            .zip(chosen)
            .filter(|(_, pick)| **pick)
            .map(|(c, _)| if self.first_twice(c) { 2 } else { 1 })
            .fold(0usize, usize::saturating_add)
    }

    /// Write the copies due ([twice]), each as the block of puts it copies, belonging with no other block.
    fn write_copies(&mut self) -> Result<(), WriteError<S::Error>> {
        if self.twice_writing {
            return Ok(());
        }
        self.twice_writing = true;
        let mut out = Ok(());
        while let Some(due) = self.twice.next_due() {
            let copy = crate::copy_trusted(due, self.structural_id()).map_err(WriteError::Block);
            if let Err(e) = copy.and_then(|c| self.write_own(&c).map(drop)) {
                out = Err(e);
                break;
            }
        }
        self.twice_writing = false;
        out
    }

    /// Before a block of data needing `cands`: put them, belonging with it.
    fn put_needed(&mut self, cands: &[Candidate<'_>]) -> Result<(), WriteError<S::Error>> {
        self.put_chosen(cands, PutsEnd::WithNext).map(drop)
    }

    /// The blocks of definitions the tracks of head rows `rows` carry ([carried]), as candidates to put, taking at
    /// most `room` state entries ([carried::fit]), and the numbers of the tracks left out: of each row whose track
    /// `defs` holds under the row's number with the row's id and owner and that isn't marked
    /// [TrackStart::not_carried]; another is passed over. The tracks [Self::want] named come first, then those with
    /// data in this piece, newest first.
    fn carried(
        &self,
        defs: &Definitions,
        rows: &[TrackStart],
        room: usize,
    ) -> (Vec<Vec<Candidate<'static>>>, Vec<TrackNum>) {
        let rows: Vec<TrackStart> = rows.iter().filter(|r| !r.not_carried).copied().collect();
        let numbers = carried::numbers(defs, &rows);
        let states = self.carried_states(defs, &rows);
        let lens: BTreeMap<TrackNum, usize> = states
            .iter()
            .map(|(n, (_, s))| (*n, s.item.len()))
            .collect();
        // the live keys of each keyed track, by slot: a key's id is its slot, no two live keys share one
        let mut keys: BTreeMap<TrackNum, Vec<Slot>> = BTreeMap::new();
        let mut key_states: BTreeMap<Slot, (crate::StatePolicy, &kept::State)> = BTreeMap::new();
        for (n, ident) in Self::keyed_rows(defs, &rows) {
            let slots = keys.entry(n).or_default();
            let (Some(k), Some(policy)) = (
                self.kept.keys(n, ident),
                defs.track(n).and_then(|f| f.state),
            ) else {
                continue;
            };
            for (key, st) in &k.live {
                let Some(id) = crate::KeyId::of(key) else {
                    continue;
                };
                let slot = Slot::Keyed { track: n, key: id };
                if key_states.insert(slot, (policy, st)).is_none() {
                    slots.push(slot);
                }
            }
        }
        let key_lens: BTreeMap<Slot, usize> = key_states
            .iter()
            .map(|(s, (_, st))| (*s, st.item.len()))
            .collect();
        let (blocks, dropped) = carried::fit(
            carried::Source {
                defs,
                states: &lens,
                keys: &keys,
                key_lens: &key_lens,
            },
            &numbers,
            &self.wanted,
            &self.ranked(&rows, &numbers),
            room,
        );
        let blocks = blocks
            .iter()
            .map(|b| {
                b.iter()
                    .filter_map(|s| match s {
                        Slot::Keyed { track, .. } => {
                            let (policy, state) = key_states.get(s)?;
                            Some(Candidate {
                                slot: *s,
                                rule: self.rule(*policy),
                                track: RecordTrack::Data(*track),
                                at: state.at,
                                item: Item::Held(state.item.clone()),
                                hash: state.hash,
                            })
                        }
                        Slot::State { track } => {
                            let (policy, state) = states.get(track)?;
                            Some(Candidate {
                                slot: *s,
                                rule: self.rule(*policy),
                                track: RecordTrack::Data(*track),
                                at: state.at,
                                item: Item::Held(state.item.clone()),
                                hash: state.hash,
                            })
                        }
                        _ => defs
                            .held(s)
                            .map(|h| Candidate::held(*s, self.definition_rule(defs, s), h)),
                    })
                    .collect()
            })
            .collect();
        (blocks, dropped)
    }

    /// The newest item the writer keeps ([kept]) of each state track of head rows `rows` that `defs` holds as a state
    /// track under the row's number, id and owner, with the track's policy, by number.
    fn carried_states<'k>(
        &'k self,
        defs: &Definitions,
        rows: &[TrackStart],
    ) -> BTreeMap<TrackNum, (crate::StatePolicy, &'k kept::State)> {
        rows.iter()
            .filter_map(|r| {
                let key = (r.track_id, r.owner);
                (defs.track_identity(r.track_number) == Some((key.0, key.1.0))).then_some(())?;
                let policy = defs.track(r.track_number)?.state?;
                let state = self.kept.get(r.track_number, key)?;
                Some((r.track_number, (policy, state)))
            })
            .collect()
    }

    /// Read the keys in force at the close of a resumed closed piece through its index, once a call given `defs`
    /// needs them: one by key (`keyed`), or one whose `defs` hold a track of that piece as keyed. The keys the
    /// writer learned after the close go over them. An index that doesn't read leaves the keys of the piece's tracks
    /// in doubt ([kept::Kept::all_keys_doubted]): the next cut doesn't mark them carried.
    pub(crate) fn load_keys(&mut self, defs: &Definitions, keyed: bool) {
        let Some((_, idents, _)) = &self.keys_base else {
            // a call: no older keys follow ([Self::keep_after] comes before any)
            self.kept.seal_keys();
            return;
        };
        let needed = keyed
            || idents
                .keys()
                .any(|n| defs.track(*n).is_some_and(|f| f.keyed));
        if !needed {
            return;
        }
        let Some((before, idents, inherited)) = self.keys_base.take() else {
            return;
        };
        let rows = self.head_rows().to_vec();
        let bytes_max = self.kept_bytes_max();
        let mut base = kept::Kept::default();
        let src = resume::SinkSource(&self.sink);
        let read = match crate::TapeReader::open(&src) {
            Ok(r) => resume::read_keys(&r, 0, &rows, &idents, before, bytes_max, &mut base),
            Err(e) => Err(e),
        };
        if read.is_ok() {
            base.keys_after(inherited);
            self.kept.keys_after(base);
            // the plain items placed since the close, forgotten first to make room for the keys
            self.kept.trim(bytes_max);
        } else {
            self.kept.all_keys_doubted(idents.into_values());
        }
        self.kept.seal_keys();
    }

    /// Whether the writer sees every record of keyed state track `number` from its first on when it sees those of
    /// this piece: a piece without a head or first in its chain, a track without a row in the head, or one whose
    /// row is marked [TrackStart::keys_carried]; never for a track known before bytes it couldn't read
    /// ([kept::Kept::is_doubted]), nor before it read the keys of a resumed closed piece ([Self::load_keys]).
    pub(crate) fn keys_from_start(&self, number: TrackNum) -> bool {
        let key = self.indexer.key_of(number, None);
        let doubted = key.is_some_and(|k| self.kept.is_doubted(k));
        let unlisted = self
            .unlisted
            .as_ref()
            .is_some_and(|known| key.is_none_or(|k| !known.contains(&k)));
        if doubted || unlisted || self.keys_base.is_some() {
            return false;
        }
        let Some((head, rows)) = &self.head else {
            return true;
        };
        head.prev.is_none()
            || rows
                .iter()
                .find(|r| r.track_number == number)
                .is_none_or(|r| r.keys_carried)
    }

    /// Bytes met may hold keyed records of tracks the writer doesn't know: a track's definition among them would be
    /// lost with its records. The keys of every keyed track not known now are doubted from here on, in this piece and
    /// the pieces after it ([PieceHead::unlisted_keys_unknown]).
    pub(crate) fn unlisted_doubted(&mut self) {
        if self.unlisted.is_none() {
            self.unlisted = Some(self.indexer.known().collect());
        }
    }

    /// The piece head says the pieces before met bytes that may hold keyed records of tracks without a row
    /// (`unlisted`): the keys of every track but those of its rows are doubted ([Self::unlisted_doubted]).
    pub(crate) fn unlisted_from_head(&mut self, unlisted: bool) {
        if unlisted {
            self.unlisted_doubted();
        }
    }

    /// Whether the writer knows every live key of keyed state track `number` standing for `key` ([kept::Keys]).
    fn keys_complete(&self, number: TrackNum, key: TrackIdent) -> bool {
        self.kept
            .keys(number, key)
            .map_or_else(|| self.keys_from_start(number), |k| k.complete)
    }

    /// The keyed state tracks of head rows `rows` that `defs` holds under the row's number, id and owner.
    fn keyed_rows(defs: &Definitions, rows: &[TrackStart]) -> Vec<(TrackNum, TrackIdent)> {
        rows.iter()
            .filter(|r| defs.track_identity(r.track_number) == Some((r.track_id, r.owner.0)))
            .filter(|r| defs.track(r.track_number).is_some_and(|f| f.keyed))
            .map(|r| (r.track_number, (r.track_id, r.owner)))
            .collect()
    }

    /// Track numbers `numbers` of head rows `rows`, newest data first: those [Self::want] named, then those with a
    /// chunk in this piece by the offset of their newest chunk, then those with records in it, then the others, each
    /// in the order given.
    fn ranked(&self, rows: &[TrackStart], numbers: &[TrackNum]) -> Vec<TrackNum> {
        let mut newest: BTreeMap<TrackId, u64> = BTreeMap::new();
        for e in self.indexer.entries() {
            if e.track_id != RECORDS_TRACK_ID && e.track_id != STATE_TRACK_ID {
                let n = newest.entry(e.track_id).or_default();
                *n = (*n).max(e.offset);
            }
        }
        let touched: BTreeSet<TrackIdent> = self
            .indexer
            .track_ranges()
            .iter()
            .map(|t| (t.track_id, t.owner))
            .collect();
        let by_number: BTreeMap<TrackNum, &TrackStart> =
            rows.iter().map(|r| (r.track_number, r)).collect();
        let rank = |n: TrackNum| {
            let row = by_number.get(&n);
            let at = row.and_then(|r| newest.get(&r.track_id).copied());
            let with = row.is_some_and(|r| touched.contains(&(r.track_id, r.owner)));
            let wanted = self
                .wanted
                .iter()
                .position(|w| *w == n)
                .unwrap_or(usize::MAX);
            (wanted, core::cmp::Reverse(at), !with)
        };
        let mut out = numbers.to_vec();
        out.sort_by_key(|n| rank(*n));
        out
    }

    /// Name `tracks` as those of the next data: the next cut carries their definitions before any other track's, so
    /// data on them that a full piece refused ([IndexError::StateEntries]) fits the next piece. Each call of data or
    /// a put names its tracks itself; a caller cutting between calls names those of the call to come.
    pub fn want(&mut self, tracks: &[TrackNum]) {
        self.wanted.clear();
        self.wanted.extend_from_slice(tracks);
    }

    /// The owners of the tracks whose definitions the piece head carries ([TrackStart::not_carried] not set), each
    /// once, ascending; a track of all-zero owner counts as the piece's own owner's. Empty without a head.
    pub fn carried_owners(&self, defs: &Definitions) -> Vec<OwnerId> {
        let rows: Vec<TrackStart> = self
            .head_rows()
            .iter()
            .filter(|r| !r.not_carried)
            .copied()
            .collect();
        let owners: BTreeSet<OwnerId> = carried::numbers(defs, &rows)
            .into_iter()
            .filter_map(|n| self.track_owner(defs, n))
            .collect();
        owners.into_iter().collect()
    }

    /// The owner of track `number` held in `defs`: its [TrackDef::owner](crate::TrackDef::owner), the piece's own
    /// owner when that is all zeros ([OwnerId::NONE] without a head). None when `defs` holds no such track.
    pub fn track_owner(&self, defs: &Definitions, number: TrackNum) -> Option<OwnerId> {
        let (_, o) = defs.track_identity(number)?;
        if o != [0; 32] {
            return Some(OwnerId(o));
        }
        Some(
            self.head
                .as_ref()
                .map_or(OwnerId::NONE, |(h, _)| h.ident.owner),
        )
    }

    /// The rows of the piece head this writer wrote or resumed after; none without a head.
    fn head_rows(&self) -> &[TrackStart] {
        self.head
            .as_ref()
            .map(|(_, rows)| rows.as_slice())
            .unwrap_or_default()
    }

    /// The layout and the producer ([Self::ensure_head]), then, when due, the definitions of the tracks the piece head
    /// carries ([Self::carried]), belonging with the call's block: what a call given `defs` writes before its own.
    /// Those of rows marked [TrackStart::not_carried] are left out, and so are those past the state entries left.
    /// Then the groups that waited for a caller's unit to end ([Self::write_waiting]).
    fn ensure_carried(&mut self, defs: &Definitions) -> Result<(), WriteError<S::Error>> {
        self.sweep.clear();
        self.carry_head(defs)?;
        if self.relay_groups.has_waiting() {
            self.write_waiting(Some(defs))?;
        }
        self.find_first_copies(defs);
        Ok(())
    }

    /// Find the first copies due where the next block goes ([near::Near::first_copies]) as candidates, into
    /// [Self::sweep]: each definition `defs` holds, the layout, the producer and each plain state item whose one
    /// record lies [NearPolicy::copy_first] behind, still the newest of its slot. A call finds them right before its
    /// run, so a resume after blocks the call placed first decides as the writer did.
    fn find_first_copies(&mut self, defs: &Definitions) {
        let pos = self.offset();
        let mut slots = core::mem::take(&mut self.sweep_slots);
        self.near
            .first_copies(pos, self.near_policy.copy_first, &mut slots);
        self.sweep.clear();
        // as many as the open run has room for, by a bound on their blocks of puts: the rest wait for the next calls,
        // the oldest first. The call's own puts and block take FIRST_COPY_RUN_RESERVE of the room
        let room = crate::RUN_BLOCKS_MAX
            .saturating_sub(self.run_blocks)
            .saturating_sub(FIRST_COPY_RUN_RESERVE);
        let mut bytes = 0usize;
        for slot in &slots {
            let Some(c) = self.copy_candidate(defs, slot) else {
                continue;
            };
            if self.near.first_copy_due(slot, c.hash, pos, c.rule) != Some(true) {
                continue;
            }
            bytes = bytes.saturating_add(c.item.bytes().len());
            // a block of puts holds at least half of DEFINITION_BLOCK_LEN, but a block per part
            if bytes.div_ceil(DEFINITION_BLOCK_LEN / 2).saturating_add(3) > room {
                break;
            }
            self.sweep.push(c);
        }
        self.sweep_slots = slots;
    }

    /// The record a copy in `slot` writes: the layout or the producer as put, the definition `defs` holds, or the
    /// newest item kept of a state track `defs` holds; None for any other.
    fn copy_candidate(&self, defs: &Definitions, slot: &Slot) -> Option<Candidate<'static>> {
        match slot {
            Slot::Layout | Slot::Producer => {
                self.head_items.iter().find(|c| c.slot == *slot).cloned()
            }
            Slot::State { track } => {
                let policy = defs.track(*track)?.state?;
                let (id, owner) = defs.track_identity(*track)?;
                let state = self.kept.get(*track, (id, OwnerId(owner)))?;
                Some(Candidate {
                    slot: *slot,
                    rule: self.rule(policy),
                    track: RecordTrack::Data(*track),
                    at: state.at,
                    item: Item::Held(state.item.clone()),
                    hash: state.hash,
                })
            }
            Slot::Keyed { .. } => None,
            _ => defs
                .held(slot)
                .map(|h| Candidate::held(*slot, self.definition_rule(defs, slot), h)),
        }
    }

    /// `cands` with the first copies ([Self::sweep]) of slots none of them has.
    fn with_first_copies<'c>(&self, cands: &mut Vec<Candidate<'c>>) {
        for c in &self.sweep {
            if !cands.iter().any(|o| o.slot == c.slot) {
                cands.push(c.clone());
            }
        }
    }

    /// [Self::ensure_carried] without the groups waiting.
    fn carry_head(&mut self, defs: &Definitions) -> Result<(), WriteError<S::Error>> {
        self.load_keys(defs, false);
        if !self.carried_due {
            return self.ensure_head();
        }
        let rows = self
            .head
            .as_ref()
            .map(|(_, rows)| rows.clone())
            .unwrap_or_default();
        // a carry the head promises keys for, put again after a crash, needs every one of them and the track; a mark
        // on the row of a track the definitions hold as not keyed is passed over, as a reader passes it over
        for r in rows.iter().filter(|r| r.keys_carried) {
            if defs.track_identity(r.track_number) != Some((r.track_id, r.owner.0)) {
                return Err(ResumeError::KeysUndefined {
                    track: r.track_number,
                }
                .into());
            }
            if defs.track(r.track_number).is_none_or(|f| !f.keyed) {
                continue;
            }
            if !self.keys_complete(r.track_number, (r.track_id, r.owner)) {
                return Err(ResumeError::KeysUnknown {
                    track: r.track_number,
                }
                .into());
            }
        }
        let head = if self.head_put { 0 } else { HEAD_ENTRIES };
        let room = self.indexer.policy().state_entries_max.get();
        let room = crate::cap_usize(room)
            .saturating_sub(self.indexer.states())
            .saturating_sub(head);
        let (blocks, _) = self.carried(defs, &rows, room);
        self.put_carried(&blocks)
    }

    /// Put `blocks`, the definitions of the tracks the piece head carries, after the layout and the producer, every
    /// one of them, each block as it is, belonging with the next block.
    fn put_carried(&mut self, blocks: &[Vec<Candidate<'_>>]) -> Result<(), WriteError<S::Error>> {
        let n = blocks
            .iter()
            .map(|b| self.entries(b, &vec![true; b.len()]))
            .sum();
        self.room(n, !self.head_put)?;
        let before = self.closed_bytes();
        self.ensure_head()?;
        for b in blocks {
            let all = vec![true; b.len()];
            self.write_chosen(b, &all, PutsEnd::WithNext)?;
        }
        self.carried_due = false;
        self.add_carried(self.closed_bytes().saturating_sub(before));
        self.carried_for.clone_from(&self.wanted);
        Ok(())
    }

    /// Write the layout and the producer, whatever lies close behind: after a piece head, before the first block of a
    /// file without one, and where a caller's block starts another session.
    fn ensure_head(&mut self) -> Result<(), WriteError<S::Error>> {
        if self.head_put {
            return Ok(());
        }
        self.head_put = true;
        let items = self.head_candidates()?;
        self.head_items.clone_from(&items);
        // they belong with the call's block; at a piece head a resume puts them again when nothing follows
        self.write_puts(&chosen_records(&items, &[true, true]), PutsEnd::WithNext)
    }

    /// The layout and the producer as candidates to put, on track 0 by its rule.
    fn head_candidates(&self) -> Result<Vec<Candidate<'static>>, WriteError<S::Error>> {
        let rule = self.definitions_rule();
        let layout = Describe::Layout(self.layout);
        let producer = Describe::Producer(Producer {
            name: &self.producer,
        });
        [layout, producer]
            .iter()
            .map(|d| {
                let bytes = crate::open::encode(d)?;
                let hash = ItemHash::of(&bytes);
                Ok(Candidate {
                    slot: Slot::of_definition(d, || hash),
                    rule,
                    track: RecordTrack::Definitions,
                    at: None,
                    item: Item::Held(bytes.into()),
                    hash,
                })
            })
            .collect()
    }

    /// Write `records` as blocks of puts ([Self::encode_puts]) ending as `end` says ([Self::write_put_blocks]).
    fn write_puts(
        &mut self,
        records: &[PutRef<'_>],
        end: PutsEnd,
    ) -> Result<(), WriteError<S::Error>> {
        let flags = if end == PutsEnd::Group {
            crate::FLAG_GROUP
        } else {
            0
        };
        let blocks = self.encode_puts(records, flags)?;
        self.write_put_blocks(blocks, end)
    }

    /// `records` as blocks marked [FLAG_REPEAT](crate::FLAG_REPEAT) and `flags`, with the seq of the last counted
    /// block, not counted themselves.
    fn encode_puts(
        &self,
        records: &[PutRef<'_>],
        flags: u16,
    ) -> Result<Vec<Vec<u8>>, WriteError<S::Error>> {
        let mut blocks = Vec::new();
        if records.is_empty() {
            return Ok(blocks);
        }
        let id = self.structural_id();
        put_blocks(records, id, flags, |b| {
            blocks.push(b.to_vec());
            Ok::<(), DescribeError>(())
        })?;
        Ok(blocks)
    }

    /// Write `blocks` of puts ([Self::encode_puts]), all of them counted toward
    /// [RUN_BLOCKS_MAX](crate::RUN_BLOCKS_MAX) before the first is written; the call that writes them checked their
    /// room first ([Self::room]). Every block but the last is marked [FLAG_WITH_NEXT](crate::FLAG_WITH_NEXT), the
    /// last one too when `end` says the call goes on with a block of data they serve; the first block of a group is
    /// marked [FLAG_GROUP_START](crate::FLAG_GROUP_START).
    fn write_put_blocks(
        &mut self,
        blocks: Vec<Vec<u8>>,
        end: PutsEnd,
    ) -> Result<(), WriteError<S::Error>> {
        let with_next = end == PutsEnd::WithNext;
        let last = blocks.len().saturating_sub(1);
        if blocks.is_empty() {
            return Ok(());
        }
        let marked = if with_next { blocks.len() } else { last };
        self.check_run(marked)?;
        for (i, mut b) in blocks.into_iter().enumerate() {
            if i < last || with_next {
                crate::mark_with_next(&mut b).map_err(WriteError::Block)?;
            }
            if i == 0 && end == PutsEnd::Group {
                crate::mark_group_start(&mut b).map_err(WriteError::Block)?;
            }
            self.write_own(&b)?;
        }
        Ok(())
    }

    /// When checkpoint index blocks are placed (by default every 4 MiB).
    /// A policy whose [IndexPolicy::state_entries_max] has no room for a piece head's [HEAD_ENTRIES] is refused:
    /// every new piece of a chain would fail.
    pub fn set_index_policy(&mut self, policy: IndexPolicy) -> Result<(), WriteError<S::Error>> {
        if crate::cap_usize(policy.state_entries_max.get()) < HEAD_ENTRIES {
            return Err(WriteError::Index(IndexError::StateEntries {
                max: crate::cap_usize(policy.state_entries_max.get()),
                used: HEAD_ENTRIES,
            }));
        }
        self.indexer.set_policy(policy);
        Ok(())
    }

    /// Start the file as a piece of a chain: the piece head block, first in the file. The session of a chain's first piece is
    /// [SessionTag::of_piece] its id, so the same input gives the same bytes.
    pub fn begin_piece(
        &mut self,
        head: &PieceHead,
        tracks: &[TrackStart],
    ) -> Result<(), WriteError<S::Error>> {
        if head.prev.is_none() {
            self.session = SessionTag::of_piece(&head.ident.id);
        }
        self.write_piece_head(head, tracks)
    }

    fn write_piece_head(
        &mut self,
        head: &PieceHead,
        tracks: &[TrackStart],
    ) -> Result<(), WriteError<S::Error>> {
        self.check()?;
        if self.bytes != 0 {
            return Err(WriteError::Index(IndexError::Invalid(
                "a piece head is the first block of a file",
            )));
        }
        let mut buf = vec![0u8; piece_head_len(tracks.len())];
        let n = write_piece_head(&mut buf, self.structural_id(), head, tracks)
            .map_err(WriteError::Index)?;
        buf.truncate(n);
        self.set_level(head.integrity);
        self.indexer.start(tracks);
        self.head_run = true;
        self.write_own(&buf)?;
        self.head = Some((*head, tracks.to_vec()));
        self.unlisted = None;
        self.unlisted_from_head(head.unlisted_keys_unknown);
        self.near.clear();
        // the layout and the producer follow every piece head, the definitions of the tracks it carries them
        self.head_put = false;
        self.carried_due = !tracks.is_empty();
        self.carried_for.clear();
        self.ensure_head()?;
        self.carried = self.closed_bytes();
        Ok(())
    }

    /// Compute the CRCs and hashes of `level` in this piece from its head on.
    pub(crate) fn set_level(&mut self, level: Integrity) {
        self.level = level;
        self.indexer.set_hashed(level == Integrity::Full);
    }

    /// The level of the piece being written, as its head names it ([Integrity::Full] without a head).
    pub fn integrity(&self) -> Integrity {
        self.level
    }

    /// Where the next block goes, from the start of the piece.
    fn offset(&self) -> u64 {
        self.bytes
    }

    /// Where the next block goes, from the start of the file.
    fn at(&self) -> Result<u64, WriteError<S::Error>> {
        self.start
            .checked_add(self.bytes)
            .ok_or(WriteError::Index(IndexError::Invalid(
                "a file past u64::MAX bytes",
            )))
    }

    pub fn blocks(&self) -> u64 {
        self.blocks
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// [Self::bytes] and, at most, the final index the close lists them in: its entries, each
    /// [ENTRY_LEN](crate::index::ENTRY_LEN) bytes, and its blocks' heads, directories and CRCs. What the piece holds
    /// once closed, short of the close and the tail. A piece of puts lists an entry for each small block, so its final
    /// index may outgrow its blocks.
    pub fn closed_bytes(&self) -> u64 {
        let index = u64::try_from(self.indexer.final_bytes_upper()).unwrap_or(u64::MAX);
        self.bytes.saturating_add(index)
    }

    /// The piece's bytes up to the writer's last sync ([Self::bytes] then): durable once the sink's sync is. 0 before
    /// the first sync.
    pub fn synced_bytes(&self) -> u64 {
        self.synced
    }

    /// What the piece's carry takes of [Self::closed_bytes]: its head, the layout and the producer, the definitions,
    /// state and keys it carries, and what a chain counted as carry
    /// ([PieceChain::as_carry](crate::piece::PieceChain::as_carry)). A resume finds the run of blocks after the head
    /// that belong with the next ([FLAG_WITH_NEXT](crate::FLAG_WITH_NEXT)); of a piece resumed after its close, none.
    pub fn carried_bytes(&self) -> u64 {
        self.carried
    }

    /// [Self::closed_bytes] past the carry ([Self::carried_bytes]): what a [CutPolicy](crate::piece::CutPolicy)
    /// counts. 0 while the piece holds its carry alone, however large: a piece grows to its carry and the budget.
    pub fn data_bytes(&self) -> u64 {
        self.closed_bytes().saturating_sub(self.carried)
    }

    /// Count the block `block` of `kind` just placed as carry while the piece head's run goes on: the head and every
    /// block after it that belongs with the next ([FLAG_WITH_NEXT](crate::FLAG_WITH_NEXT)), as a resume counts the
    /// carry. The first call after the carry so counts its puts and the first copies they bring as carry too.
    fn count_head_run(&mut self, kind: BlockKind, block: &[u8]) {
        if !self.head_run {
            return;
        }
        let flags = match (block.get(6), block.get(7)) {
            (Some(lo), Some(hi)) => u16::from_le_bytes([*lo, *hi]),
            _ => 0,
        };
        if kind != BlockKind::PieceHead && flags & crate::FLAG_WITH_NEXT == 0 {
            self.head_run = false;
        } else if !self.carried_due {
            self.carried = self.carried.max(self.closed_bytes());
        }
    }

    /// Count `n` more bytes of the piece as its carry.
    pub(crate) fn add_carried(&mut self, n: u64) {
        self.carried = self.carried.saturating_add(n);
    }

    /// Whether a cut now makes progress: the piece holds data past its carry, or the next data is on other tracks than
    /// those the carry put first ([Self::want]), which a new piece carries first. Else the cut would make the same
    /// piece.
    pub(crate) fn cut_makes_progress(&self) -> bool {
        self.data_bytes() > 0 || self.wanted != self.carried_for
    }

    /// Whether the piece has a piece head: written by [Self::begin_piece], or read back by a resume.
    pub(crate) fn has_head(&self) -> bool {
        self.head.is_some()
    }

    /// Place chunk payloads on multiples of `alignment` (0 or [crate::ALIGNMENT]), counted from the start of the file:
    /// the writer's layout with that alignment ([Self::set_layout]).
    pub fn set_alignment(&mut self, alignment: u32) -> Result<(), WriteError<S::Error>> {
        let layout = Layout {
            alignment: exact_units::Bytes::new(alignment),
            ..self.layout
        };
        self.set_layout(layout)
    }

    /// The layout the file's blocks are placed by, put at each piece head; its alignment is 0 or [crate::ALIGNMENT].
    /// Set before the first block: another layout after it is refused ([InputError::Layout]).
    pub fn set_layout(&mut self, layout: Layout) -> Result<(), WriteError<S::Error>> {
        let alignment = layout.alignment.get();
        if alignment != 0 && alignment != crate::ALIGNMENT {
            return Err(WriteError::Input(InputError::Alignment(alignment)));
        }
        if self.head_put && layout != self.layout {
            return Err(WriteError::Input(InputError::Layout {
                described: self.layout,
                given: layout,
            }));
        }
        self.layout = layout;
        Ok(())
    }

    /// The producer named at each piece head, from the next head on.
    pub fn set_producer(&mut self, name: &str) {
        name.clone_into(&mut self.producer);
    }

    /// Seq the writer gives the next block it places itself ([Self::write_chunk], [Self::defer_block]): one after
    /// the last counted block written. Blocks the file structure adds take the last counted seq and are not counted
    /// ([crate::index::is_structural]).
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Session and seq of the next block the writer places itself, see [Self::next_seq]: what a caller gives a block it
    /// makes for [Self::write_block].
    pub fn next_id(&self) -> BlockId {
        BlockId::new(self.session, self.next_seq)
    }

    /// The session of the blocks the writer makes: random for a new writer (none without `std`, see
    /// [Self::set_session]), [SessionTag::of_piece] for a chain's first piece,
    /// else the session of the last counted block written.
    pub fn session(&self) -> SessionTag {
        self.session
    }

    /// Make the blocks from here on part of session `session` (a deterministic producer's own tag), its seqs from 0. A tag
    /// a writer may not use ([SessionTag::is_session]) is refused.
    pub fn set_session(&mut self, session: SessionTag) -> Result<(), WriteError<S::Error>> {
        if !session.is_session() {
            return Err(WriteError::Input(InputError::Session(session)));
        }
        self.session = session;
        self.next_seq = 0;
        Ok(())
    }

    /// Id of the blocks the file structure adds (index, close, tail, piece head) and of puts: the seq of the last
    /// counted block before them, 0 before the first.
    fn structural_id(&self) -> BlockId {
        BlockId::new(self.session, self.next_seq.saturating_sub(1))
    }

    /// Write `block` at the end: one the writer made itself, or a caller's checked before ([Self::check_data]).
    /// Where it landed in the piece.
    fn write_own(&mut self, block: &[u8]) -> Result<Placed, WriteError<S::Error>> {
        self.write_own_hashed(block, None)
    }

    /// [Self::write_own] of a chunk whose payload hash the caller gives.
    fn write_own_hashed(
        &mut self,
        block: &[u8],
        payload: Option<[u8; 32]>,
    ) -> Result<Placed, WriteError<S::Error>> {
        self.check()?;
        if !self.session.is_session() {
            return Err(WriteError::Input(InputError::Session(self.session)));
        }
        let (Some(header), Some(crc)) = (
            block.first_chunk::<{ crate::HEADER_LEN }>(),
            block.last_chunk::<{ crate::CRC_LEN }>(),
        ) else {
            return Err(WriteError::Block(crate::BlockError::Short {
                needed: crate::HEADER_LEN,
            }));
        };
        // a Block is at most MAX_BLOCK_LEN long: its length fits a u32
        #[expect(
            clippy::map_err_ignore,
            reason = "a TryFromIntError says only that the number is out of range, which this error says"
        )]
        let len = u32::try_from(block.len()).map_err(|_| {
            WriteError::Index(IndexError::Invalid("a block longer than u32::MAX bytes"))
        })?;
        self.end_after(block.len())?;
        if let Some(head) = self.append_head.take()
            && let Err(error) = self.sink.write_all(&head)
        {
            self.failed = true;
            return Err(WriteError::Sink {
                what: "writing the append head",
                error,
            });
        }
        if let Err(error) = self.sink.write_all(block) {
            self.failed = true;
            return Err(WriteError::Sink {
                what: "writing a block",
                error,
            });
        }
        let offset = self.offset();
        let with_next = u16::from_le_bytes([header[6], header[7]]) & crate::FLAG_WITH_NEXT != 0;
        let chunk = matches!(
            crate::BlockKind::from_u8(header[5]),
            crate::BlockKind::Events | crate::BlockKind::Series
        );
        // a marked chunk was checked against UNIT_CHUNKS_MAX ([Self::check_unit]): never near usize::MAX
        self.unit_chunks = match (with_next, chunk) {
            (false, _) => 0,
            (true, true) => self.unit_chunks.saturating_add(1),
            (true, false) => self.unit_chunks,
        };
        // a marked block was checked against RUN_BLOCKS_MAX ([Self::check_run]): never near usize::MAX
        self.run_blocks = if with_next {
            self.run_blocks.saturating_add(1)
        } else {
            0
        };
        self.unit_open = with_next;
        self.caller_run &= with_next;
        let unit = self.account_hashed(block, header, *crc, payload)?;
        self.unsynced = self.unsynced.saturating_add(1);
        let due = match self.policy {
            SyncPolicy::None => false,
            SyncPolicy::EveryBlocks(n) => self.unsynced >= n.max(1),
            SyncPolicy::Interval(t) => self.clock.passed(self.last_sync, t),
        };
        if due {
            self.sync()?;
        }
        self.twice.saw(block);
        self.write_copies()?;
        Ok(Placed { offset, len, unit })
    }

    /// Check a block of the caller's against `defs`: a records, events, series or summary block of this version
    /// whose body this writer knows ([Definitions::check_block]): no definitions, every record on a held track that
    /// isn't a state track. Other kinds (the reserved kind 2 among them), other versions, bodies of an unknown
    /// codec and header flags the kind doesn't take ([caller_flags]) are refused. `b` is read with its CRCs checked ([crate::Block::read]).
    fn check_data(defs: &Definitions, b: &crate::Block<'_>) -> Result<(), WriteError<S::Error>> {
        let ours = matches!(
            b.kind,
            BlockKind::Records
                | BlockKind::CodedRecords
                | BlockKind::Events
                | BlockKind::Series
                | BlockKind::Summary
                | BlockKind::TileSummary
        );
        if !ours || !b.body_known() {
            return Err(WriteError::Input(InputError::Unknown {
                version: b.version,
                kind: b.kind,
            }));
        }
        if b.flags & !caller_flags(b.kind) != 0 {
            return Err(WriteError::Input(InputError::Flags {
                kind: b.kind,
                flags: b.flags,
            }));
        }
        let mut buf = Vec::new();
        let b = b.decode(&mut buf).map_err(WriteError::Block)?;
        defs.check_block(&b).map_err(WriteError::Describe)
    }

    /// Where the piece ends once `len` more bytes are placed.
    fn end_after(&self, len: usize) -> Result<u64, WriteError<S::Error>> {
        self.bytes
            .checked_add(wide(len))
            .ok_or(WriteError::Index(IndexError::Invalid(
                "a file past u64::MAX bytes",
            )))
    }

    /// Note the whole block `block` placed at the end, with its `header` and `crc`: the content hash, the index,
    /// the state records it holds ([near]), the counts, the session and seq. What a resume replays for each block it
    /// keeps. Returns the chunk's unit hash, [UnitHash::NONE] for a block that is no chunk.
    fn account(
        &mut self,
        block: &[u8],
        header: &[u8; crate::HEADER_LEN],
        crc: [u8; crate::CRC_LEN],
    ) -> Result<UnitHash, WriteError<S::Error>> {
        self.account_hashed(block, header, crc, None)
    }

    /// [Self::account] of a chunk whose payload hash the caller gives ([crate::piece::columns_payload_hash]).
    fn account_hashed(
        &mut self,
        block: &[u8],
        header: &[u8; crate::HEADER_LEN],
        crc: [u8; crate::CRC_LEN],
        payload: Option<[u8; 32]>,
    ) -> Result<UnitHash, WriteError<S::Error>> {
        let bytes = self.end_after(block.len())?;
        // below Integrity::Full nothing is hashed: the chunk is only read, by its head
        let (chunk, unit) = match self.level {
            Integrity::Full => {
                let chunk = hash_block(&mut self.hash, block, payload);
                // the unit hash goes into the chunk's index entry and its track's hash: computed once, here
                let unit = chunk.as_ref().map_or(UnitHash::NONE, |(c, payload)| {
                    TrackHash::chunk_unit(c, payload)
                });
                (chunk.map(|(c, _)| c), unit)
            }
            Integrity::CrcOnly | Integrity::None => (
                crate::Block::read_trusted(block)
                    .ok()
                    .and_then(|b| b.chunk()),
                UnitHash::NONE,
            ),
        };
        // the range from the chunk's own column, by the definition placed before it: a resume derives the same
        let range = chunk.as_ref().and_then(|c| self.relations.range(c));
        // blocks the file structure adds are not counted: seq stays the producer's count of its own blocks
        let kind = BlockKind::from_u8(header[5]);
        let flags = u16::from_le_bytes([header[6], header[7]]);
        let repeat = kind.holds_records() && flags & crate::FLAG_REPEAT != 0;
        let counted = !repeat && !crate::index::is_structural(kind);
        let tile = match &chunk {
            Some(c) => self.relations.tile(c),
            None if kind.is_summary() => crate::Block::read_trusted(block)
                .ok()
                .and_then(|b| b.summary_head())
                .and_then(|h| self.relations.summary_tile(&h)),
            None => None,
        };
        let extras = crate::index::ChunkExtras { unit, range, tile };
        let again = self.index(block, crc, counted, extras);
        // a counted block of data goes into its tracks' hashes once: written again with the same bytes it adds
        // nothing, with other bytes it takes the place of the one before
        let tally = counted && again != Again::Same;
        if tally {
            if again == Again::Replaced {
                self.indexer.undo_block();
            }
            self.indexer.begin_block();
            self.grids.placed(
                chunk.as_ref().map(|c| c.head()).as_ref(),
                again == Again::Replaced,
            );
            if let Some(c) = &chunk {
                let head = c.head();
                self.indexer
                    .tally_chunk(head.track_number, head.track_id, &unit);
                if head.kind == BlockKind::Events
                    && let Some(key) = self.indexer.key_of(head.track_number, Some(head.track_id))
                {
                    self.relations.placed(key, c, again == Again::Replaced);
                }
            }
        }
        let at = self.bytes;
        self.bytes = bytes;
        let tracks = self.learn(block, at, tally);
        self.count_head_run(kind, block);
        // counts against thresholds: past u64::MAX they stay due
        self.blocks = self.blocks.saturating_add(1);
        let data = match kind {
            BlockKind::Records | BlockKind::CodedRecords => !tracks.is_empty(),
            BlockKind::Events | BlockKind::Series => true,
            _ => false,
        };
        if counted && data {
            self.holds_data = true;
        }
        if counted && matches!(kind, BlockKind::Events | BlockKind::Series) {
            self.chunks = self.chunks.saturating_add(1);
        }
        // every block is of the writer's session, as a block of another session starts that session
        // ([Self::write_block]): a piece of puts alone resumes in its session
        if let Some(id) = block_id(block) {
            self.session = id.session;
            if counted {
                self.next_seq = id.seq.wrapping_add(1);
            }
        }
        Ok(unit)
    }

    /// Take what the block `block` placed at `at` says ([Self::learn_records]), the state records of a group of puts
    /// once the group is whole ([group_records]).
    fn learn(&mut self, block: &[u8], at: u64, tally: bool) -> Vec<TrackNum> {
        let marks = crate::Block::read_trusted(block)
            .ok()
            .filter(|b| b.kind.holds_records())
            .and_then(|b| crate::group_marks::Marks::of_puts(b.flags));
        let begin = self.group_records.begin(marks);
        self.void_records(begin.voided);
        for h in begin.whole {
            self.take_record(h.slot, h.at, &h.item, h.block_at, true);
        }
        let grouped = marks.is_some_and(|m| m.group);
        let tracks = self.learn_records(block, at, (tally, grouped), begin.take);
        for h in self.group_records.end() {
            self.take_record(h.slot, h.at, &h.item, h.block_at, true);
        }
        tracks
    }

    /// Take what the block `block` placed at `at` says: every state record (track 0's, and every record of a block
    /// marked [FLAG_REPEAT](crate::FLAG_REPEAT)) noted as written there ([near]) as `take` says, the track each
    /// definition of a track names, and, with `tally`, each record of data into its track's hash. Returns the data
    /// tracks it holds records or a chunk of.
    fn learn_records(
        &mut self,
        block: &[u8],
        at: u64,
        (tally, grouped): (bool, bool),
        take: group_records::Take,
    ) -> Vec<TrackNum> {
        // every block placed was made or checked whole before: its CRCs are not checked again
        let Ok(b) = crate::Block::read_trusted(block) else {
            return Vec::new();
        };
        if let Some(c) = b.chunk() {
            return vec![c.head().track_number];
        }
        if !b.kind.holds_records() {
            return Vec::new();
        }
        let mut buf = Vec::new();
        let Ok(b) = b.decode(&mut buf) else {
            return Vec::new();
        };
        let repeat = b.is_repeat();
        let keyed = b.is_keyed();
        let mut tracks = Vec::new();
        for r in b.records().flatten() {
            if let RecordTrack::Data(track) = r.track
                && !repeat
            {
                tracks.push(track);
                if tally {
                    self.indexer
                        .tally_record(track, r.at.map_or(0, |a| a.ticks), r.item);
                }
                continue;
            }
            let Some(slot) = Slot::of(r.track, r.item, keyed) else {
                continue;
            };
            // a state record gives its track a row
            if let Slot::State { track } | Slot::Keyed { track, .. } = slot
                && let Some(key) = self.indexer.key_of(track, None)
            {
                self.indexer.state_placed(key, track);
            }
            if matches!(slot, Slot::Track { .. })
                && let Ok(Describe::Track(t)) =
                    <Describe<'_> as shrink_wrap::DeserializeShrinkWrap>::from_ww_bytes(r.item)
            {
                self.indexer.define(t.number, t.id(), OwnerId(t.owner));
                self.relations
                    .define(t.number, &crate::open::TrackFacts::of(&t));
            }
            match take {
                group_records::Take::Now => self.take_record(slot, r.at, r.item, at, grouped),
                group_records::Take::Hold => self.group_records.hold(slot, r.at, r.item, at),
                group_records::Take::Void => {
                    if let Slot::Keyed { track, .. } = slot {
                        self.doubt_keys(track);
                    }
                }
            }
        }
        tracks.sort_unstable();
        tracks.dedup();
        tracks
    }

    /// Take the state record `item` in `slot` at `at`, of the block placed at `block_at`: noted as lying there
    /// ([near]), a state track's item as in force, a keyed one's key into the next cut's carry. A keyed record of a
    /// track whose definition the writer never read leaves the keys of every track it doesn't know in doubt. A
    /// record of a group (`grouped`) gets no first copy without a use ([near::Near::wrote_in_group]).
    fn take_record(
        &mut self,
        slot: Slot,
        at: Option<ClockTime>,
        item: &[u8],
        block_at: u64,
        grouped: bool,
    ) {
        match slot {
            Slot::State { track } => {
                if let Some(key) = self.indexer.key_of(track, None) {
                    self.kept
                        .placed(track, key, at, item, self.kept_bytes_max());
                }
            }
            Slot::Keyed { track, .. } => match self.indexer.key_of(track, None) {
                Some(key) => {
                    let complete = self.keys_from_start(track);
                    self.kept
                        .key_placed(track, key, at, item, complete, self.kept_bytes_max());
                }
                None => self.unlisted_doubted(),
            },
            _ => {}
        }
        let hash = ItemHash::of(item);
        if grouped {
            self.near
                .wrote_in_group(slot, hash, block_at, &self.near_policy);
        } else {
            self.near
                .wrote(slot, Some(hash), block_at, &self.near_policy);
        }
    }

    /// A keyed record of track `track` not taken: the track's keys may miss it ([Self::take_record]).
    pub(crate) fn doubt_keys(&mut self, track: TrackNum) {
        match self.indexer.key_of(track, None) {
            Some(key) => self.kept.keys_doubted(track, key),
            None => self.unlisted_doubted(),
        }
    }

    /// The records `held` of a group a reader drops, voided by damage or torn: none is taken, and the keys of the
    /// tracks of keyed ones are doubted.
    fn void_records(&mut self, held: Vec<group_records::Held>) {
        for h in held {
            if let Slot::Keyed { track, .. } = h.slot {
                self.doubt_keys(track);
            }
        }
    }

    /// Damage met in a resume: a block that doesn't read or bytes no block explains. It voids the group of puts it
    /// falls in ([group_records]).
    pub(crate) fn group_damaged(&mut self) {
        let held = self.group_records.damage();
        self.void_records(held);
    }

    /// The end of a resume's replay: a group of puts still open, which no block ended, is voided ([group_records]).
    pub(crate) fn groups_replayed(&mut self) {
        let held = self.group_records.finish();
        self.void_records(held);
    }

    /// List `block`, placed here, in the index as readers take it: a counted block with the id of the last one is
    /// that block again, unlisted with the same bytes, in its place with other bytes. A chunk's entry carries
    /// `extras`.
    fn index(
        &mut self,
        block: &[u8],
        crc: [u8; crate::CRC_LEN],
        counted: bool,
        extras: crate::index::ChunkExtras,
    ) -> Again {
        let id = block_id(block);
        let last = self
            .last_counted
            .filter(|(last, ..)| counted && id == Some(*last));
        let again = match last {
            Some((_, len, c, at)) if len == block.len() && c == crc && self.same_as(at, block) => {
                self.indexer.placed_unlisted(block.len());
                Again::Same
            }
            Some((.., offset)) => {
                self.indexer.replaced(offset);
                self.indexer.placed(self.bytes, block, extras);
                Again::Replaced
            }
            None => {
                self.indexer.placed(self.bytes, block, extras);
                Again::New
            }
        };
        if counted && let Some(id) = id {
            self.last_counted = Some((id, block.len(), crc, self.bytes));
        }
        again
    }

    /// Whether the block at `at` holds the bytes of `block`, which has its length and CRC. Below
    /// [Integrity::None] the CRC covers them all; at None a chunk's covers its head only, so its payload is read back
    /// and compared (a block written again is rare). A read that fails says no.
    fn same_as(&self, at: u64, block: &[u8]) -> bool {
        let unchecked =
            crate::bytes::le_u16(block, 6).is_some_and(|f| f & crate::FLAG_PAYLOAD_UNCHECKED != 0);
        if !unchecked {
            return true;
        }
        let mut old = vec![0u8; block.len()];
        self.sink.read_at(at, &mut old).is_ok() && old == block
    }

    /// After every counted block, whoever placed it: a checkpoint when due (in an aligned file, in the gap before
    /// the next chunk).
    fn after_block(&mut self) -> Result<(), WriteError<S::Error>> {
        if self.checkpoint_due() {
            // inside a unit the checkpoint waits for the unit's last block
            if self.alignment() == 0 && !self.unit_open {
                self.place_checkpoint()?;
            } else {
                self.index_due = true;
            }
        }
        Ok(())
    }

    /// Whether the index policy asks for a checkpoint now.
    fn checkpoint_due(&self) -> bool {
        let by_time = self.indexer.policy().interval.is_some_and(|t| {
            self.indexer.pending() > 0 && self.clock.passed(self.last_checkpoint, t)
        });
        self.indexer.due() || by_time
    }

    fn place_checkpoint(&mut self) -> Result<(), WriteError<S::Error>> {
        self.index_due = false;
        self.last_checkpoint = self.clock.now_ms();
        if self.indexer.pending() == 0 {
            return Ok(());
        }
        // a checkpoint holds at most CHECKPOINT_ENTRIES_MAX entries: one block of many state records may need more
        loop {
            let b = self
                .indexer
                .checkpoint(self.offset(), self.structural_id())
                .map_err(WriteError::Index)?;
            self.write_own(&b)?;
            if self.indexer.pending() < crate::index::CHECKPOINT_ENTRIES_MAX {
                return Ok(());
            }
        }
    }

    /// A small summary or data block that may wait for the gap in front of the next aligned chunk payload: checked
    /// against `defs` and the definitions it needs put now, as for a block written now; when it is placed, nothing
    /// is put. It waits at most [DEFER_WINDOW] bytes and lies at most its tracks' `near` plus that after their
    /// definitions; a deferred block is placed in the piece it was deferred in. It is renumbered into the writer's
    /// session when placed ([crate::set_id]), so seqs ascend in the file. In an unaligned file it is written at once;
    /// past [DEFERRED_BYTES_MAX] waiting, all are. Index, close, tail and piece head blocks are the writer's own:
    /// refused with [WriteError::Index]. A producer's block of puts or copies is put at once ([Self::put_block]); a chunk
    /// marked [FLAG_WITH_NEXT](crate::FLAG_WITH_NEXT) is refused with [InputError::Flags].
    pub fn defer_block(
        &mut self,
        defs: &Definitions,
        block: Vec<u8>,
    ) -> Result<(), WriteError<S::Error>> {
        self.check()?;
        let b = crate::Block::read(&block).map_err(WriteError::Block)?;
        if b.is_repeat() {
            return self.put_block(defs, &block);
        }
        // a chunk of a unit goes with the block after it: a deferred one would open the unit wherever it is placed
        if b.flags & crate::FLAG_WITH_NEXT != 0 {
            return Err(WriteError::Input(InputError::Flags {
                kind: b.kind,
                flags: b.flags,
            }));
        }
        let needs = self.defer_needs(defs, &b)?;
        self.check_caller_relations(defs, &b, false)?;
        self.check_caller_grid(&b, false)?;
        if self.alignment() == 0 {
            self.ensure_carried(defs)?;
            self.put_needed(&needs)?;
            return self.place(block);
        }
        self.place_overdue(puts_len_upper(&needs))?;
        self.ensure_carried(defs)?;
        self.put_chosen(&needs, PutsEnd::Last)?;
        // placed before any later block of its track: a chunk after it goes on from its values, filed under its
        // track's key once the definition naming the track's owner is placed
        if let Some(c) = b.chunk()
            && let Some(facts) = defs.track(c.head().track_number)
            && let Some(key) = self
                .indexer
                .key_of(c.head().track_number, Some(c.head().track_id))
        {
            self.relations.deferred(key, &c, &facts);
        }
        if let Some(c) = b.chunk() {
            self.grids.deferred(&c.head());
        }
        let tracks = data_tracks(&b);
        let d = Deferred {
            keys: self.new_tracks(defs, &tracks),
            tracks,
            block,
            at: self.offset(),
        };
        // flushed once over DEFERRED_BYTES_MAX: never near usize::MAX
        self.deferred_bytes = self.deferred_bytes.saturating_add(d.block.len());
        self.deferred.push(d);
        if self.deferred_bytes > DEFERRED_BYTES_MAX {
            self.place_deferred()?;
        }
        Ok(())
    }

    /// Refuse the events chunk `c` when a monotone relation's first value is below the newest one of its track
    /// placed before it (with `replacing`, before the last counted block, whose place it takes): the values of a
    /// monotone relation never decrease along the track, across chunks and pieces.
    fn check_relations(
        &self,
        defs: &Definitions,
        c: &crate::chunk::Chunk<'_>,
        replacing: bool,
    ) -> Result<bool, WriteError<S::Error>> {
        let head = c.head();
        if head.kind != BlockKind::Events {
            return Ok(false);
        }
        let (Some(facts), Some(key)) = (
            defs.track(head.track_number),
            self.indexer.key_of(head.track_number, Some(head.track_id)),
        ) else {
            return Ok(false);
        };
        self.relations
            .check(key, &facts, c, replacing)
            .map_err(WriteError::Describe)
    }

    /// Go on past monotone relation `relation` of track number `track` from `from`: the track's next events chunk is
    /// checked against `from` for that relation, not against the newest value before it, and is marked
    /// [FLAG_RELATION_BREAK](crate::FLAG_RELATION_BREAK) in its header and its index entry, where readers see the
    /// break. From that chunk on the relation's order is checked again. This is how a caller goes on where the chunk
    /// holding the newest value doesn't read ([DescribeError::RelationUnread]); the writer never does it by itself.
    ///
    /// The break waits for the track's next chunk, through cuts; a crash before that chunk drops it, and the
    /// resumed writer refuses as before. A chunk the writer encodes ([Self::write_chunk]) is marked by the writer; a
    /// caller's block ([Self::write_block], [Self::adopt_block], [Self::defer_block]) carries the flag itself
    /// ([crate::mark_relation_break]) exactly when a break waits, else [InputError::Flags]. A track `defs` doesn't
    /// hold is refused ([DescribeError::UnknownTrack]), as is a relation that isn't monotone with stored values
    /// ([DescribeError::NotMonotone]).
    pub fn accept_relation_break(
        &mut self,
        defs: &Definitions,
        track: TrackNum,
        relation: u16,
        from: u64,
    ) -> Result<(), WriteError<S::Error>> {
        self.check()?;
        let (Some(facts), Some((id, _))) = (defs.track(track), defs.track_identity(track)) else {
            return Err(WriteError::Describe(DescribeError::UnknownTrack { track }));
        };
        let monotone = (facts.stored & facts.monotone).contains(relation);
        let key = self.indexer.key_of(track, Some(id));
        let (true, Some(key)) = (monotone, key) else {
            return Err(WriteError::Describe(DescribeError::NotMonotone {
                track,
                relation,
            }));
        };
        self.relations.accept_break(key, relation, from);
        Ok(())
    }

    /// [Self::check_relations] of a caller's chunk block `b`: it is marked
    /// [FLAG_RELATION_BREAK](crate::FLAG_RELATION_BREAK) exactly when a break the caller accepted waits for its
    /// track, else [InputError::Flags].
    fn check_caller_relations(
        &self,
        defs: &Definitions,
        b: &crate::Block<'_>,
        replacing: bool,
    ) -> Result<(), WriteError<S::Error>> {
        let Some(c) = b.chunk() else {
            return Ok(());
        };
        if self.check_relations(defs, &c, replacing)? != c.relation_break() {
            return Err(WriteError::Input(InputError::Flags {
                kind: b.kind,
                flags: b.flags,
            }));
        }
        Ok(())
    }

    /// Refuse a caller's series chunk block `b` of level 0 marked [FLAG_GRID_START](crate::FLAG_GRID_START) other
    /// than due ([grids]), with `replacing` against the newest before the last counted block, whose place it takes;
    /// and an overview chunk marked.
    fn check_caller_grid(
        &self,
        b: &crate::Block<'_>,
        replacing: bool,
    ) -> Result<(), WriteError<S::Error>> {
        let Some(c) = b.chunk() else {
            return Ok(());
        };
        let head = c.head();
        let wrong = match head.level {
            0 => self.grids.due(&head, replacing) != c.grid_start(),
            _ => b.flags & crate::FLAG_GRID_START != 0,
        };
        if head.kind == BlockKind::Series && wrong {
            return Err(WriteError::Input(InputError::Flags {
                kind: b.kind,
                flags: b.flags,
            }));
        }
        Ok(())
    }

    /// Check `block` for [Self::defer_block] and the piece's room for the definitions it needs: those.
    fn defer_needs(
        &mut self,
        defs: &Definitions,
        b: &crate::Block<'_>,
    ) -> Result<Vec<Candidate<'static>>, WriteError<S::Error>> {
        if crate::index::is_structural(b.kind) {
            return Err(WriteError::Index(IndexError::Invalid(
                "index, close, tail and piece head blocks are written by the writer",
            )));
        }
        Self::check_data(defs, b)?;
        let tracks = data_tracks(b);
        self.track_room(defs, &tracks)?;
        let needs = self.needed(defs, &tracks);
        self.room(0, !self.head_put)?;
        Ok(needs)
    }

    /// Place every deferred block now when a call that writes at most `ahead` more bytes would carry the piece past
    /// [DEFER_WINDOW] from where the oldest was deferred.
    fn place_overdue(&mut self, ahead: usize) -> Result<(), WriteError<S::Error>> {
        if self.unit_open {
            return Ok(());
        }
        let due = self.deferred.first().is_some_and(|d| {
            self.offset().saturating_add(wide(ahead)) > d.at.saturating_add(DEFER_WINDOW)
        });
        if due { self.flush_deferred() } else { Ok(()) }
    }

    /// [Self::flush_deferred] where the writer places its own blocks before a call's: none inside a unit, whose
    /// blocks follow each other with nothing between ([FLAG_WITH_NEXT](crate::FLAG_WITH_NEXT)).
    fn place_deferred(&mut self) -> Result<(), WriteError<S::Error>> {
        if self.unit_open {
            return Ok(());
        }
        self.flush_deferred()
    }

    /// Write every deferred block now, in the order they came; on an error the ones not yet placed stay.
    pub fn flush_deferred(&mut self) -> Result<(), WriteError<S::Error>> {
        while !self.deferred.is_empty() {
            let d = self.take_deferred();
            self.place(d.block)?;
        }
        Ok(())
    }

    /// The oldest deferred block, no longer waiting.
    fn take_deferred(&mut self) -> Deferred {
        let d = self.deferred.remove(0);
        self.deferred_bytes = self.deferred_bytes.saturating_sub(d.block.len());
        d
    }

    /// Place a deferred block (checked, and its definitions put, when it was deferred) as the writer's next counted
    /// block, nothing put before it, then what is due after it.
    fn place(&mut self, mut block: Vec<u8>) -> Result<(), WriteError<S::Error>> {
        crate::set_id_trusted(&mut block, self.next_id()).map_err(WriteError::Block)?;
        self.write_own(&block)?;
        self.after_block()
    }

    /// Encode and append a chunk block numbered [Self::next_seq], its track checked against `defs`. In an aligned
    /// file its payload lands on a multiple of the alignment: deferred blocks fill the gap in front of it while they
    /// fit, in the order they came, the rest of the gap is zeros in its head. The chunk is made and checked before
    /// the definitions it needs are put, so a refused chunk leaves nothing of its own. Where the chunk landed.
    pub fn write_chunk(
        &mut self,
        defs: &Definitions,
        head: &crate::ChunkHead,
        columns: &[(crate::ColumnSpec, &[u8])],
    ) -> Result<Placed, WriteError<S::Error>> {
        self.write_chunk_as(defs, head, columns, None, false)
    }

    /// [Self::write_chunk] with the hash of the chunk's payload given: `payload` is
    /// [columns_payload_hash](crate::piece::columns_payload_hash) of `columns`, computed by the producer on its own
    /// thread, so the writer hashes no payload. Stated, not verified (a debug build checks it): a wrong hash makes
    /// the piece's content hash and the track's hash wrong. The bytes written are the same.
    pub fn write_chunk_hashed(
        &mut self,
        defs: &Definitions,
        head: &crate::ChunkHead,
        columns: &[(crate::ColumnSpec, &[u8])],
        payload: [u8; 32],
    ) -> Result<Placed, WriteError<S::Error>> {
        self.write_chunk_as(defs, head, columns, Some(payload), false)
    }

    /// Chunks that belong together, written one after the other as one unit: each but the last marked
    /// [FLAG_WITH_NEXT](crate::FLAG_WITH_NEXT), nothing of the writer's own (a deferred block, a checkpoint) placed
    /// between them, so a resume after a crash inside the unit drops all of it ([Self::resume_on]) and the call is
    /// given again. A grid tile chunk and its overview chunks are such a unit ([grid](crate::grid#overviews)). The
    /// chunks are of one track, else [InputError::UnitTracks] and nothing is written; each is written as
    /// [Self::write_chunk] or, with its payload hash, [Self::write_chunk_hashed] writes it. Every chunk's head and
    /// columns are checked before the first is written. A chunk that fails after that (a sink error, a column shorter
    /// than its spec) leaves a torn unit: the writer stops ([WriteError::Failed]), so no later chunk joins it, and a
    /// resume drops it. Where each landed.
    pub fn write_chunk_unit(
        &mut self,
        defs: &Definitions,
        chunks: &[UnitChunk<'_>],
    ) -> Result<Vec<Placed>, WriteError<S::Error>> {
        let track = chunks.first().map(|c| c.head.track_number);
        if chunks.iter().any(|c| Some(c.head.track_number) != track) {
            return Err(WriteError::Input(InputError::UnitTracks));
        }
        if self.unit_chunks.saturating_add(chunks.len()) > UNIT_CHUNKS_MAX {
            return Err(WriteError::Input(InputError::UnitTooLong {
                max: UNIT_CHUNKS_MAX,
            }));
        }
        // each chunk is checked before the first is written: a refused one leaves no unit open
        for c in chunks {
            self.check_chunk(defs, c.head, c.columns, &mut Vec::new())?;
        }
        let last = chunks.len().saturating_sub(1);
        let mut placed = Vec::with_capacity(chunks.len());
        for (i, c) in chunks.iter().enumerate() {
            match self.write_chunk_as(defs, c.head, c.columns, c.payload_hash, i < last) {
                Ok(p) => placed.push(p),
                // the unit's chunks so far would take the next call's: the writer stops, a resume drops the unit
                Err(e) if i > 0 => {
                    self.failed = true;
                    return Err(e);
                }
                Err(e) => return Err(e),
            }
        }
        Ok(placed)
    }

    /// Refuse a chunk of `kind` marked [FLAG_WITH_NEXT](crate::FLAG_WITH_NEXT) when `with_next` that would make the
    /// open unit longer than [UNIT_CHUNKS_MAX] chunks: with it and one after it, the unit has two more.
    fn check_unit(
        &self,
        kind: crate::BlockKind,
        with_next: bool,
    ) -> Result<(), WriteError<S::Error>> {
        let chunk = matches!(kind, crate::BlockKind::Events | crate::BlockKind::Series);
        if with_next && chunk && self.unit_chunks.saturating_add(2) > UNIT_CHUNKS_MAX {
            return Err(WriteError::Input(InputError::UnitTooLong {
                max: UNIT_CHUNKS_MAX,
            }));
        }
        if with_next {
            self.check_run(1)?;
        }
        Ok(())
    }

    /// Refuse `marked` more blocks marked [FLAG_WITH_NEXT](crate::FLAG_WITH_NEXT) that would make the open run,
    /// with one block after them, longer than [RUN_BLOCKS_MAX](crate::RUN_BLOCKS_MAX) blocks.
    fn check_run(&self, marked: usize) -> Result<(), WriteError<S::Error>> {
        if self.run_blocks.saturating_add(marked).saturating_add(1) > crate::RUN_BLOCKS_MAX {
            return Err(WriteError::Input(InputError::RunTooLong {
                max: crate::RUN_BLOCKS_MAX,
            }));
        }
        Ok(())
    }

    /// The item number the next chunk of track `track` of `defs` starts at: past the last item of the track's chunks
    /// in this piece, else where the piece head says the track goes on, else 0. A bytes track's next part takes it
    /// ([BytesWriter::new](crate::bytes_track::BytesWriter::new)); a resumed piece knows it from its blocks, or from
    /// its head when it holds no chunk of the track.
    pub fn next_item(&self, defs: &Definitions, track: TrackNum) -> crate::ItemNum {
        let Some((id, owner)) = defs.track_identity(track) else {
            return crate::ItemNum::new(0);
        };
        let key = (id, OwnerId(owner));
        if let Some(next) = self.indexer.next_item(key) {
            return next;
        }
        self.head
            .as_ref()
            .and_then(|(_, starts)| {
                starts
                    .iter()
                    .find(|t| (t.track_id, t.owner) == key)
                    .map(|t| t.next_item)
            })
            .unwrap_or(crate::ItemNum::new(0))
    }

    /// Whether the last block written belongs with the block after it ([FLAG_WITH_NEXT](crate::FLAG_WITH_NEXT)): a
    /// unit is open, and a cut now would split it.
    pub fn unit_open(&self) -> bool {
        self.unit_open
    }

    /// Check a chunk's head and column table against `defs`, and its longest length: refused before anything is
    /// allocated or put. The head and the column table are checked again as the chunk is encoded: the payload is the
    /// caller's columns, copied, not read back. The column specs into `specs`, and the longest the block can be.
    fn check_chunk(
        &self,
        defs: &Definitions,
        head: &crate::ChunkHead,
        columns: &[(crate::ColumnSpec, &[u8])],
        specs: &mut Vec<crate::ColumnSpec>,
    ) -> Result<usize, WriteError<S::Error>> {
        defs.check_chunk_head(head).map_err(WriteError::Describe)?;
        specs.clear();
        specs.extend(columns.iter().map(|(c, _)| *c));
        if head.kind == crate::BlockKind::Events {
            defs.check_relation_columns(head.track_number, specs)
                .map_err(WriteError::Describe)?;
        }
        if defs.track(head.track_number).is_some_and(|t| t.bytes) {
            crate::bytes_track::check_part_columns(head, columns).map_err(|error| {
                WriteError::Describe(crate::DescribeError::BytesPart {
                    track: head.track_number,
                    error,
                })
            })?;
            // a part numbered below the track's next item would make two parts of one number
            let next = self.next_item(defs, head.track_number);
            if head.first_item < next {
                return Err(WriteError::Describe(crate::DescribeError::BytesPart {
                    track: head.track_number,
                    error: crate::bytes_track::PartError::Behind {
                        part: head.first_item,
                        next,
                    },
                }));
            }
        }
        // the longest the block can be, its padding at most the alignment
        let chunk_max = crate::chunk::chunk_len(
            head,
            specs,
            crate::from_file::u32_to_usize(self.alignment()),
        );
        if chunk_max > crate::MAX_BLOCK_LEN {
            return Err(WriteError::Chunk(crate::chunk::ChunkError::TooLong {
                len: chunk_max,
            }));
        }
        Ok(chunk_max)
    }

    /// [Self::write_chunk] and its kin, through the buffers the writer keeps for a chunk, given back after.
    fn write_chunk_as(
        &mut self,
        defs: &Definitions,
        head: &crate::ChunkHead,
        columns: &[(crate::ColumnSpec, &[u8])],
        payload_hash: Option<[u8; 32]>,
        with_next: bool,
    ) -> Result<Placed, WriteError<S::Error>> {
        let mut bufs = ChunkBufs {
            specs: core::mem::take(&mut self.chunk_specs),
            needs: core::mem::take(&mut self.chunk_needs),
            chosen: core::mem::take(&mut self.chunk_chosen),
        };
        let r = self.write_chunk_in(defs, head, columns, (payload_hash, with_next), &mut bufs);
        // the definitions held go back to `defs`: nothing kept past the call
        bufs.needs.clear();
        self.chunk_specs = bufs.specs;
        self.chunk_needs = bufs.needs;
        self.chunk_chosen = bufs.chosen;
        r
    }

    fn write_chunk_in(
        &mut self,
        defs: &Definitions,
        head: &crate::ChunkHead,
        columns: &[(crate::ColumnSpec, &[u8])],
        (payload_hash, with_next): (Option<[u8; 32]>, bool),
        bufs: &mut ChunkBufs,
    ) -> Result<Placed, WriteError<S::Error>> {
        self.check()?;
        self.check_unit(head.kind, with_next)?;
        self.want(&[head.track_number]);
        let chunk_max = self.check_chunk(defs, head, columns, &mut bufs.specs)?;
        let specs = &bufs.specs;
        self.needed_into(defs, &[head.track_number], &mut bufs.needs);
        self.track_room(defs, &[head.track_number])?;
        self.room(0, !self.head_put)?;
        self.ensure_carried(defs)?;
        let needed = bufs.needs.len();
        let payload = specs.iter().fold(0usize, |n, c| n.saturating_add(c.len));
        let head_len = crate::chunk::chunk_head_len(head, columns.len(), payload);
        // a deferred block of the chunk's track goes before it, as in an unaligned file: so do those before it
        if self
            .deferred
            .iter()
            .any(|d| d.tracks.contains(&head.track_number))
        {
            self.place_deferred()?;
        }
        // inside a unit nothing of the writer's own comes between its chunks: deferred blocks and a checkpoint wait
        if self.alignment() != 0 && !self.unit_open {
            // what this call may write: the deferred blocks, a checkpoint, the puts and the chunk
            let checkpoint = if self.index_due {
                self.indexer.checkpoint_bytes()
            } else {
                0
            };
            let ahead = self
                .deferred_bytes
                .saturating_add(checkpoint)
                .saturating_add(puts_len_upper(&bufs.needs))
                .saturating_add(chunk_max);
            self.place_overdue(ahead)?;
            loop {
                let gap = crate::chunk::chunk_padding(self.at()?, head_len, self.alignment())
                    .ok_or(WriteError::Input(InputError::Alignment(self.alignment())))?;
                if self
                    .deferred
                    .first()
                    .is_none_or(|next| next.block.len() > gap)
                {
                    break;
                }
                let d = self.take_deferred();
                self.place(d.block)?;
            }
            // a checkpoint goes before the chunk, in the gap when it fits
            if self.index_due {
                self.place_checkpoint()?;
            }
        }
        // right before the chunk, after every counted block this call places: a resume after one of them puts the
        // same
        // the blocks placed above moved the run on: the first copies due where it starts
        self.find_first_copies(defs);
        let mut needs = core::mem::take(&mut bufs.needs);
        self.with_first_copies(&mut needs);
        bufs.needs = needs;
        self.sweep.clear();
        let needs = &mut bufs.needs;
        self.choose_into(needs, &mut bufs.chosen, true)?;
        // copies are skipped where the piece has no room for their state entries, the first copies first: they never
        // refuse a call
        if needs.len() > needed
            && (self.room(self.entries(needs, &bufs.chosen), false).is_err()
                || !self.run_fits(needs, &bufs.chosen)?)
        {
            needs.truncate(needed);
            self.choose_into(needs, &mut bufs.chosen, true)?;
        }
        if self.room(self.entries(needs, &bufs.chosen), false).is_err() {
            self.choose_into(needs, &mut bufs.chosen, false)?;
        }
        let needs = &*needs;
        let chosen = &bufs.chosen;
        self.room(self.entries(needs, chosen), false)?;
        let puts = self.chosen_len(needs, chosen)?;
        let at = self.at()?.saturating_add(wide(puts));
        let padding = crate::chunk::chunk_padding(at, head_len, self.alignment())
            .ok_or(WriteError::Input(InputError::Alignment(self.alignment())))?;
        let len = crate::chunk::chunk_len(head, specs, padding);
        let mut buf = core::mem::take(&mut self.chunk_buf);
        buf.resize(len, 0);
        let crcs = self.level != Integrity::None;
        let r = crate::chunk::encode_chunk(&mut buf, self.next_id(), head, columns, padding, crcs)
            .map_err(WriteError::Chunk)
            .and_then(|n| {
                buf.truncate(n);
                if head.kind == crate::BlockKind::Events {
                    let chunk = crate::chunk::Chunk::read_head(&buf)
                        .map_err(crate::BlockError::Chunk)
                        .map_err(WriteError::Block)?;
                    crate::open::relation_columns_read(&chunk)?;
                    if self.check_relations(defs, &chunk, false)? {
                        crate::mark_relation_break(&mut buf).map_err(WriteError::Block)?;
                    }
                }
                if self.grids.due(head, false) {
                    crate::mark_grid_start_trusted(&mut buf).map_err(WriteError::Block)?;
                }
                if with_next {
                    crate::mark_with_next_trusted(&mut buf).map_err(WriteError::Block)?;
                }
                self.write_chosen(needs, chosen, PutsEnd::WithNext)?;
                if self.at()? != at {
                    return Err(WriteError::Index(IndexError::Invalid(
                        "the puts before a chunk took other bytes than chosen",
                    )));
                }
                self.write_own_hashed(&buf, payload_hash)
            });
        self.chunk_buf = buf;
        let placed = r?;
        self.caller_run = self.unit_open;
        self.after_block()?;
        self.write_waiting_after_unit(defs)?;
        Ok(placed)
    }

    /// Append one whole block (its bytes as [crate::Block::bytes] gives them, its session and seq as made, see
    /// [Self::next_id]), then sync if the policy says so. A block of another session starts that session: the blocks the
    /// writer makes from there on are in it.
    ///
    /// The block is checked against `defs` ([Definitions::check_block]): no definitions (they are held in `defs` and
    /// put by the writer), every record on a held track that isn't a state track and on its clock; a chunk or
    /// summary block's track is held, and no events chunk is on a state track. Then the definitions the block's data
    /// needs are put. A block numbered [Self::next_id] before that is renumbered after it ([crate::set_id]).
    ///
    /// A producer's block of puts or copies ([FLAG_REPEAT](crate::FLAG_REPEAT)) is not written as it is: its records
    /// are put on this tape ([Self::put_block]), where this writer's own near rule decides what is written; `None`
    /// then, there being no single block it landed as. Else where it landed.
    pub fn write_block(
        &mut self,
        defs: &Definitions,
        block: &[u8],
    ) -> Result<Option<Placed>, WriteError<S::Error>> {
        self.check()?;
        let b = crate::Block::read(block).map_err(WriteError::Block)?;
        if b.is_repeat() {
            return self.put_block(defs, block).map(|()| None);
        }
        self.write_read(defs, &b, false)
    }

    /// [Self::write_block] of a block that isn't a block of puts, read with its CRCs checked.
    fn write_read(
        &mut self,
        defs: &Definitions,
        b: &crate::Block<'_>,
        relayed: bool,
    ) -> Result<Option<Placed>, WriteError<S::Error>> {
        let block = b.bytes();
        Self::check_data(defs, b)?;
        self.check_unit(b.kind, b.flags & crate::FLAG_WITH_NEXT != 0)?;
        let tracks = data_tracks(b);
        self.track_room(defs, &tracks)?;
        let needs = self.needed(defs, &tracks);
        // a block of another session starts it: a reader takes a new session where track 0 comes first, so the
        // layout and the producer are put again in it
        let session = block_id(block).filter(|theirs| theirs.session != self.session);
        self.room(0, !self.head_put || session.is_some())?;
        // a block numbered the next id when called goes after the deferred blocks placed first: renumbered
        let asked = self.next_id();
        self.place_deferred()?;
        let renumbered = block_id(block) == Some(asked) && self.next_id() != asked;
        // a counted block with the last one's id takes its place
        let replacing = !renumbered
            && self
                .last_counted
                .is_some_and(|(id, ..)| block_id(block) == Some(id));
        // a relayed series chunk's grid start mark follows this tape's chunks, not the producer's
        let restamped;
        let read;
        let b = match b.chunk() {
            Some(c) if relayed && self.grids.due(&c.head(), replacing) != c.grid_start() => {
                let mut own = block.to_vec();
                crate::mark_grid_start(&mut own, !c.grid_start()).map_err(WriteError::Block)?;
                restamped = own;
                read = crate::Block::read(&restamped).map_err(WriteError::Block)?;
                &read
            }
            _ => b,
        };
        let block = b.bytes();
        self.check_caller_relations(defs, b, replacing)?;
        self.check_caller_grid(b, replacing)?;
        if let Some(theirs) = session {
            self.session = theirs.session;
            self.next_seq = theirs.seq;
            self.head_put = false;
        }
        self.ensure_carried(defs)?;
        self.put_needed(&needs)?;
        let placed = if renumbered {
            let mut own = block.to_vec();
            crate::set_id_trusted(&mut own, self.next_id()).map_err(WriteError::Block)?;
            self.write_own(&own)?
        } else {
            self.write_own(block)?
        };
        self.caller_run = self.unit_open;
        self.after_block()?;
        self.write_waiting_after_unit(defs)?;
        Ok(Some(placed))
    }

    /// Append a block of any session as the writer's own: renumbered into the writer's session and next seq ([crate::set_id]),
    /// then as [Self::write_block]. On a raw medium a session starts only at a piece head naming the session before it, so a
    /// piece there is one session: a writer for raw media passes a producer's blocks (a tap's, of its own session) this way. A
    /// block passed twice gets two seqs, both data. Index, close, tail and piece head blocks are the writer's own:
    /// refused with [WriteError::Index].
    ///
    /// Lost blocks stay visible: a jump in the producer's seq (a subscriber that fell behind) leaves the same jump in
    /// the writer's, so a reader of the adopted session sees the gap. A block of another producer session is a restart, with
    /// no gap.
    ///
    /// A block of puts or copies ([FLAG_REPEAT](crate::FLAG_REPEAT)) is passed to [Self::put_block] instead, `None`
    /// returned: it lands as no single block. Else where it landed.
    pub fn adopt_block(
        &mut self,
        defs: &Definitions,
        block: &[u8],
    ) -> Result<Option<Placed>, WriteError<S::Error>> {
        self.check()?;
        let b = crate::Block::read(block).map_err(WriteError::Block)?;
        if b.is_repeat() {
            return self.put_block(defs, block).map(|()| None);
        }
        let (kind, theirs) = (b.kind, BlockId::new(b.session, b.seq));
        if crate::index::is_structural(kind) {
            return Err(WriteError::Index(IndexError::Invalid(
                "index, close, tail and piece head blocks are written by the writer",
            )));
        }
        Self::check_data(defs, &b)?;
        let tracks = data_tracks(&b);
        self.track_room(defs, &tracks)?;
        let needs = self.needed(defs, &tracks);
        self.room(0, !self.head_put)?;
        self.place_deferred()?;
        self.check_caller_relations(defs, &b, false)?;
        self.check_caller_grid(&b, false)?;
        self.ensure_carried(defs)?;
        self.put_needed(&needs)?;
        if let Some(last) = self.adopted
            && last.session == theirs.session
        {
            let lost = theirs.seq.saturating_sub(last.seq).saturating_sub(1);
            self.next_seq = self.next_seq.wrapping_add(lost);
        }
        self.adopted = Some(theirs);
        let mut own = block.to_vec();
        crate::set_id_trusted(&mut own, self.next_id()).map_err(WriteError::Block)?;
        let placed = self.write_own(&own)?;
        self.caller_run = self.unit_open;
        self.after_block()?;
        self.write_waiting_after_unit(defs)?;
        Ok(Some(placed))
    }

    /// Put the records of a producer's block of puts or copies ([FLAG_REPEAT](crate::FLAG_REPEAT)) on this tape, as
    /// a relay of a live producer passes them on, each with what it needs as [Self::put] does, all decided at once
    /// and written in as few blocks as they fit: a definition of a kind `defs` holds must be the one held (a relay
    /// holds the block's definitions first, [Self::relay_block]) and is put after what it names; a typed item or a
    /// kind this crate doesn't know is put as it is, a typed item after its bundle; a producer's producer and layout
    /// are passed over (this tape's are the writer's own); every other record is put on its state track after the
    /// track's definitions. A record on a track that isn't a state track of `defs`, or on another clock than its
    /// track's, or a definition other than the one held, is refused before anything is put. The block itself is not
    /// written.
    ///
    /// A block of a producer's group ([FLAG_GROUP](crate::FLAG_GROUP)) is held until the group's last block, not
    /// marked [FLAG_WITH_NEXT](crate::FLAG_WITH_NEXT); then the records of all its blocks are put as one group, as
    /// [Self::put_group] puts one. A group is held from its first block ([FLAG_GROUP_START](crate::FLAG_GROUP_START))
    /// on, under the session tag of its producer's blocks, and carried over a cut. Blocks of other producers between
    /// its blocks go through and leave it whole; groups of at most [RELAY_GROUPS_MAX] producers costing at most
    /// [RELAY_HELD_BYTES_MAX] together are held at once, each record its bytes and [RELAY_RECORD_OVERHEAD]. As
    /// [Self::put_group] does, a group puts one item per state track and per key of a keyed one
    /// ([GroupRefusal::SlotTwice]), and its keys are checked all at once at its last block, before any of it is
    /// written ([DescribeError::KeyCollision], [InputError::CarryFull]).
    ///
    /// A group held is never dropped without an error naming it ([InputError::Group]):
    /// - a block of its producer that isn't a group's, or the first block of another group, tears it
    ///   ([GroupRefusal::Torn]): that block is put, the error comes after;
    /// - more than [RUN_BLOCKS_MAX](crate::RUN_BLOCKS_MAX) blocks ([InputError::RunTooLong]), more than
    ///   [GROUP_BYTES_MAX] bytes of items ([GroupRefusal::TooLong]), a state track or a key twice
    ///   ([GroupRefusal::SlotTwice]), keys refused at its last block, more than [RELAY_HELD_BYTES_MAX] held ([GroupRefusal::HeldTooLong]), or a
    ///   block refused, drops it. A first block that tears its producer's group held and is refused itself is
    ///   named [GroupRefusal::Torn]; given again, it names its own refusal.
    ///
    /// A block of a group not held is refused ([GroupRefusal::NoStart]), and so is a first block past
    /// [RELAY_GROUPS_MAX] ([GroupRefusal::TooManyHeld]). A group whose put is refused at its last block (no room:
    /// [IndexError::StateEntries]) stays held without that block, to be given again.
    pub fn put_block(
        &mut self,
        defs: &Definitions,
        block: &[u8],
    ) -> Result<(), WriteError<S::Error>> {
        self.check()?;
        let b = crate::Block::read(block).map_err(WriteError::Block)?;
        let mut buf = Vec::new();
        let b = b.decode(&mut buf).map_err(WriteError::Block)?;
        if !b.kind.holds_records() {
            return Err(WriteError::Input(InputError::Unknown {
                version: b.version,
                kind: b.kind,
            }));
        }
        let session = b.session;
        if b.flags & crate::FLAG_GROUP == 0 {
            let torn = self.relay_groups.take(session);
            if let Err(e) = self.put_one_block(defs, &b) {
                // refused, the block tore nothing yet: given again, it tears the group and says so
                if let Some(g) = torn {
                    self.relay_groups.hold(session, g);
                }
                return Err(e);
            }
            return torn_error(session, torn);
        }
        let held = self.relay_groups.take(session);
        let (mut group, torn) = if b.flags & crate::FLAG_GROUP_START != 0 {
            if held.is_none() && self.relay_groups.len() >= RELAY_GROUPS_MAX {
                return Err(group_refused(GroupRefusal::TooManyHeld {
                    session,
                    max: RELAY_GROUPS_MAX,
                }));
            }
            (RelayGroup::default(), held)
        } else {
            match held {
                Some(g) => (g, None),
                None => return Err(group_refused(GroupRefusal::NoStart { session })),
            }
        };
        let last = b.flags & crate::FLAG_WITH_NEXT == 0;
        let added = self.add_relay_block(defs, &b, &mut group).and_then(|()| {
            if last {
                self.check_relay_keys(defs, &group.cands)?;
            }
            let len = self.relay_groups.held().saturating_add(group.held);
            if len > RELAY_HELD_BYTES_MAX {
                return Err(group_refused(GroupRefusal::HeldTooLong {
                    session,
                    len,
                    max: RELAY_HELD_BYTES_MAX,
                }));
            }
            Ok(())
        });
        if let Err(e) = added {
            // the group torn is the loss to name; the block given again, with nothing held, names its own refusal
            return match torn {
                Some(g) => torn_error(session, Some(g)),
                None => Err(e),
            };
        }
        if !last {
            self.relay_groups.hold(session, group);
            return torn_error(session, torn);
        }
        if let Err(e) = self.put_relay_group(defs, &group.cands) {
            group.undo_last();
            // held as before the block, to be given again: a group of one block holds nothing yet
            match (group.blocks, torn) {
                (0, Some(g)) => self.relay_groups.hold(session, g),
                (0, None) => {}
                _ => self.relay_groups.hold(session, group),
            }
            return Err(e);
        }
        torn_error(session, torn)
    }

    /// What the groups of puts this writer holds for a relay cost, as [RELAY_HELD_BYTES_MAX] counts it: each record's
    /// bytes and [RELAY_RECORD_OVERHEAD].
    pub fn relay_held_bytes(&self) -> usize {
        self.relay_groups.held()
    }

    /// Put the records of block `b` of puts, none of a group, as one call ([Self::put_block]).
    fn put_one_block(
        &mut self,
        defs: &Definitions,
        b: &crate::Block<'_>,
    ) -> Result<(), WriteError<S::Error>> {
        self.load_keys(defs, b.is_keyed());
        let mut cands = Vec::new();
        let mut key_puts = Vec::new();
        self.block_cands(
            defs,
            b,
            &mut BlockSeen::default(),
            (&mut cands, &mut key_puts),
        )?;
        self.check_keys(defs, &key_puts)?;
        self.room(0, !self.head_put)?;
        self.place_deferred()?;
        self.ensure_carried(defs)?;
        self.put_last(&cands).map(drop)
    }

    /// Add the records of block `b` of a producer's group to `group` ([Self::put_block]), refused past
    /// [RUN_BLOCKS_MAX](crate::RUN_BLOCKS_MAX) blocks, [GROUP_BYTES_MAX] bytes of items or a second item on a state
    /// track or a key ([GroupRefusal::SlotTwice]).
    fn add_relay_block(
        &mut self,
        defs: &Definitions,
        b: &crate::Block<'_>,
        group: &mut RelayGroup,
    ) -> Result<(), WriteError<S::Error>> {
        group.before = (
            group.cands.len(),
            group.bytes,
            group.held,
            group.seen.clone(),
        );
        let mut cands = Vec::new();
        self.block_cands(defs, b, &mut group.seen, (&mut cands, &mut Vec::new()))?;
        if let Some(track) = group.seen.twice {
            return Err(group_refused(GroupRefusal::SlotTwice { track }));
        }
        group.blocks = group.blocks.saturating_add(1);
        if group.blocks > crate::RUN_BLOCKS_MAX {
            return Err(WriteError::Input(InputError::RunTooLong {
                max: crate::RUN_BLOCKS_MAX,
            }));
        }
        for c in cands {
            if let Item::Given(item) = c.item {
                group.bytes = group.bytes.saturating_add(item.len());
            }
            group.held = group
                .held
                .saturating_add(c.item.bytes().len())
                .saturating_add(RELAY_RECORD_OVERHEAD);
            group.cands.push(c.owned());
        }
        if group.bytes > GROUP_BYTES_MAX {
            return Err(group_refused(GroupRefusal::TooLong {
                len: group.bytes,
                max: GROUP_BYTES_MAX,
            }));
        }
        Ok(())
    }

    /// Refuse the keyed records of a producer's group held, `cands`, as [Self::put_group] refuses its keys: all at once
    /// ([keyed::check]).
    fn check_relay_keys(
        &mut self,
        defs: &Definitions,
        cands: &[Candidate<'_>],
    ) -> Result<(), WriteError<S::Error>> {
        if !cands.iter().any(|c| matches!(c.slot, Slot::Keyed { .. })) {
            return Ok(());
        }
        self.load_keys(defs, true);
        let mut key_puts = Vec::new();
        for c in cands {
            if let Slot::Keyed { track, .. } = c.slot
                && let Some((id, owner)) = defs.track_identity(track)
            {
                let item = c.item.bytes();
                key_puts.push(keyed::KeyPut {
                    track,
                    ident: (id, OwnerId(owner)),
                    item: crate::KeyedItem::read(item).map_err(DescribeError::KeyedItem)?,
                    len: item.len(),
                });
            }
        }
        self.check_keys(defs, &key_puts)
    }

    /// Put the records of a producer's group held ([Self::put_block]) as one group.
    fn put_relay_group(
        &mut self,
        defs: &Definitions,
        cands: &[Candidate<'_>],
    ) -> Result<(), WriteError<S::Error>> {
        self.admit(defs, &[], entries_max(cands))?;
        if self.caller_run {
            return self.hold_group(cands.iter().cloned().map(Candidate::owned).collect());
        }
        self.room(0, !self.head_put)?;
        self.place_deferred()?;
        self.ensure_carried(defs)?;
        self.put_chosen(cands, PutsEnd::Group).map(|_| ())
    }

    /// Hold the whole group `cands` while a caller's unit is open: its last block, a block of puts, would end the
    /// unit. It is written once the unit ends ([Self::write_waiting]). Refused as a relay's groups are, past
    /// [RELAY_GROUPS_MAX] groups or [RELAY_HELD_BYTES_MAX] bytes held.
    fn hold_group(&mut self, cands: Vec<Candidate<'static>>) -> Result<(), WriteError<S::Error>> {
        if self.relay_groups.len() >= RELAY_GROUPS_MAX {
            return Err(group_refused(GroupRefusal::TooManyHeld {
                session: self.session,
                max: RELAY_GROUPS_MAX,
            }));
        }
        self.hold_waiting(cands, true)
    }

    /// Hold `cands` behind the groups waiting ([Self::hold_group]), as a group or the records of one put: refused
    /// past [RELAY_HELD_BYTES_MAX] bytes held.
    fn hold_waiting(
        &mut self,
        cands: Vec<Candidate<'static>>,
        group: bool,
    ) -> Result<(), WriteError<S::Error>> {
        let cost = cands.iter().fold(0usize, |n, c| {
            n.saturating_add(c.item.bytes().len())
                .saturating_add(RELAY_RECORD_OVERHEAD)
        });
        let len = self.relay_groups.held().saturating_add(cost);
        if len > RELAY_HELD_BYTES_MAX {
            return Err(if group {
                group_refused(GroupRefusal::HeldTooLong {
                    session: self.session,
                    len,
                    max: RELAY_HELD_BYTES_MAX,
                })
            } else {
                WriteError::Input(InputError::WaitingTooLong {
                    len,
                    max: RELAY_HELD_BYTES_MAX,
                })
            });
        }
        self.relay_groups
            .wait(relay::Waiting { cands, cost, group });
        Ok(())
    }

    /// The groups, and the puts behind them, that wait for a caller's unit to end: written after its last block. A
    /// put or group call that returned all `false` while some wait was held, not passed over by the near rule.
    pub fn puts_waiting(&self) -> usize {
        self.relay_groups.waiting().len()
    }

    /// Write the groups and puts that waited for a caller's unit to end, oldest first, each group whole; with `defs`,
    /// after the definitions a new piece carries. One the piece has no room for ([IndexError::StateEntries]) waits
    /// on, with those after it, and the error is returned: a chain cuts and writes it in the next piece.
    fn write_waiting(&mut self, defs: Option<&Definitions>) -> Result<(), WriteError<S::Error>> {
        while !self.caller_run
            && let Some(w) = self.relay_groups.next_waiting()
        {
            let end = if w.group {
                PutsEnd::Group
            } else {
                PutsEnd::Last
            };
            let r = self
                .room(0, !self.head_put)
                .and_then(|()| defs.map_or(Ok(()), |d| self.carry_head(d)))
                .and_then(|()| self.put_chosen(&w.cands, end).map(drop));
            if let Err(e) = r {
                if matches!(e, WriteError::Index(IndexError::StateEntries { .. })) {
                    self.relay_groups.wait_first(w);
                }
                return Err(e);
            }
        }
        Ok(())
    }

    /// Before a close: write the groups and puts waiting, in order. A caller's unit still open ends with them: they
    /// follow its blocks, and the last of them, unmarked, ends its run, so nothing written is cut and the producer's
    /// part of the unit reads as it wrote it. With nothing waiting, the close itself ends the run.
    fn close_waiting(&mut self) -> Result<(), WriteError<S::Error>> {
        self.caller_run = false;
        self.write_waiting(None)
    }

    /// [Self::write_waiting] right after the block that ends a caller's unit: a group the piece has no room for
    /// waits for the next call, which cuts in a chain.
    fn write_waiting_after_unit(&mut self, defs: &Definitions) -> Result<(), WriteError<S::Error>> {
        if self.caller_run || !self.relay_groups.has_waiting() {
            return Ok(());
        }
        match self.write_waiting(Some(defs)) {
            Err(WriteError::Index(IndexError::StateEntries { .. })) => Ok(()),
            r => r,
        }
    }

    /// Push the records of block `b` of puts to put onto `out`, each after what it needs ([Self::put_block]), and
    /// the keyed records among them onto `key_puts`, for [keyed::check]; `seen` holds what earlier blocks of its
    /// group pushed.
    fn block_cands<'b>(
        &mut self,
        defs: &Definitions,
        b: &crate::Block<'b>,
        seen: &mut BlockSeen,
        (out, key_puts): (&mut Vec<Candidate<'b>>, &mut Vec<keyed::KeyPut<'b>>),
    ) -> Result<(), WriteError<S::Error>> {
        let rule = self.definitions_rule();
        let keyed = b.is_keyed();
        for r in b.records() {
            let r = r.map_err(WriteError::Block)?;
            let (RecordTrack::Data(track), Some(at)) = (r.track, r.at) else {
                let d = <Describe<'_> as shrink_wrap::DeserializeShrinkWrap>::from_ww_bytes(r.item)
                    .map_err(DescribeError::Decode)?;
                if matches!(d, Describe::Producer(_) | Describe::Layout(_)) {
                    continue;
                }
                if crate::open::is_held(&d) {
                    let slot = Slot::of_definition(&d, || ItemHash::of(r.item));
                    if defs.get(&slot) != Some(r.item) {
                        return Err(DescribeError::NotHeld { slot }.into());
                    }
                    let named = match &d {
                        Describe::Track(t) => defs.needs(t.number),
                        Describe::Map(m) => vec![
                            Slot::Clock { number: m.from },
                            Slot::Clock { number: m.to },
                            slot,
                        ],
                        _ => vec![slot],
                    };
                    let mut needs = Vec::new();
                    self.push_needs(defs, named, &mut seen.slots, &mut needs);
                    out.extend(needs);
                    continue;
                }
                if let Describe::Item(item) = &d {
                    let bundle: Vec<u8> = item.ty.bundle.iter().filter_map(Result::ok).collect();
                    if let Some((slot, _)) = defs.bundle(&bundle) {
                        let mut needs = Vec::new();
                        self.push_needs(defs, [slot], &mut seen.slots, &mut needs);
                        out.extend(needs);
                    }
                }
                self.work.hash(r.item.len());
                let hash = ItemHash::of(r.item);
                out.push(Candidate {
                    slot: Slot::of_definition(&d, || hash),
                    rule,
                    track: RecordTrack::Definitions,
                    at: None,
                    hash,
                    item: Item::Given(r.item),
                });
                continue;
            };
            let facts = defs.check_on_track(track, at)?;
            let policy = facts.state.ok_or(DescribeError::NotState { track })?;
            let slot = match (facts.keyed, keyed) {
                (false, false) => Slot::State { track },
                (true, true) => Slot::of(r.track, r.item, true)
                    .ok_or(WriteError::Block(crate::BlockError::BadRecords))?,
                (true, false) => return Err(DescribeError::Keyed { track }.into()),
                (false, true) => return Err(DescribeError::NotKeyed { track }.into()),
            };
            if r.item.len() > STATE_ITEM_MAX {
                return Err(DescribeError::TooLong { len: r.item.len() }.into());
            }
            if let Slot::Keyed { .. } = slot
                && let Some((id, owner)) = defs.track_identity(track)
            {
                let item = crate::KeyedItem::read(r.item).map_err(DescribeError::KeyedItem)?;
                key_puts.push(keyed::KeyPut {
                    track,
                    ident: (id, OwnerId(owner)),
                    item,
                    len: r.item.len(),
                });
            }
            self.work.hash(r.item.len());
            let hash = ItemHash::of(r.item);
            // the same item again in a slot adds nothing; a group's slot takes one item (put_block), a keyed slot one
            // per key: another key of its id is for keyed::check
            let before = seen.newest.insert(slot, hash);
            let same_key = match slot {
                Slot::Keyed { .. } => {
                    let key = crate::KeyedItem::read(r.item)
                        .map_err(DescribeError::KeyedItem)?
                        .key;
                    seen.keys
                        .insert(slot, key.to_vec())
                        .is_none_or(|k| k == key)
                }
                _ => true,
            };
            if before.is_some() && same_key && seen.twice.is_none() {
                seen.twice = Some(track);
            }
            if before == Some(hash) {
                continue;
            }
            let mut needs = Vec::new();
            self.push_needs(defs, defs.needs(track), &mut seen.slots, &mut needs);
            out.extend(needs);
            out.push(Candidate {
                slot,
                rule: self.rule(policy),
                track: r.track,
                at: Some(at),
                hash,
                item: Item::Given(r.item),
            });
        }
        Ok(())
    }

    /// Pass on a live producer's block, as a relay does: a block of puts ([FLAG_REPEAT](crate::FLAG_REPEAT)) has its
    /// definitions held in `defs` ([Definitions::take_block]) and its records put ([Self::put_block]), a group's
    /// held until its last block; any other block is written under `defs` ([Self::write_block]), and tears the group
    /// held of its producer ([GroupRefusal::Torn]). A series chunk's grid start mark
    /// ([FLAG_GRID_START](crate::FLAG_GRID_START)) follows this tape's chunks, not the producer's: where it isn't due
    /// here, as after a chunk lost on the way, the chunk is restamped ([crate::mark_grid_start]): its header, head CRC
    /// and block CRC, nothing else.
    pub fn relay_block(
        &mut self,
        defs: &mut Definitions,
        block: &[u8],
    ) -> Result<(), WriteError<S::Error>> {
        let b = crate::Block::read(block).map_err(WriteError::Block)?;
        if !b.is_repeat() {
            self.check()?;
            let torn = self.relay_groups.take(b.session);
            if let Err(e) = self.write_read(defs, &b, true) {
                if let Some(g) = torn {
                    self.relay_groups.hold(b.session, g);
                }
                return Err(e);
            }
            return torn_error(b.session, torn);
        }
        let mut buf = Vec::new();
        defs.take_block(&b.decode(&mut buf).map_err(WriteError::Block)?)?;
        self.put_block(defs, block)
    }

    /// Sync now if blocks wait and the interval of [SyncPolicy::Interval] is up; call it when no blocks come.
    pub fn tick(&mut self) -> Result<(), WriteError<S::Error>> {
        self.check()?;
        let by_time = self.indexer.policy().interval.is_some_and(|t| {
            self.indexer.pending() > 0 && self.clock.passed(self.last_checkpoint, t)
        });
        if by_time && self.alignment() == 0 && !self.unit_open {
            self.place_checkpoint()?;
        } else if by_time {
            self.index_due = true;
        }
        match self.policy {
            SyncPolicy::Interval(t)
                if self.unsynced > 0 && self.clock.passed(self.last_sync, t) =>
            {
                self.sync()
            }
            _ => Ok(()),
        }
    }

    /// Sync the data written so far to the disk.
    pub fn sync(&mut self) -> Result<(), WriteError<S::Error>> {
        self.check()?;
        if let Err(error) = self.sink.sync() {
            // never retried, see the module docs
            self.failed = true;
            return Err(WriteError::Sink {
                what: "syncing the blocks written",
                error,
            });
        }
        self.unsynced = 0;
        self.synced = self.bytes;
        self.last_sync = self.clock.now_ms();
        Ok(())
    }

    /// Close the piece ([CloseReason::Closed]) and sync it.
    pub fn close(self) -> Result<Closed, WriteError<S::Error>> {
        self.close_as(CloseReason::Closed)
    }

    /// Close the piece for `reason`: the definitions made, the deferred blocks, the final index, the close record and
    /// the tail; then sync as [Self::close].
    pub fn close_as(self, reason: CloseReason) -> Result<Closed, WriteError<S::Error>> {
        self.end(End::Close(reason), None).map(|e| e.closed)
    }

    /// Name `pieces` of the chain as removed on purpose in the close of this piece and in every later close of it, with
    /// those named before ([crate::piece::RemovedPiece]). A reader's [check_chain](crate::piece::check_chain) then
    /// links over them. Nothing is written until the close; a closed piece resumed and closed again with only this
    /// writes an append, a final index, a close and a tail. A piece already named with the same id and hash is taken
    /// once.
    ///
    /// Refused ([InputError::Removed]), with nothing taken: a piece without a head, a piece not before this one, a
    /// piece named before with another id or hash, or more than
    /// [REMOVED_PIECES_MAX](crate::piece::REMOVED_PIECES_MAX) in all.
    pub fn record_removed(
        &mut self,
        pieces: &[crate::piece::RemovedPiece],
    ) -> Result<(), WriteError<S::Error>> {
        let refuse = |why| Err(WriteError::Input(InputError::Removed(why)));
        let Some((head, _)) = &self.head else {
            return refuse(RemovedRefusal::NoChain);
        };
        let own = head.ident.seq;
        let mut all = self.removed.clone();
        for p in pieces {
            if p.seq >= own {
                return refuse(RemovedRefusal::NotBefore { seq: p.seq });
            }
            match all.binary_search_by_key(&p.seq, |r| r.seq) {
                Ok(i) if all.get(i) == Some(p) => {}
                Ok(_) => return refuse(RemovedRefusal::Conflict { seq: p.seq }),
                Err(i) => all.insert(i, *p),
            }
        }
        let max = crate::piece::REMOVED_PIECES_MAX;
        if all.len() > max {
            return refuse(RemovedRefusal::TooMany { max });
        }
        if all.len() > self.removed.len() {
            self.removed = all;
            self.removed_added = true;
        }
        Ok(())
    }

    /// The pieces of the chain the next close names as removed, ascending by sequence number: those the close in force
    /// names, and those [Self::record_removed] added.
    pub fn removed(&self) -> &[crate::piece::RemovedPiece] {
        &self.removed
    }

    /// Close the piece now ([CloseReason::Closed]), synced, and go on writing after that close: the writer a resume
    /// of the closed piece gives ([Self::resume_on]), so the calls after it write what they would after a close and a
    /// resume. The newest state items and relation values of the pieces before stay, for the next cut. A crash after
    /// the call finds the piece closed by this close, with the pieces [Self::record_removed] named in it. Nothing is
    /// written when nothing came since the close in force. Gives the close in force.
    ///
    /// After a failure the writer is gone: the piece is closed, or reads as a crash left it.
    pub fn close_and_go_on(self) -> Result<(Self, Closed), WriteError<S::Error>> {
        self.check()?;
        if !self.removed_added
            && self.deferred.is_empty()
            && let Some(c) = self.closed_at.filter(|_| self.append_head.is_some())
        {
            let ranges = self.indexer.track_ranges();
            let len = self
                .bytes
                .checked_sub(wide(crate::piece::APPEND_HEAD_LEN))
                .ok_or(WriteError::Index(IndexError::Invalid(
                    "an append head past the piece's bytes",
                )))?;
            let closed = Closed {
                hash: c.hash,
                len,
                ident: self.head.as_ref().map(|(h, _)| h.ident),
                next: None,
                start: self.sink.base().saturating_add(self.start),
                tracks: self.next_tracks(&ranges),
                states_unknown: 0,
                keys_unknown: 0,
            };
            return Ok((self, closed));
        }
        let options = self.options();
        let kept = self.kept.clone();
        let newest = self.relations.newest().clone();
        let grids = self.grids.clone();
        let relay_groups = self.relay_groups.clone();
        // the carry stays what it was: a resume after a close finds none
        let (carried, carried_for) = (self.carried, self.carried_for.clone());
        let ended = self.end(End::Kept, None)?;
        let (mut w, _) = Self::resume_on(ended.sink, options)?;
        w.carried = carried;
        w.carried_for = carried_for;
        w.keep_after(kept);
        w.take_newest(&newest);
        w.grids = grids;
        w.relay_groups = relay_groups;
        Ok((w, ended.closed))
    }

    /// The piece's head, written or read back by a resume; None before one.
    pub(crate) fn piece_head(&self) -> Option<&PieceHead> {
        self.head.as_ref().map(|(h, _)| h)
    }

    /// [Self::close_as] with a summary of the caller's own: `body`, at most
    /// [CLOSE_SUMMARY_MAX](crate::piece::CLOSE_SUMMARY_MAX) bytes, goes into a summary block right before the final
    /// index, which the close names ([Close::summary]) and its content hash covers. A reader of the tail reads it back
    /// with [TapeReader::close_summary](crate::TapeReader::close_summary). A longer body is refused
    /// ([InputError::SummaryTooLong]) before anything is written.
    pub fn close_with_summary(
        self,
        reason: CloseReason,
        body: &[u8],
    ) -> Result<Closed, WriteError<S::Error>> {
        self.end(End::Close(reason), Some(body)).map(|e| e.closed)
    }

    /// End this piece as a cut (a [Close] naming the next piece) and start the next piece of the chain on the tape `tapes` makes
    /// under `next`, see [Self::cut_on].
    pub fn cut_with<T: PieceStore<Sink = S, Error = S::Error>>(
        self,
        defs: &Definitions,
        tapes: &mut T,
        next: &T::Key,
    ) -> Result<(Closed, Self), CutError<S>> {
        self.cut_into(defs, || {
            tapes
                .create(next)
                .map_err(sink_err("creating the next piece"))
        })
    }

    /// End this piece as a cut (a [Close] naming the next piece) and start the next piece of the chain on `next`: its id
    /// [PieceId::after] this one's id and final content hash (the close's), its head naming this piece, the track
    /// starts moved on by this piece's chunks, then the layout and the producer put, then the definitions of every
    /// track the head carries that `defs` holds ([Definitions::needs]), in blocks of at most [CARRIED_BLOCK_LEN] that
    /// each hold their tracks' clocks and maps: a piece holds the definitions of its quiet tracks too, so it reads
    /// alone when the pieces before it are gone. A state track carries its newest item beside its definition ([NearPolicy::kept_bytes_max]),
    /// so the state in force reads there too. A definition is put again before the next data that needs it, unless
    /// it lies close behind. Seqs go on from this piece; the options carry over.
    ///
    /// The next piece carries every definition while the chain's tracks fit the index policy's state entries
    /// ([IndexPolicy::state_entries_max]), less the layout, the producer and the records of the call the cut is made
    /// for. A cut is never refused for them. Past that room each goes once, no block repeating its chain, in this
    /// order while room lasts: the tracks of the next data ([Self::want], those of the call a full piece refused),
    /// then the clocks and maps, then the tracks with the newest data. The tracks with the oldest data are so left
    /// out; the next head's rows and [Closed::tracks] mark each such track ([TrackStart::not_carried],
    /// [Closed::not_carried]), and a reader names them. A call is admitted only when its definitions fit an empty
    /// piece ([InputError::CarriedEntries]), so an admitted call always fits the piece a cut made for it.
    pub fn cut_on(self, defs: &Definitions, next: S) -> Result<(Closed, Self), CutError<S>> {
        self.cut_into(defs, || Ok(next))
    }

    /// [Self::cut_on] with the next sink made once this piece is closed.
    fn cut_into(
        self,
        defs: &Definitions,
        next: impl FnOnce() -> Result<S, WriteError<S::Error>>,
    ) -> Result<(Closed, Self), CutError<S>> {
        let (mut w, rows) = self.next_rows(defs)?;
        let options = w.options();
        let newest = w.relations.newest().clone();
        let grids = core::mem::take(&mut w.grids);
        let kept = core::mem::take(&mut w.kept);
        let unlisted = w.unlisted.is_some();
        let relay_groups = core::mem::take(&mut w.relay_groups);
        let wanted = core::mem::take(&mut w.wanted);
        let done = w.end(End::Cut, None)?;
        let mut next_piece = NextPiece::after(
            &rows.head,
            &done.closed,
            (rows.tracks, unlisted),
            done.session,
            done.next_seq,
        )?;
        next_piece.newest = newest;
        next_piece.grids = grids.next_piece();
        next_piece.kept = kept;
        let mut next = Self::start_next(next()?, &options, next_piece)?;
        next.wanted = wanted;
        next.put_carried(&rows.carried)?;
        next.wanted.clear();
        next.relay_groups = relay_groups;
        let mut closed = done.closed;
        closed.tracks = next.head_rows().to_vec();
        closed.states_unknown = rows.states_unknown;
        closed.keys_unknown = rows.keys_unknown;
        Ok((closed, next))
    }

    /// What a cut starts the next piece with ([NextRows]), or the writer back in a [CutError] when the cut is
    /// refused: this piece has no head. The carried definitions take what the index policy's state entries leave
    /// past the layout and the producer ([carried::fit]); the next head's rows mark the tracks left out.
    fn next_rows(mut self, defs: &Definitions) -> Result<(Self, NextRows), CutError<S>> {
        // the next piece starts from every key known
        self.load_keys(defs, true);
        let Some((head, starts)) = self.head.as_ref() else {
            return Err(self.refuse(WriteError::Index(IndexError::Invalid(
                "a cut needs a piece head",
            ))));
        };
        let head = *head;
        let mut tracks = self.indexer.next_starts(starts);
        let room = self.indexer.policy().state_entries_max.get();
        let room = crate::cap_usize(room).saturating_sub(HEAD_ENTRIES);
        let (carried, dropped) = self.carried(defs, &tracks, room);
        let dropped: BTreeSet<TrackNum> = dropped.into_iter().collect();
        for t in &mut tracks {
            t.not_carried = dropped.contains(&t.track_number);
        }
        for (n, ident) in Self::keyed_rows(defs, &tracks) {
            let complete = self.keys_complete(n, ident);
            for t in tracks.iter_mut().filter(|t| t.track_number == n) {
                t.keys_carried = complete && !t.not_carried;
            }
        }
        let keyed: BTreeSet<TrackNum> = Self::keyed_rows(defs, &tracks)
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        let keys_unknown = tracks
            .iter()
            .filter(|t| keyed.contains(&t.track_number) && !t.keys_carried)
            .count();
        let states = self.carried_states(defs, &tracks);
        let states_unknown = tracks
            .iter()
            .filter(|t| !t.not_carried && !states.contains_key(&t.track_number))
            .filter(|t| defs.track_identity(t.track_number) == Some((t.track_id, t.owner.0)))
            .filter(|t| {
                defs.track(t.track_number)
                    .is_some_and(|f| f.state.is_some())
            })
            .count();
        Ok((
            self,
            NextRows {
                head,
                tracks,
                carried,
                states_unknown,
                keys_unknown,
            },
        ))
    }

    /// A cut refused for `error` before anything was written: the writer comes back with it.
    fn refuse(self, error: WriteError<S::Error>) -> CutError<S> {
        CutError {
            error,
            writer: Some(Box::new(self)),
        }
    }

    /// End this piece as a cut (a [Close] naming the next piece) and start the next piece of the chain right behind it in the same
    /// sink, as [Self::cut_on] does on a new one: the sink's window ([TapeSink::rebase]) moves to the end of this
    /// piece, a multiple of [PIECE_ALIGN] when this one started on one. The sink stays open: it isn't closed, a
    /// file stays as it is. A sink of one piece ([TapeSink::holds_pieces] false) is refused before anything is
    /// written: a file of several pieces is made by [PieceStore::create_whole].
    ///
    /// A refusal before the cut, this one or one of [Self::cut_on]'s, hands the writer back in [CutError::writer], to
    /// go on with.
    pub fn cut_in_place(self, defs: &Definitions) -> Result<(Closed, Self), CutError<S>> {
        if !self.sink.holds_pieces() {
            return Err(self.refuse(WriteError::Index(IndexError::Invalid(
                "a sink of one piece takes no cut in place",
            ))));
        }
        let (mut w, rows) = self.next_rows(defs)?;
        let options = w.options();
        let newest = w.relations.newest().clone();
        let grids = core::mem::take(&mut w.grids);
        let kept = core::mem::take(&mut w.kept);
        let unlisted = w.unlisted.is_some();
        let relay_groups = core::mem::take(&mut w.relay_groups);
        let wanted = core::mem::take(&mut w.wanted);
        let mut done = w.end(End::CutInPlace, None)?;
        let mut next_piece = NextPiece::after(
            &rows.head,
            &done.closed,
            (rows.tracks, unlisted),
            done.session,
            done.next_seq,
        )?;
        next_piece.newest = newest;
        next_piece.grids = grids.next_piece();
        next_piece.kept = kept;
        let base = done
            .closed
            .start
            .checked_add(done.closed.len)
            .ok_or(WriteError::Index(IndexError::Invalid(
                "a file past u64::MAX bytes",
            )))?;
        done.sink
            .rebase(base)
            .map_err(sink_err("starting the next piece behind the last"))?;
        let mut next = Self::start_next(done.sink, &options, next_piece)?;
        next.wanted = wanted;
        next.put_carried(&rows.carried)?;
        next.wanted.clear();
        next.relay_groups = relay_groups;
        done.closed.tracks = next.head_rows().to_vec();
        done.closed.states_unknown = rows.states_unknown;
        done.closed.keys_unknown = rows.keys_unknown;
        Ok((done.closed, next))
    }

    /// The options the writer runs with: what [Self::resume_on] is to be given for this piece.
    pub fn options(&self) -> WriterOptions {
        WriterOptions {
            sync: self.policy,
            index: self.indexer.policy(),
            layout: self.layout,
            producer: self.producer.clone(),
            near: self.near_policy,
            clock: self.clock.clone(),
            integrity: self.level_next,
        }
    }

    /// A writer on `sink` starting the piece `next` of a chain: its head, then the layout and the producer put.
    /// Seqs go on from that piece.
    pub(crate) fn start_next(
        sink: S,
        options: &WriterOptions,
        next: NextPiece,
    ) -> Result<Self, WriteError<S::Error>> {
        let mut w = Self::create_on(sink, options.sync);
        w.apply(options)?;
        w.begin_next(next)?;
        Ok(w)
    }

    /// Start the empty sink as the piece `next` of a chain, see [Self::start_next].
    pub(crate) fn begin_next(&mut self, next: NextPiece) -> Result<(), WriteError<S::Error>> {
        self.next_seq = next.next_seq;
        self.session = next.session;
        self.relations.take_newest(&next.newest);
        self.grids = next.grids;
        self.kept = kept::Kept::default();
        let head = PieceHead {
            integrity: self.level_next,
            ..next.head
        };
        self.write_piece_head(&head, &next.tracks)?;
        self.keep_after(next.kept);
        Ok(())
    }

    /// Go on after `older`, the newest state items of the pieces before ([kept]): those of the tracks the piece head
    /// carries, older than those this piece holds.
    pub(crate) fn keep_after(&mut self, mut older: kept::Kept) {
        let rows: BTreeMap<TrackNum, TrackIdent> = self
            .head_rows()
            .iter()
            .map(|t| (t.track_number, (t.track_id, t.owner)))
            .collect();
        older.retain(&rows);
        if let Some((_, _, inherited)) = &mut self.keys_base {
            // older than the keys of the closed piece, which are read later: kept apart until then
            inherited.keys_after(older.take_keys());
        }
        self.kept.after(older, self.kept_bytes_max());
    }

    /// Go on from `older`, the grids of the series tracks' newest chunks in the pieces before: a resume's.
    pub(crate) fn take_grids(&mut self, older: &grids::Grids) {
        self.grids.after(older);
    }

    /// Go on from `newest`, the newest values of the monotone relations of the pieces before: a resume's.
    pub(crate) fn take_newest(&mut self, newest: &relations::Newest) {
        self.relations.take_newest(newest);
    }

    /// End the piece as `how` says; a cut piece with a head names the next piece's id, [PieceId::after] its own
    /// and the content hash in its close. Unless cut in place the sink is closed ([TapeSink::close]), else it is
    /// synced and handed back for the next piece.
    ///
    /// The sink is synced twice, whatever the [SyncPolicy]: after the final index, before the close record, and after
    /// the tail. A disk may write pages out of order; the first sync keeps a close record and tail that reached it from
    /// naming blocks that didn't.
    fn end(mut self, how: End, summary: Option<&[u8]>) -> Result<Ended<S>, WriteError<S::Error>> {
        if matches!(how, End::Close(_)) && self.relay_groups.has_waiting() {
            self.close_waiting()?;
        }
        let summary = summary
            .map(|body| {
                crate::piece::write_close_summary(body, BlockId::new(self.session, 0)).ok_or(
                    WriteError::Input(InputError::SummaryTooLong {
                        len: body.len(),
                        max: crate::piece::CLOSE_SUMMARY_MAX,
                    }),
                )
            })
            .transpose()?;
        if matches!(how, End::Close(_))
            && summary.is_none()
            && !self.removed_added
            && self.deferred.is_empty()
            && let Some(c) = self.closed_at.filter(|_| self.append_head.is_some())
        {
            return self.unchanged(c.hash);
        }
        self.flush_deferred()?;
        self.ensure_head()?;
        let (closed, ..) = self.write_end(how, summary, None)?;
        if !matches!(how, End::CutInPlace | End::Kept) {
            self.sink
                .close()
                .map_err(sink_err("marking the piece closed"))?;
        }
        Ok(Ended {
            closed,
            next_seq: self.next_seq,
            session: self.session,
            sink: self.sink,
        })
    }

    /// Write the end of the piece as `how` says: the caller's `summary` block, the final index, the close record and
    /// the tail, synced after the final index and after the tail ([Self::end]); gives the piece closed, where its close
    /// record starts and what it says. With `fits` (where the end starts and its length), an end of another length is
    /// refused before any byte of it is written ([SealError::Moved]): a seal writes it over the end it replaces.
    fn write_end(
        &mut self,
        how: End,
        summary: Option<Vec<u8>>,
        fits: Option<(u64, u64)>,
    ) -> Result<(Closed, u64, Close), WriteError<S::Error>> {
        let summary = match summary {
            Some(mut block) => {
                crate::set_id(&mut block, self.structural_id()).map_err(WriteError::Block)?;
                Some(block)
            }
            None => None,
        };
        let mut prev = self.indexer.final_before();
        let mut parts = self.indexer.final_parts();
        // a close after an append follows a final index block of its own, an empty one when no block came since
        if parts.is_empty() && self.closed_at.is_some() {
            parts.push(Vec::new());
        }
        // the final index blocks, each pointing back at the one before, placed after the summary
        let summary_len = summary.as_ref().map_or(0, Vec::len);
        let mut at = self.end_after(summary_len)?;
        let mut finals = Vec::with_capacity(parts.len());
        for part in parts {
            let b = Indexer::final_block(&part, at, prev, self.structural_id())
                .map_err(WriteError::Index)?;
            prev = Some(at);
            at = at.saturating_add(wide(b.len()));
            finals.push(b);
        }
        let ranges = self.indexer.track_ranges();
        let removed = self.removed.len();
        let padding = close_padding(at, ranges.len(), removed);
        let close_len = close_len(ranges.len(), removed).saturating_add(padding);
        if let Some((from, len)) = fits {
            let end = at
                .saturating_add(wide(close_len))
                .saturating_add(wide(TAIL_LEN));
            if self.offset() != from || end.checked_sub(from) != Some(len) {
                return Err(WriteError::Seal(SealError::Moved { offset: from }));
            }
        }
        // the caller's summary, right before the final index, with the id of the blocks the file structure adds
        let summary = match summary {
            Some(block) => {
                let at = self.offset();
                let len = u32::try_from(block.len()).map_err(|_overflow| {
                    WriteError::Index(IndexError::Invalid("a summary block past u32::MAX bytes"))
                })?;
                self.write_own(&block)?;
                Some((at, len))
            }
            None => None,
        };
        for b in &finals {
            self.write_own(b)?;
        }
        // the blocks and the final index reach the disk before a close record names them: a close and tail found
        // whole after a crash then never stand for bytes a disk never got
        self.sync()?;
        // below Integrity::Full the content hash is absent: zero, read as absent by the head's level
        let hash = match self.level {
            Integrity::Full => ContentHash(*self.hash.clone().finalize().as_bytes()),
            Integrity::CrcOnly | Integrity::None => ContentHash::default(),
        };
        let ident = self.head.as_ref().map(|(h, _)| h.ident);
        let (reason, continued) = match how {
            End::Close(reason) => (reason, false),
            End::Kept => (CloseReason::Closed, false),
            End::Cut | End::CutInPlace => (CloseReason::Closed, true),
        };
        let next = ident
            .filter(|_| continued)
            .map(|i| PieceId::after(i.id, &hash));
        let close = Close {
            reason,
            hash,
            checkpoint: self.indexer.last_checkpoint(),
            final_index: prev,
            prev: self.closed_at.map(|c| c.offset),
            summary,
            ident,
            next,
            unlisted_keys_unknown: self.unlisted.is_some(),
        };
        let tracks = self.next_tracks(&ranges);
        let close_offset = self.offset();
        // the piece ends, after its tail, on a multiple of PIECE_ALIGN
        let mut buf = vec![0u8; close_len];
        let n = write_close(
            &mut buf,
            self.structural_id(),
            &close,
            (&ranges, &self.removed),
            padding,
        )
        .map_err(WriteError::Index)?;
        buf.truncate(n);
        self.write_own(&buf)?;
        let too_long = WriteError::Index(IndexError::Invalid("a piece past u64::MAX bytes"));
        let piece_len = self.offset().checked_add(wide(TAIL_LEN)).ok_or(too_long)?;
        // close_padding sizes the close record so the tail ends the piece on PIECE_ALIGN; a piece that doesn't is
        // refused before its tail is written, so it never reads as closed
        if !piece_len.is_multiple_of(u64::from(PIECE_ALIGN)) {
            return Err(WriteError::Index(IndexError::Invalid(
                "the close padding doesn't end the piece on PIECE_ALIGN bytes",
            )));
        }
        let tail = Tail {
            piece_len,
            close_offset,
            close_len: u32::try_from(n)
                .ok()
                .ok_or(WriteError::Index(IndexError::Invalid(
                    "close longer than a block",
                )))?,
            reason,
            hash,
            ident,
            next,
        };
        let mut tb = [0u8; TAIL_LEN];
        tail.write(&mut tb, self.structural_id());
        self.write_own(&tb)?;
        self.sync()?;
        let closed = Closed {
            hash,
            len: self.bytes,
            ident,
            next,
            start: self.sink.base().saturating_add(self.start),
            tracks,
            states_unknown: 0,
            keys_unknown: 0,
        };
        Ok((closed, close_offset, close))
    }

    /// Where each track goes on in the next piece, from the ranges `ranges` of this one.
    fn next_tracks(&self, ranges: &[crate::piece::TrackRange]) -> Vec<crate::piece::TrackStart> {
        crate::index::next_starts(
            self.head
                .as_ref()
                .map(|(_, s)| s.as_slice())
                .unwrap_or_default(),
            ranges,
        )
    }

    /// The end of a resumed closed piece to which nothing was appended: no byte is written, the close in force with
    /// content hash `hash` stays the last, and the append head is dropped unwritten. A closed piece a crash left
    /// that a crash left unmarked is marked closed ([TapeSink::close]).
    fn unchanged(mut self, hash: ContentHash) -> Result<Ended<S>, WriteError<S::Error>> {
        self.append_head = None;
        self.sink
            .close()
            .map_err(sink_err("marking the piece closed"))?;
        let len = self
            .bytes
            .checked_sub(wide(crate::piece::APPEND_HEAD_LEN))
            .ok_or(WriteError::Index(IndexError::Invalid(
                "an append head past the piece's bytes",
            )))?;
        let ranges = self.indexer.track_ranges();
        let tracks = self.next_tracks(&ranges);
        let start = self.sink.base().saturating_add(self.start);
        Ok(Ended {
            closed: Closed {
                hash,
                len,
                ident: self.head.as_ref().map(|(h, _)| h.ident),
                next: None,
                start,
                tracks,
                states_unknown: 0,
                keys_unknown: 0,
            },
            next_seq: self.next_seq,
            session: self.session,
            sink: self.sink,
        })
    }

    /// The piece was closed by the close record `close` at `at`, its tail at `tail`: what follows chains its content
    /// hash over that close's ([crate::piece]), its final index goes on from that close's, and the pieces `removed` it
    /// names go on in the next close. A resume calls it before it replays the close record, each close it keeps in turn.
    pub(crate) fn closed_by(
        &mut self,
        at: u64,
        tail: u64,
        close: &Close,
        removed: Vec<crate::piece::RemovedPiece>,
    ) {
        self.removed = removed;
        self.removed_added = false;
        self.closed_at = Some(ClosedAt {
            offset: at,
            tail,
            hash: close.hash,
        });
        self.hash = crate::piece::chained(&close.hash);
        self.indexer.closed(at, close.final_index, close.checkpoint);
    }

    /// Write on after the piece's last close record and tail, which end the bytes: the append head naming them
    /// ([crate::piece::AppendHead]), accounted for here and written before the first block that follows. A resume of
    /// a closed piece calls it once it replayed the piece.
    pub(crate) fn write_on(&mut self) -> Result<(), WriteError<S::Error>> {
        let Some(c) = self.closed_at else {
            return Err(WriteError::Index(IndexError::Invalid(
                "writing on after a close needs one",
            )));
        };
        let head = crate::piece::AppendHead {
            close_offset: c.offset,
            tail_offset: c.tail,
            hash: c.hash,
        };
        let mut buf = [0u8; crate::piece::APPEND_HEAD_LEN];
        head.write(&mut buf, self.structural_id());
        let (Some(header), Some(crc)) = (
            buf.first_chunk::<{ crate::HEADER_LEN }>(),
            buf.last_chunk::<{ crate::CRC_LEN }>(),
        ) else {
            return Err(WriteError::Block(crate::BlockError::Short {
                needed: crate::HEADER_LEN,
            }));
        };
        self.account(&buf, header, *crc)?;
        self.append_head = Some(buf);
        Ok(())
    }

    /// The index entries a checkpoint would list now, for tests and tools.
    pub fn pending_entries(&self) -> usize {
        self.indexer.pending()
    }

    fn check(&self) -> Result<(), WriteError<S::Error>> {
        if self.failed {
            Err(WriteError::Failed)
        } else {
            Ok(())
        }
    }
}

/// The bytes of a candidate's item: a held definition's, shared, the caller's, or made for the put.
#[derive(Clone, Debug)]
enum Item<'a> {
    Held(alloc::sync::Arc<[u8]>),
    Given(&'a [u8]),
    Owned(Vec<u8>),
}

impl Item<'_> {
    fn bytes(&self) -> &[u8] {
        match self {
            Item::Held(b) => b,
            Item::Given(b) => b,
            Item::Owned(b) => b,
        }
    }
}

/// Which blocks of a call's puts a record goes into, in this order.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Part {
    /// The first record of a definition written twice at once: blocks marked [FLAG_TWICE](crate::FLAG_TWICE)
    Twice,
    /// A bundle, a schema or another definition filling a slot by its hash: blocks of their own, before the tracks
    /// that name them, so a reader of the tracks and clocks never reads their bytes
    Own,
    /// Any other record
    Rest,
}

/// A record a put may write: its slot, the rule of its track, the record and its item's hash.
#[derive(Clone, Debug)]
struct Candidate<'a> {
    slot: Slot,
    rule: Rule,
    track: RecordTrack,
    at: Option<ClockTime>,
    item: Item<'a>,
    hash: ItemHash,
}

impl Candidate<'static> {
    /// The definition `held` in `slot`, on track 0 of `rule`: shared, its hash the one kept with it.
    fn held(slot: Slot, rule: Rule, held: &crate::open::Held) -> Self {
        Candidate {
            slot,
            rule,
            track: RecordTrack::Definitions,
            at: None,
            item: Item::Held(held.item.clone()),
            hash: held.hash,
        }
    }
}

/// A record to put, and whether it is a keyed state record ([Slot::Keyed]).
type PutRef<'c> = (RecordRef<'c>, bool);

impl Candidate<'_> {
    /// The candidate with its item's bytes its own.
    fn owned(self) -> Candidate<'static> {
        Candidate {
            slot: self.slot,
            rule: self.rule,
            track: self.track,
            at: self.at,
            item: match self.item {
                Item::Held(b) => Item::Held(b),
                Item::Given(b) => Item::Owned(b.to_vec()),
                Item::Owned(b) => Item::Owned(b),
            },
            hash: self.hash,
        }
    }
}

/// What the blocks of puts a writer put so far of one producer's block or group pushed ([TapeWriter::put_block]).
#[derive(Clone, Debug, Default)]
struct BlockSeen {
    /// The definitions pushed
    slots: BTreeSet<Slot>,
    /// The hash of the newest record of each state slot: of each state track, of each key of a keyed one
    newest: alloc::collections::BTreeMap<Slot, ItemHash>,
    /// The first state track with a second record, of the same key on a keyed one
    twice: Option<TrackNum>,
    /// The key of the newest record of each keyed slot
    keys: alloc::collections::BTreeMap<Slot, Vec<u8>>,
}

/// A producer's group of puts a writer holds until its last block ([TapeWriter::put_block]).
#[derive(Clone, Debug, Default)]
struct RelayGroup {
    /// The records of its blocks so far, to put
    cands: Vec<Candidate<'static>>,
    /// Its blocks so far, at most [RUN_BLOCKS_MAX](crate::RUN_BLOCKS_MAX)
    blocks: usize,
    /// The bytes of the records' items so far, at most [GROUP_BYTES_MAX]
    bytes: usize,
    /// What its records cost held: each one's bytes and [RELAY_RECORD_OVERHEAD]
    held: usize,
    seen: BlockSeen,
    /// The records, bytes, cost and what was seen before its last block: [Self::undo_last]
    before: (usize, usize, usize, BlockSeen),
}

impl RelayGroup {
    /// Take its last block out again, as before [TapeWriter::put_block] added it.
    fn undo_last(&mut self) {
        let (len, bytes, held, seen) = core::mem::take(&mut self.before);
        self.cands.truncate(len);
        self.blocks = self.blocks.saturating_sub(1);
        self.bytes = bytes;
        self.held = held;
        self.seen = seen;
    }
}

/// A refusal of a group of puts.
fn group_refused<E>(r: GroupRefusal) -> WriteError<E> {
    WriteError::Input(InputError::Group(r))
}

/// The error naming the group `torn` of the producer of `session` dropped, if any ([GroupRefusal::Torn]).
fn torn_error<E>(
    session: crate::SessionTag,
    torn: Option<RelayGroup>,
) -> Result<(), WriteError<E>> {
    match torn {
        Some(g) => Err(group_refused(GroupRefusal::Torn {
            session,
            blocks: g.blocks,
        })),
        None => Ok(()),
    }
}

/// The records of `cands` that `chosen` marks, in order.
fn chosen_records<'c>(cands: &'c [Candidate<'_>], chosen: &[bool]) -> Vec<PutRef<'c>> {
    cands
        .iter()
        .zip(chosen)
        .filter(|(_, c)| **c)
        .map(|(c, _)| put_ref(c))
        .collect()
}

/// The record `c` puts, and whether it is keyed.
fn put_ref<'c>(c: &'c Candidate<'_>) -> PutRef<'c> {
    let r = RecordRef {
        track: c.track,
        at: c.at,
        item: c.item.bytes(),
    };
    (r, matches!(c.slot, Slot::Keyed { .. }))
}

/// Make the blocks of puts holding `records`, each of id `id`, into `out`, in order: blocks marked
/// [FLAG_REPEAT](crate::FLAG_REPEAT) and `flags`, a run of keyed state records in blocks of their own of kind
/// [KeyedPuts](BlockKind::KeyedPuts).
fn put_blocks<E: From<DescribeError>>(
    records: &[PutRef<'_>],
    id: BlockId,
    flags: u16,
    mut out: impl FnMut(&[u8]) -> Result<(), E>,
) -> Result<(), E> {
    for run in records.chunk_by(|a, b| a.1 == b.1) {
        let keyed = run.first().is_some_and(|(_, k)| *k);
        let kind = if keyed {
            BlockKind::KeyedPuts
        } else {
            BlockKind::Records
        };
        let refs: Vec<RecordRef<'_>> = run.iter().map(|(r, _)| *r).collect();
        let flags = crate::FLAG_REPEAT | flags;
        write_blocks_of(kind, &refs, DEFINITION_BLOCK_LEN, flags, || id, &mut out)?;
    }
    Ok(())
}

/// A block waiting for the gap before an aligned chunk payload, its definitions put when it came, and where the
/// piece was then.
struct Deferred {
    block: Vec<u8>,
    at: u64,
    /// The data tracks it holds: placed before any later block of them
    tracks: Vec<TrackNum>,
    /// Those of them without a row in the close when it was deferred: counted as with data ([TapeWriter::track_room])
    keys: BTreeSet<TrackIdent>,
}

/// The state entries `cands` take at most: two of each written twice at once ([Rule::twice]).
fn entries_max(cands: &[Candidate<'_>]) -> usize {
    cands
        .iter()
        .map(|c| if c.rule.twice { 2 } else { 1 })
        .fold(0usize, usize::saturating_add)
}

/// Blocks of puts holding `records`, as [TapeWriter::write_puts] writes them.
fn puts_blocks<E>(records: &[PutRef<'_>]) -> Result<usize, WriteError<E>> {
    let mut n = 0usize;
    put_blocks(records, BlockId::new(SessionTag::NONE, 0), 0, |_| {
        n = n.saturating_add(1);
        Ok::<(), DescribeError>(())
    })?;
    Ok(n)
}

/// Bytes of the blocks of puts holding `records`, as [TapeWriter::write_puts] writes them.
fn puts_len<E>(records: &[PutRef<'_>]) -> Result<usize, WriteError<E>> {
    let mut len = 0usize;
    put_blocks(records, BlockId::new(SessionTag::NONE, 0), 0, |b| {
        len = len.saturating_add(b.len());
        Ok::<(), DescribeError>(())
    })?;
    Ok(len)
}

/// At least the bytes of the blocks of puts holding all of `cands`, without making them: each record in a block of
/// its own at most, and a definition written twice at once ([Rule::twice]) with its copy.
fn puts_len_upper(cands: &[Candidate<'_>]) -> usize {
    cands.iter().fold(0usize, |n, c| {
        let one = c
            .item
            .bytes()
            .len()
            .saturating_add(crate::MAX_RECORD_HEAD)
            .saturating_add(crate::open::EMPTY_LEN);
        // a definition written twice at once has its copy after the run
        n.saturating_add(one.saturating_mul(if c.rule.twice { 2 } else { 1 }))
    })
}

/// What the next piece of a chain starts with, from the piece before.
pub(crate) struct NextPiece {
    head: PieceHead,
    tracks: Vec<TrackStart>,
    session: SessionTag,
    next_seq: u64,
    /// The newest values of the monotone relations so far
    newest: relations::Newest,
    /// The grids of the newest series chunks so far
    grids: grids::Grids,
    /// The newest item of each state track so far ([kept])
    pub(crate) kept: kept::Kept,
}

impl NextPiece {
    /// The piece after the one headed `head` that ended as `done`, continued: its id [PieceId::after] that one's id
    /// and content hash, its head naming that piece, its track starts `tracks`, and whether keyed tracks without a
    /// row may have keys before it (`unlisted`, [PieceHead::unlisted_keys_unknown]); seqs go on in `session` from
    /// `next_seq`.
    pub(crate) fn after<E>(
        head: &PieceHead,
        done: &Closed,
        (tracks, unlisted): (Vec<TrackStart>, bool),
        session: SessionTag,
        next_seq: u64,
    ) -> Result<Self, WriteError<E>> {
        let seq = head
            .ident
            .seq
            .checked_add(1)
            .ok_or(WriteError::Index(IndexError::Invalid(
                "piece seq past u64::MAX",
            )))?;
        Ok(Self {
            head: PieceHead {
                ident: PieceIdent {
                    id: PieceId::after(head.ident.id, &done.hash),
                    owner: head.ident.owner,
                    seq,
                },
                prev: Some((done.hash, head.ident.id)),
                prev_session: session,
                integrity: crate::Integrity::Full,
                unlisted_keys_unknown: unlisted,
            },
            tracks,
            session,
            next_seq,
            newest: relations::Newest::default(),
            grids: grids::Grids::default(),
            kept: kept::Kept::default(),
        })
    }
}

/// Whether a counted block placed is new, the last one again with the same bytes, or the last one's id with other
/// bytes, which takes its place.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Again {
    New,
    Same,
    Replaced,
}

/// How [TapeWriter::end] ends a piece.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum End {
    /// Closed for the reason given; the sink is closed.
    Close(CloseReason),
    /// Closed by a cut, naming the next piece; the sink is closed.
    Cut,
    /// Closed by a cut, the next piece behind it in the same sink, which stays open.
    CutInPlace,
    /// Closed ([CloseReason::Closed]), the sink kept open to write on after the close ([TapeWriter::close_and_go_on]).
    Kept,
}

/// A piece just ended, the session and seq after its last block, and its sink.
struct Ended<S> {
    closed: Closed,
    next_seq: u64,
    session: SessionTag,
    sink: S,
}

/// Session and seq of the block whose bytes start `block`, None when it is shorter than a header.
pub(crate) fn block_id(block: &[u8]) -> Option<BlockId> {
    let seq = u64::from_le_bytes(block.get(16..24)?.try_into().ok()?);
    let session = u32::from_le_bytes(block.get(24..28)?.try_into().ok()?);
    Some(BlockId::new(SessionTag::from_u32(session), seq))
}

/// The data tracks a block names: the tracks of a record block's records past track 0, a chunk's or summary's
/// track; none for a body that doesn't read.
fn data_tracks(b: &crate::Block<'_>) -> Vec<TrackNum> {
    if let Some(c) = b.chunk() {
        return vec![c.head().track_number];
    }
    if let Some(head) = b.summary_head() {
        return vec![head.track_number];
    }
    let mut buf = Vec::new();
    let Ok(d) = b.decode(&mut buf) else {
        return Vec::new();
    };
    let mut tracks: Vec<TrackNum> = d
        .records()
        .flatten()
        .filter_map(|r| r.track.data())
        .collect();
    tracks.sort_unstable();
    tracks.dedup();
    tracks
}
