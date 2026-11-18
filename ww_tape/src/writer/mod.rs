//! [TapeWriter]: blocks appended to a file, durable the way databases make their logs durable.
//!
//! - A new file is written as `NAME.partial` and renamed to `NAME` when it is finished; the file and its directory are
//!   synced after it is created and after the rename, so neither the data nor the name can vanish in a crash.
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
//! reader after a crash rescans only the stretch after the newest one. [TapeWriter::finish] ends the file as a
//! [piece](crate::piece): the final index sorted by track, the finish block with the BLAKE3 content hash of
//! everything before it, and the tail.
//!
//! The application keeps the definitions of what it works on ([Definitions]) and passes them with every call that
//! writes data or puts an item; the writer checks the data against them and keeps no table of its own. State is put
//! ([state](crate::state)): before a block of data of a track the writer puts the track's definition and what it
//! names, the application puts a state track's items ([TapeWriter::put]) at each use, and each is written only when
//! the same item doesn't lie close behind in the piece, or as a copy ([near]). The layout and the producer are the
//! writer's own ([WriterOptions]), put at each piece head. [TapeWriter::cut] ends a piece and starts the next one of
//! the chain; [crate::PieceChain] cuts by size, chunk count or time.
//!
//! The writer acts around every block of data it places, whoever made it (the caller, or the writer from its
//! deferred blocks): before it, the definitions it puts; after it, a checkpoint when due (in an aligned file, in the
//! gap before the next chunk). So what the writer adds is a function of the blocks before it and the calls since, and
//! [TapeWriter::resume] rebuilds the writer of an unfinished piece from its bytes alone ([resume]).
//!
//! A writer puts its bytes in a [TapeSink]: a file ([FileSink], the default) or memory ([crate::sink::MemSink]).

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::index::{IndexPolicy, Indexer, TrackKey};
use crate::open::{RecordRef, write_records};
use crate::piece::{
    ContentHash, Finish, FinishState, OwnerId, PIECE_ALIGN, PieceHead, PieceId, PieceIdent,
    TAIL_LEN, Tail, TrackHash, TrackStart, UnitHash, finish_len, finish_padding, hash_block,
    piece_head_len, write_finish, write_piece_head,
};
use crate::reader::{ReadError, wide};
pub use crate::sink::partial_name;
use crate::sink::{FileSink, Files, SinkFactory, TapeSink, io_err};
use crate::state::{ItemHash, Slot};
use crate::{
    BlockId, BlockKind, ClockTime, DESCRIBE_TRACK, Definitions, Describe, DescribeError,
    IndexError, Layout, Producer, STATE_ITEM_MAX, SessionTag, TypedItem,
};
use std::collections::BTreeSet;

pub mod near;
mod relations;
pub mod resume;
pub use near::NearPolicy;
use near::Rule;
pub(crate) use relations::Newest;
pub use resume::{RESUME_LOOK_MAX, RESUME_PROBES_MAX, Resumed};

/// When a [TapeWriter] syncs the data to the disk, besides at [TapeWriter::finish].
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
        SyncPolicy::Interval(DEFAULT_SYNC_INTERVAL)
    }
}

/// The header flags a caller's block of `kind` may carry when written as it is, not as puts: a coded body
/// ([FLAG_BODY_CODED](crate::FLAG_BODY_CODED)) on a records block, none on a chunk or a summary. The other bits are
/// reserved or the writer's own.
fn caller_flags(kind: BlockKind) -> u16 {
    match kind {
        BlockKind::Records => crate::FLAG_BODY_CODED,
        _ => 0,
    }
}

/// Default of [SyncPolicy::Interval].
pub const DEFAULT_SYNC_INTERVAL: Duration = Duration::from_secs(1);

/// Why a [TapeWriter] stopped.
#[derive(Debug)]
#[non_exhaustive]
pub enum WriteError {
    /// Opening, writing, syncing or renaming failed, with what was being done.
    Io {
        what: &'static str,
        error: io::Error,
    },
    /// An earlier write or sync failed (said then); the file takes no more blocks.
    Failed,
    /// `create` of a file that is already there.
    Exists(PathBuf),
    /// Another writer holds the file or memory tape: one writer per file ([crate::sink]).
    Locked(PathBuf),
    /// The `.partial` file `create` opened is no longer the one its name points to once locked: another writer
    /// finished and renamed it, or it was replaced. Nothing is written.
    Replaced(PathBuf),
    /// [MemTapes::remove](crate::MemTapes::remove) of a piece still being written.
    Unfinished(PathBuf),
    /// A deferred block isn't a good block.
    Block(crate::BlockError),
    /// A chunk doesn't encode.
    Chunk(crate::ChunkError),
    /// An alignment that is neither 0 nor [crate::ALIGNMENT].
    Alignment(u32),
    /// A session tag a writer may not use ([SessionTag::is_session]).
    Session(SessionTag),
    /// An index, finish, tail or piece head block doesn't encode, or a piece call came out of order (a piece head
    /// after other blocks, a cut of a file without a piece head).
    Index(IndexError),
    /// Reading back the piece to resume failed.
    Read(ReadError<io::Error>),
    /// The piece or chain to resume ended already in this state.
    Ended(FinishState),
    /// A put or a block's records don't agree with the definitions passed.
    Describe(DescribeError),
    /// The piece to resume describes another layout than the one `given` in the [WriterOptions], or a layout set
    /// after the first block.
    Layout { described: Layout, given: Layout },
    /// The piece to resume names another producer than the one `given` in the [WriterOptions].
    Producer { described: String, given: String },
    /// A caller's block the writer doesn't take: a kind it makes itself or doesn't know (the reserved kind 2), a
    /// block of another format version, or a body of a codec it doesn't know.
    Unknown { version: u8, kind: BlockKind },
    /// A block or chunk that would bring the tracks with data in the piece past `max`
    /// ([TRACKS_PER_PIECE_MAX](crate::TRACKS_PER_PIECE_MAX)), so its finish would not fit a block. Nothing is
    /// written; cut the piece first.
    TooManyTracks { max: usize },
    /// A caller's block of `kind` with header `flags` its kind doesn't take: reserved bits, or a flag only the
    /// writer sets. A records block may carry [FLAG_BODY_CODED](crate::FLAG_BODY_CODED), other kinds none.
    Flags { kind: BlockKind, flags: u16 },
    /// A resume would cut off a good block of the piece at `offset`, behind bytes the reader didn't take: more than a
    /// torn end. The bytes stay as they are.
    GoodBlockAfterCut { offset: u64 },
    /// A resume looked for good blocks past where the reader stopped and ran out of its look ([RESUME_LOOK_MAX]) or
    /// of the places it reads whole ([RESUME_PROBES_MAX]) at `offset`, before the end of the bytes: it can't tell the
    /// bytes from there on hold no good block, so it cuts nothing. The bytes stay as they are.
    LookSpent { offset: u64 },
    /// The content-hash walk of the bytes a resume keeps spent its budget of missed blocks
    /// ([crate::piece::WALK_MISS_PER_BYTE_MAX]): a block written at `end` would not verify. The bytes stay as they are.
    HashWalkSpent { end: u64 },
}

/// Why [TapeWriter::cut_in_place] failed: the error, and the writer when the cut was refused before anything was
/// written.
pub struct CutError<S: TapeSink> {
    /// Why.
    pub error: WriteError,
    /// The writer as it was, when the cut was refused before the piece ended; None once it failed past that.
    pub writer: Option<Box<TapeWriter<S>>>,
}

impl<S: TapeSink> From<WriteError> for CutError<S> {
    fn from(error: WriteError) -> Self {
        CutError {
            error,
            writer: None,
        }
    }
}

impl<S: TapeSink> From<CutError<S>> for WriteError {
    fn from(e: CutError<S>) -> Self {
        e.error
    }
}

impl<S: TapeSink> core::fmt::Debug for CutError<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CutError")
            .field("error", &self.error)
            .field("writer", &self.writer.is_some())
            .finish()
    }
}

impl<S: TapeSink> core::fmt::Display for CutError<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.writer {
            Some(_) => write!(f, "cut refused, the writer handed back: {}", self.error),
            None => write!(f, "cut failed: {}", self.error),
        }
    }
}

impl<S: TapeSink> std::error::Error for CutError<S> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

impl From<DescribeError> for WriteError {
    fn from(e: DescribeError) -> Self {
        WriteError::Describe(e)
    }
}

impl core::fmt::Display for WriteError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            WriteError::Io { what, error } => write!(f, "{what}: {error}"),
            WriteError::Failed => write!(
                f,
                "an earlier write or sync failed, the file takes no more blocks"
            ),
            WriteError::Exists(p) => write!(f, "{} exists", p.display()),
            WriteError::Locked(p) => write!(f, "another writer holds {}", p.display()),
            WriteError::Replaced(p) => write!(
                f,
                "{} was renamed or replaced while it was opened",
                p.display()
            ),
            WriteError::Unfinished(p) => write!(f, "{} is still being written", p.display()),
            WriteError::Block(e) => write!(f, "not a good block: {e:?}"),
            WriteError::Chunk(e) => write!(f, "chunk doesn't encode: {e:?}"),
            WriteError::Index(e) => write!(f, "index or piece block: {e:?}"),
            WriteError::Alignment(a) => {
                write!(f, "alignment {a} is neither 0 nor {}", crate::ALIGNMENT)
            }
            WriteError::Session(r) => write!(f, "session tag {r:?} is not one a writer may use"),
            WriteError::Read(e) => write!(f, "reading back the piece to resume: {e}"),
            WriteError::Ended(s) => write!(f, "the piece or chain ended already ({s:?})"),
            WriteError::Describe(e) => write!(f, "{e}"),
            WriteError::Layout { described, given } => write!(
                f,
                "the piece describes layout {described:?}, the writer was given {given:?} (or a layout set after the first block)"
            ),
            WriteError::Producer { described, given } => write!(
                f,
                "the piece names producer {described:?}, the writer was given {given:?}"
            ),
            WriteError::GoodBlockAfterCut { offset } => write!(
                f,
                "a resume would cut off a good block at {offset}, more than a torn end"
            ),
            WriteError::LookSpent { offset } => write!(
                f,
                "a resume looked for good blocks it would cut off up to {offset}, short of the end of the bytes"
            ),
            WriteError::HashWalkSpent { end } => write!(
                f,
                "the bytes to resume claim too many blocks that don't read: a block written at {end} would not verify"
            ),
            WriteError::TooManyTracks { max } => write!(
                f,
                "more than {max} tracks with data in one piece: cut the piece first"
            ),
            WriteError::Flags { kind, flags } => write!(
                f,
                "a caller's block of kind {kind:?} with header flags {flags:#06x}: it takes {:#06x}",
                caller_flags(*kind)
            ),
            WriteError::Unknown { version, kind } => write!(
                f,
                "a caller's block of version {version}, kind {kind:?}: the writer takes records, events, series and summary blocks of version {} it knows the body of",
                crate::VERSION
            ),
        }
    }
}

impl std::error::Error for WriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            WriteError::Io { error, .. } => Some(error),
            WriteError::Read(e) => Some(e),
            WriteError::Describe(e) => Some(e),
            _ => None,
        }
    }
}

/// How a [TapeWriter] places what it adds: what [TapeWriter::resume] needs to rebuild one, and what a
/// [crate::PieceChain] gives each of its pieces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriterOptions {
    pub sync: SyncPolicy,
    pub index: IndexPolicy,
    /// [TapeWriter::set_layout]: how the file's blocks are placed, put at each piece head; its alignment is 0 or
    /// [crate::ALIGNMENT].
    pub layout: Layout,
    /// [TapeWriter::set_producer]: the producer named at each piece head.
    pub producer: String,
    /// [TapeWriter::set_near_policy].
    pub near: NearPolicy,
}

impl Default for WriterOptions {
    fn default() -> Self {
        WriterOptions {
            sync: SyncPolicy::default(),
            index: IndexPolicy::default(),
            layout: Layout::UNALIGNED,
            producer: String::new(),
            near: NearPolicy::default(),
        }
    }
}

/// Appends blocks to a [TapeSink], a file by default, see the [module docs](self).
pub struct TapeWriter<S: TapeSink = FileSink> {
    sink: S,
    policy: SyncPolicy,
    unsynced: u32,
    last_sync: Instant,
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
    /// Seq of the next block placed by the writer: after the last block written
    next_seq: u64,
    /// Session of the blocks the writer makes: its own, or the session of the last counted block written
    session: SessionTag,
    /// Small blocks waiting for the gap before an aligned chunk payload, each with the definitions it needs
    deferred: Vec<Deferred>,
    deferred_bytes: usize,
    /// Reused for encoding chunk blocks
    chunk_buf: Vec<u8>,
    /// What is placed in the piece, for its checkpoints, final index and finish
    indexer: Indexer,
    /// A checkpoint waits for the gap before the next aligned chunk
    index_due: bool,
    last_checkpoint: Instant,
    /// The piece's content hash so far: BLAKE3 of its bytes, each chunk payload by its hash
    hash: blake3::Hasher,
    /// The piece head written first, and its track starts
    head: Option<(PieceHead, Vec<TrackStart>)>,
    /// The state records of the piece the writer remembers
    near: near::Near,
    near_policy: NearPolicy,
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
    /// Most tracks with data in the piece: [crate::piece::TRACKS_PER_PIECE_MAX], smaller in tests
    pub(crate) tracks_max: usize,
}

/// Bytes a writer's puts hashed and copied since it started ([TapeWriter::put_work]): what deciding and writing
/// them costs. A definition's hash is kept with it ([Definitions]), so a block of data whose definitions all lie
/// close behind costs none of either.
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

/// What a finished piece is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    /// The piece's name: its file, or its key in memory.
    pub path: PathBuf,
    /// BLAKE3 of the piece's bytes before its finish block.
    pub hash: ContentHash,
    /// Bytes of the piece, its tail included.
    pub len: u64,
    pub ident: Option<PieceIdent>,
    /// The next piece's id when the piece was [cut](TapeWriter::cut).
    pub next: Option<PieceId>,
    /// Where the piece starts in its file or memory tape: 0 for a piece of its own, past the pieces before it in
    /// a file of several.
    pub start: u64,
    /// Every track of the chain so far (carried over by the piece head, or with data in the piece), ascending id
    /// then owner: its [hash](crate::TrackHash) after this piece and where its items go on.
    pub tracks: Vec<TrackStart>,
}

/// Where [TapeWriter::write_chunk], [TapeWriter::write_block] or [TapeWriter::adopt_block] placed the block it was
/// asked to write.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Placed {
    /// Where the block starts, from the start of the piece.
    pub offset: u64,
    /// The whole block's length.
    pub len: u32,
}

/// Most bytes of a block of definitions or of puts the writer makes; a longer record gets a block of its own.
pub const DEFINITION_BLOCK_LEN: usize = 64 << 10;

/// Most bytes of deferred blocks a [TapeWriter] keeps before writing them out anyway.
pub const DEFERRED_BYTES_MAX: usize = 1 << 20;

/// Bytes a deferred block may wait for a gap, from where it was deferred: it is placed before a call would carry
/// the piece past that. Its definitions are put when it is deferred and none when it is placed, so a deferred block
/// lies at most its tracks' `near` plus this after them ([TapeWriter::defer_block]).
pub const DEFER_WINDOW: u64 = 1 << 20;

impl TapeWriter<FileSink> {
    /// A new file at `path`, written as `path.partial` until [Self::finish]; fails if `path` is there or a
    /// `path.partial` holds bytes ([FileSink::create]).
    pub fn create(path: &Path, policy: SyncPolicy) -> Result<Self, WriteError> {
        Ok(Self::create_on(FileSink::create(path)?, policy))
    }

    /// Append to the file at `path` in place, created if it isn't there. The blocks of another producer session start
    /// with its definitions, which a reader takes as the next part of the file.
    pub fn append(path: &Path, policy: SyncPolicy) -> Result<Self, WriteError> {
        let sink = FileSink::append(path)?;
        let start = sink.size().map_err(io_err("reading the file's length"))?;
        let mut w = Self::create_on(sink, policy);
        w.start = start;
        Ok(w)
    }

    /// Reopen the unfinished piece `path` (written as `path.partial` by [Self::create] and cut off by a crash) to go
    /// on writing it, see [Self::resume_on].
    pub fn resume(path: &Path, options: WriterOptions) -> Result<(Self, Resumed), WriteError> {
        match FileSink::reopen(path)? {
            Some(sink) => Self::resume_on(sink, options),
            None if path.exists() => Err(WriteError::Exists(path.to_path_buf())),
            None => Err(WriteError::Io {
                what: "opening the unfinished file",
                error: io::ErrorKind::NotFound.into(),
            }),
        }
    }

    /// End this piece as [FinishState::Continued] and start the next piece of the chain in a new file at
    /// `next_path`, see [Self::cut_on].
    pub fn cut(self, next_path: &Path) -> Result<(Finished, Self), WriteError> {
        self.cut_with(&mut Files, next_path)
    }
}

impl<S: TapeSink> TapeWriter<S> {
    /// A writer appending to `sink` (empty, or a writer's start is its length with [Self::create_on] and
    /// [TapeWriter::append] alike: offsets in the index are from where the writer starts).
    pub fn create_on(sink: S, policy: SyncPolicy) -> Self {
        TapeWriter {
            sink,
            policy,
            unsynced: 0,
            last_sync: Instant::now(),
            failed: false,
            blocks: 0,
            bytes: 0,
            start: 0,
            layout: Layout::UNALIGNED,
            producer: String::new(),
            head_put: false,
            next_seq: 0,
            session: SessionTag::random(),
            deferred: Vec::new(),
            deferred_bytes: 0,
            chunk_buf: Vec::new(),
            indexer: Indexer::new(IndexPolicy::default()),
            index_due: false,
            last_checkpoint: Instant::now(),
            hash: blake3::Hasher::new(),
            head: None,
            near: near::Near::default(),
            near_policy: NearPolicy::default(),
            chunks: 0,
            last_counted: None,
            adopted: None,
            holds_data: false,
            work: PutWork::default(),
            relations: relations::Relations::default(),
            tracks_max: crate::piece::TRACKS_PER_PIECE_MAX,
        }
    }

    /// Set what `options` say, past the sync policy given at creation.
    pub(crate) fn apply(&mut self, options: &WriterOptions) -> Result<(), WriteError> {
        self.set_layout(options.layout)?;
        self.set_producer(&options.producer);
        self.set_index_policy(options.index)?;
        self.set_near_policy(options.near);
        Ok(())
    }

    /// Ask the sink to make room for `bytes` more ([TapeSink::reserve]).
    pub(crate) fn reserve(&mut self, bytes: u64) {
        self.sink.reserve(bytes);
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
    /// over to the next piece of a [cut](Self::cut).
    pub fn set_near_policy(&mut self, policy: NearPolicy) {
        self.near_policy = policy;
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
    /// finish never meets the cap.
    /// The tracks of `tracks` (numbers held in `defs`) without a row in the finish yet, each once.
    fn new_tracks(&self, defs: &Definitions, tracks: &[u32]) -> BTreeSet<TrackKey> {
        tracks
            .iter()
            .filter_map(|n| defs.track_key(*n))
            .map(|(id, owner)| (id, OwnerId(owner)))
            .filter(|k| !self.indexer.has_row(k))
            .collect()
    }

    /// Refuse data on `tracks` (numbers held in `defs`) when it would bring the tracks with data in the piece,
    /// those of the deferred blocks counted, past [Self::tracks_max]: the finish of the piece must fit a block.
    fn track_room(&self, defs: &Definitions, tracks: &[u32]) -> Result<(), WriteError> {
        let mut new = self.new_tracks(defs, tracks);
        if new.is_empty() {
            return Ok(());
        }
        for d in &self.deferred {
            new.extend(d.keys.iter().filter(|k| !self.indexer.has_row(k)));
        }
        if self.indexer.rows().saturating_add(new.len()) > self.tracks_max {
            return Err(WriteError::TooManyTracks {
                max: self.tracks_max,
            });
        }
        Ok(())
    }

    fn room(&self, puts: usize, head: bool) -> Result<(), WriteError> {
        let head = if head { HEAD_ENTRIES } else { 0 };
        let n = puts.saturating_add(head);
        self.indexer.room_for_states(n).map_err(WriteError::Index)
    }

    /// Put `item` on state track `track` of `defs` at `ticks` of its clock: the producer uses it now, and puts it at
    /// each use. It is written, after the track's definition and what it names where those don't lie close behind,
    /// unless the newest record of the track is the same item at most the track's `near` behind in the piece and no
    /// copy is due ([near]). Deferred blocks are placed first. Whether the item was written.
    pub fn put(
        &mut self,
        defs: &Definitions,
        track: u32,
        ticks: i64,
        item: &[u8],
    ) -> Result<bool, WriteError> {
        self.check()?;
        let facts = defs
            .track(track)
            .ok_or(DescribeError::UnknownTrack { track })?;
        let policy = facts.state.ok_or(DescribeError::NotState { track })?;
        if item.len() > STATE_ITEM_MAX {
            return Err(DescribeError::TooLong { len: item.len() }.into());
        }
        let mut cands = self.needed(defs, &[track]);
        self.work.hash(item.len());
        cands.push(Candidate {
            slot: Slot::State { track },
            rule: self.rule(policy),
            track,
            at: Some(ClockTime {
                clock: facts.clock,
                ticks,
            }),
            hash: ItemHash::of(item),
            item: Item::Given(item),
        });
        self.room(0, !self.head_put)?;
        self.flush_deferred()?;
        self.ensure_head()?;
        self.put_last(&cands)
    }

    /// Put the typed item `item` on track 0, after its bundle where `defs` holds it and it doesn't lie close behind:
    /// written unless the same item lies within track 0's `near` ([crate::Layout::definitions]) and no copy is due.
    /// Whether it was written.
    pub fn put_item(
        &mut self,
        defs: &Definitions,
        item: &TypedItem<'_>,
    ) -> Result<bool, WriteError> {
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
            cands.push(Candidate::held(slot, rule, held));
        }
        self.work.hash(bytes.len());
        let hash = ItemHash::of(&bytes);
        cands.push(Candidate {
            slot: Slot::of_definition(&d, || hash),
            rule,
            track: DESCRIBE_TRACK,
            at: None,
            hash,
            item: Item::Owned(bytes),
        });
        self.room(0, !self.head_put)?;
        self.flush_deferred()?;
        self.ensure_head()?;
        self.put_last(&cands)
    }

    /// Put `cands` ([Self::choose]); whether the last one was written.
    fn put_last(&mut self, cands: &[Candidate<'_>]) -> Result<bool, WriteError> {
        self.put_chosen(cands, false)
    }

    /// Put `cands` ([Self::choose]), the last block marked as belonging with the next with `with_next`; whether the
    /// last one was written.
    /// Refused with nothing written when the piece has no room for the state entries of those chosen.
    fn put_chosen(&mut self, cands: &[Candidate<'_>], with_next: bool) -> Result<bool, WriteError> {
        let chosen = self.choose(cands)?;
        self.room(chosen.iter().filter(|c| **c).count(), false)?;
        let writes = chosen.last().copied().unwrap_or(false);
        self.write_chosen(cands, &chosen, with_next)?;
        Ok(writes)
    }

    /// How a put of a record of a state track of `policy` decides.
    fn rule(&self, policy: crate::StatePolicy) -> Rule {
        Rule {
            near: u64::from(policy.near.get()),
            copies: policy.copies.get(),
            spacing: self.near_policy.copy_spacing,
        }
    }

    /// How a put of a definition decides: track 0's policy, the layout's.
    fn definitions_rule(&self) -> Rule {
        self.rule(self.layout.definitions)
    }

    /// The writer's chunk alignment: 0 or [crate::ALIGNMENT].
    fn alignment(&self) -> u32 {
        self.layout.alignment.get()
    }

    /// The definitions data of `tracks` needs ([Definitions::needs]), each once, as candidates to put: shared with
    /// `defs`, nothing copied or hashed.
    fn needed(&self, defs: &Definitions, tracks: &[u32]) -> Vec<Candidate<'static>> {
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for t in tracks {
            self.push_needs(defs, defs.needs(*t), &mut seen, &mut out);
        }
        out
    }

    /// Push the definitions held in `slots` not `seen` yet onto `out`, as candidates.
    fn push_needs(
        &self,
        defs: &Definitions,
        slots: impl IntoIterator<Item = Slot>,
        seen: &mut BTreeSet<Slot>,
        out: &mut Vec<Candidate<'static>>,
    ) {
        let rule = self.definitions_rule();
        for slot in slots {
            if let Some(held) = defs.held(&slot)
                && seen.insert(slot)
            {
                out.push(Candidate::held(slot, rule, held));
            }
        }
    }

    /// Which of `cands` a put writes: each whose slot's newest record isn't the same item within its `near` of
    /// where the next block goes after the blocks written for the chosen ones, or is due a copy there; so a record
    /// skipped lies within `near` of the data or item that follows. Choosing one moves that place on and may choose
    /// more: repeated until nothing changes, at most once per candidate.
    fn choose(&self, cands: &[Candidate<'_>]) -> Result<Vec<bool>, WriteError> {
        let mut chosen = vec![false; cands.len()];
        loop {
            let end = self
                .offset()
                .saturating_add(wide(puts_len(&chosen_records(cands, &chosen))?));
            let mut more = false;
            for (c, pick) in cands.iter().zip(chosen.iter_mut()) {
                if !*pick && self.near.writes(&c.slot, c.hash, end, c.rule) {
                    *pick = true;
                    more = true;
                }
            }
            if !more {
                return Ok(chosen);
            }
        }
    }

    /// Write the records of `cands` that `chosen` marks, in order.
    fn write_chosen(
        &mut self,
        cands: &[Candidate<'_>],
        chosen: &[bool],
        with_next: bool,
    ) -> Result<(), WriteError> {
        let records = chosen_records(cands, chosen);
        for r in &records {
            self.work.copy(r.item.len());
        }
        self.write_puts(&records, with_next)
    }

    /// Before a block of data needing `cands`: put them, belonging with it.
    fn put_needed(&mut self, cands: &[Candidate<'_>]) -> Result<(), WriteError> {
        self.put_chosen(cands, true).map(|_| ())
    }

    /// Write the layout and the producer, whatever lies close behind: after a piece head, before the first block of a
    /// file without one, and where a caller's block starts another session.
    fn ensure_head(&mut self) -> Result<(), WriteError> {
        if self.head_put {
            return Ok(());
        }
        self.head_put = true;
        let layout = crate::open::encode(&Describe::Layout(self.layout))?;
        let producer = crate::open::encode(&Describe::Producer(Producer {
            name: &self.producer,
        }))?;
        let record = |item| RecordRef {
            track: DESCRIBE_TRACK,
            at: None,
            item,
        };
        // they belong with the call's block; at a piece head a resume puts them again when nothing follows
        self.write_puts(&[record(&layout), record(&producer)], true)
    }

    /// Write `records` as blocks marked [FLAG_REPEAT](crate::FLAG_REPEAT), with the seq of the last counted block,
    /// not counted themselves; the call that writes them checked their room first ([Self::room]).
    /// Every block but the last is marked [FLAG_WITH_NEXT](crate::FLAG_WITH_NEXT), the last one too with `with_next`:
    /// the call goes on with a block of data they serve.
    fn write_puts(&mut self, records: &[RecordRef<'_>], with_next: bool) -> Result<(), WriteError> {
        if records.is_empty() {
            return Ok(());
        }
        let id = self.structural_id();
        let mut blocks = Vec::new();
        write_records(
            records,
            DEFINITION_BLOCK_LEN,
            crate::FLAG_REPEAT,
            || id,
            |b| {
                blocks.push(b.to_vec());
                Ok::<(), DescribeError>(())
            },
        )?;
        let last = blocks.len().saturating_sub(1);
        for (i, mut b) in blocks.into_iter().enumerate() {
            if i < last || with_next {
                crate::mark_with_next(&mut b).map_err(WriteError::Block)?;
            }
            self.write_own(&b)?;
        }
        Ok(())
    }

    /// When checkpoint index blocks are placed (by default every 4 MiB).
    /// A policy whose [IndexPolicy::state_entries_max] has no room for a piece head's [HEAD_ENTRIES] is refused:
    /// every new piece of a chain would fail.
    pub fn set_index_policy(&mut self, policy: IndexPolicy) -> Result<(), WriteError> {
        if policy.state_entries_max < HEAD_ENTRIES {
            return Err(WriteError::Index(IndexError::StateEntries {
                max: policy.state_entries_max,
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
    ) -> Result<(), WriteError> {
        if head.prev.is_none() {
            self.session = SessionTag::of_piece(&head.ident.id);
        }
        self.write_piece_head(head, tracks)
    }

    fn write_piece_head(
        &mut self,
        head: &PieceHead,
        tracks: &[TrackStart],
    ) -> Result<(), WriteError> {
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
        self.indexer.start(tracks);
        self.write_own(&buf)?;
        self.head = Some((*head, tracks.to_vec()));
        self.near.clear();
        // the layout and the producer follow every piece head
        self.head_put = false;
        self.ensure_head()
    }

    /// Where the next block goes, from the start of the piece.
    fn offset(&self) -> u64 {
        self.bytes
    }

    /// Where the next block goes, from the start of the file.
    fn at(&self) -> Result<u64, WriteError> {
        self.start
            .checked_add(self.bytes)
            .ok_or(WriteError::Index(IndexError::Invalid(
                "a file past u64::MAX bytes",
            )))
    }

    /// Where the file is while it is written (`NAME.partial` for a new file).
    pub fn current_path(&self) -> &Path {
        self.sink.current_path()
    }

    pub fn blocks(&self) -> u64 {
        self.blocks
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Whether the piece has a piece head: written by [Self::begin_piece], or read back by a resume.
    pub(crate) fn has_head(&self) -> bool {
        self.head.is_some()
    }

    /// Place chunk payloads on multiples of `alignment` (0 or [crate::ALIGNMENT]), counted from the start of the file:
    /// the writer's layout with that alignment ([Self::set_layout]).
    pub fn set_alignment(&mut self, alignment: u32) -> Result<(), WriteError> {
        let layout = Layout {
            alignment: exact_units::Bytes::new(alignment),
            ..self.layout
        };
        self.set_layout(layout)
    }

    /// The layout the file's blocks are placed by, put at each piece head; its alignment is 0 or [crate::ALIGNMENT].
    /// Set before the first block: another layout after it is refused ([WriteError::Layout]).
    pub fn set_layout(&mut self, layout: Layout) -> Result<(), WriteError> {
        let alignment = layout.alignment.get();
        if alignment != 0 && alignment != crate::ALIGNMENT {
            return Err(WriteError::Alignment(alignment));
        }
        if self.head_put && layout != self.layout {
            return Err(WriteError::Layout {
                described: self.layout,
                given: layout,
            });
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

    /// The session of the blocks the writer makes: random for a new writer, [SessionTag::of_piece] for a chain's first piece,
    /// else the session of the last counted block written.
    pub fn session(&self) -> SessionTag {
        self.session
    }

    /// Make the blocks from here on part of session `session` (a deterministic producer's own tag), its seqs from 0. A tag
    /// a writer may not use ([SessionTag::is_session]) is refused.
    pub fn set_session(&mut self, session: SessionTag) -> Result<(), WriteError> {
        if !session.is_session() {
            return Err(WriteError::Session(session));
        }
        self.session = session;
        self.next_seq = 0;
        Ok(())
    }

    /// Id of the blocks the file structure adds (index, finish, tail, piece head) and of puts: the seq of the last
    /// counted block before them, 0 before the first.
    fn structural_id(&self) -> BlockId {
        BlockId::new(self.session, self.next_seq.saturating_sub(1))
    }

    /// Write `block` at the end: one the writer made itself, or a caller's checked before ([Self::check_data]).
    /// Where it landed in the piece.
    fn write_own(&mut self, block: &[u8]) -> Result<Placed, WriteError> {
        self.check()?;
        let (Some(header), Some(crc)) = (
            block.first_chunk::<{ crate::HEADER_LEN }>(),
            block.last_chunk::<{ crate::CRC_LEN }>(),
        ) else {
            return Err(WriteError::Block(crate::BlockError::Short {
                needed: crate::HEADER_LEN,
            }));
        };
        // a Block is at most MAX_BLOCK_LEN long: its length fits a u32
        let len = u32::try_from(block.len()).map_err(|_| {
            WriteError::Index(IndexError::Invalid("a block longer than u32::MAX bytes"))
        })?;
        self.end_after(block.len())?;
        if let Err(error) = self.sink.write_all(block) {
            self.failed = true;
            return Err(WriteError::Io {
                what: "writing a block",
                error,
            });
        }
        let offset = self.offset();
        self.account(block, header, *crc)?;
        self.unsynced = self.unsynced.saturating_add(1);
        let due = match self.policy {
            SyncPolicy::None => false,
            SyncPolicy::EveryBlocks(n) => self.unsynced >= n.max(1),
            SyncPolicy::Interval(t) => self.last_sync.elapsed() >= t,
        };
        if due {
            self.sync()?;
        }
        Ok(Placed { offset, len })
    }

    /// Check a block of the caller's against `defs`: a records, events, series or summary block of this version
    /// whose body this writer knows ([Definitions::check_block]): no definitions, every record on a held track that
    /// isn't a state track. Other kinds (the reserved kind 2 among them), other versions, bodies of an unknown
    /// codec and header flags the kind doesn't take ([caller_flags]) are refused. `b` is read with its CRCs checked ([crate::Block::read]).
    fn check_data(defs: &Definitions, b: &crate::Block<'_>) -> Result<(), WriteError> {
        let ours = matches!(
            b.kind,
            BlockKind::Records | BlockKind::Events | BlockKind::Series | BlockKind::Summary
        );
        if !ours || !b.body_known() {
            return Err(WriteError::Unknown {
                version: b.version,
                kind: b.kind,
            });
        }
        if b.flags & !caller_flags(b.kind) != 0 {
            return Err(WriteError::Flags {
                kind: b.kind,
                flags: b.flags,
            });
        }
        let mut buf = Vec::new();
        let b = b.decode(&mut buf).map_err(WriteError::Block)?;
        defs.check_block(&b).map_err(WriteError::Describe)
    }

    /// Where the piece ends once `len` more bytes are placed.
    fn end_after(&self, len: usize) -> Result<u64, WriteError> {
        self.bytes
            .checked_add(wide(len))
            .ok_or(WriteError::Index(IndexError::Invalid(
                "a file past u64::MAX bytes",
            )))
    }

    /// Note the whole block `block` placed at the end, with its `header` and `crc`: the content hash, the index,
    /// the state records it holds ([near]), the counts, the session and seq. What a resume replays for each block it
    /// keeps.
    fn account(
        &mut self,
        block: &[u8],
        header: &[u8; crate::HEADER_LEN],
        crc: [u8; crate::CRC_LEN],
    ) -> Result<(), WriteError> {
        let bytes = self.end_after(block.len())?;
        let chunk = hash_block(&mut self.hash, block);
        // the unit hash goes into the chunk's index entry and its track's hash: computed once, here
        let unit = chunk.as_ref().map_or(UnitHash::NONE, |(c, payload)| {
            TrackHash::chunk_unit(c, payload)
        });
        // the range from the chunk's own column, by the definition placed before it: a resume derives the same
        let range = chunk.as_ref().and_then(|(c, _)| self.relations.range(c));
        // blocks the file structure adds are not counted: seq stays the producer's count of its own blocks
        let kind = BlockKind::from_u8(header[5]);
        let flags = u16::from_le_bytes([header[6], header[7]]);
        let repeat = kind == BlockKind::Records && flags & crate::FLAG_REPEAT != 0;
        let counted = !repeat && !crate::index::is_structural(kind);
        let again = self.index(block, crc, counted, unit, range);
        // a counted block of data goes into its tracks' hashes once: written again with the same bytes it adds
        // nothing, with other bytes it takes the place of the one before
        let tally = counted && again != Again::Same;
        if tally {
            if again == Again::Replaced {
                self.indexer.undo_block();
            }
            self.indexer.begin_block();
            if let Some((c, _)) = &chunk {
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
        // counts against thresholds: past u64::MAX they stay due
        self.blocks = self.blocks.saturating_add(1);
        let data = match kind {
            BlockKind::Records => !tracks.is_empty(),
            BlockKind::Events | BlockKind::Series => true,
            _ => false,
        };
        if counted && data {
            self.holds_data = true;
        }
        if counted && matches!(kind, BlockKind::Events | BlockKind::Series) {
            self.chunks = self.chunks.saturating_add(1);
        }
        if counted && let Some(id) = block_id(block) {
            self.session = id.session;
            self.next_seq = id.seq.wrapping_add(1);
        }
        Ok(())
    }

    /// Take what the block `block` placed at `at` says: every state record (track 0's, and every record of a block
    /// marked [FLAG_REPEAT](crate::FLAG_REPEAT)) noted as written there ([near]), the track each definition of a
    /// track names, and, with `tally`, each record of data into its track's hash. Returns the data tracks it holds
    /// records or a chunk of.
    fn learn(&mut self, block: &[u8], at: u64, tally: bool) -> Vec<u32> {
        // every block placed was made or checked whole before: its CRCs are not checked again
        let Ok(b) = crate::Block::read_trusted(block) else {
            return Vec::new();
        };
        if let Some(c) = b.chunk() {
            return vec![c.head().track_number];
        }
        if b.kind != BlockKind::Records {
            return Vec::new();
        }
        let mut buf = Vec::new();
        let Ok(b) = b.decode(&mut buf) else {
            return Vec::new();
        };
        let repeat = b.is_repeat();
        let mut tracks = Vec::new();
        for r in b.records().flatten() {
            if r.track != DESCRIBE_TRACK && !repeat {
                tracks.push(r.track);
                if tally {
                    self.indexer
                        .tally_record(r.track, r.at.map_or(0, |a| a.ticks), r.item);
                }
                continue;
            }
            let Some(slot) = Slot::of(r.track, r.item) else {
                continue;
            };
            if matches!(slot, Slot::Track { .. })
                && let Ok(Describe::Track(t)) =
                    <Describe<'_> as shrink_wrap::DeserializeShrinkWrap>::from_ww_bytes(r.item)
            {
                self.indexer.define(t.number.0, t.id(), OwnerId(t.owner));
                self.relations
                    .define(t.number.0, crate::open::TrackFacts::of(&t));
            }
            self.near
                .wrote(slot, Some(ItemHash::of(r.item)), at, &self.near_policy);
        }
        tracks.sort_unstable();
        tracks.dedup();
        tracks
    }

    /// List `block`, placed here, in the index as readers take it: a counted block with the id of the last one is
    /// that block again, unlisted with the same bytes, in its place with other bytes. A chunk's entry carries
    /// `unit` and `range`.
    fn index(
        &mut self,
        block: &[u8],
        crc: [u8; crate::CRC_LEN],
        counted: bool,
        unit: UnitHash,
        range: Option<(u64, u64)>,
    ) -> Again {
        let id = block_id(block);
        let last = self
            .last_counted
            .filter(|(last, ..)| counted && id == Some(*last));
        let again = match last {
            Some((_, len, c, _)) if len == block.len() && c == crc => {
                self.indexer.placed_unlisted(block.len());
                Again::Same
            }
            Some((.., offset)) => {
                self.indexer.replaced(offset);
                self.indexer.placed(self.bytes, block, unit, range);
                Again::Replaced
            }
            None => {
                self.indexer.placed(self.bytes, block, unit, range);
                Again::New
            }
        };
        if counted && let Some(id) = id {
            self.last_counted = Some((id, block.len(), crc, self.bytes));
        }
        again
    }

    /// After every counted block, whoever placed it: a checkpoint when due (in an aligned file, in the gap before
    /// the next chunk).
    fn after_block(&mut self) -> Result<(), WriteError> {
        if self.checkpoint_due() {
            if self.alignment() == 0 {
                self.place_checkpoint()?;
            } else {
                self.index_due = true;
            }
        }
        Ok(())
    }

    /// Whether the index policy asks for a checkpoint now.
    fn checkpoint_due(&self) -> bool {
        let by_time = self
            .indexer
            .policy()
            .interval
            .is_some_and(|t| self.indexer.pending() > 0 && self.last_checkpoint.elapsed() >= t);
        self.indexer.due() || by_time
    }

    fn place_checkpoint(&mut self) -> Result<(), WriteError> {
        self.index_due = false;
        self.last_checkpoint = Instant::now();
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
    /// past [DEFERRED_BYTES_MAX] waiting, all are. Index, finish, tail and piece head blocks are the writer's own:
    /// refused with [WriteError::Index]. A producer's block of puts or copies is put at once ([Self::put_block]).
    pub fn defer_block(&mut self, defs: &Definitions, block: Vec<u8>) -> Result<(), WriteError> {
        self.check()?;
        let b = crate::Block::read(&block).map_err(WriteError::Block)?;
        if b.is_repeat() {
            return self.put_block(defs, &block);
        }
        let needs = self.defer_needs(defs, &b)?;
        if let Some(c) = b.chunk() {
            self.check_relations(defs, &c, false)?;
        }
        if self.alignment() == 0 {
            self.ensure_head()?;
            self.put_needed(&needs)?;
            return self.place(block);
        }
        self.place_overdue(puts_len_upper(&needs))?;
        self.ensure_head()?;
        self.put_chosen(&needs, false)?;
        // placed before any later block of its track: a chunk after it goes on from its values, filed under its
        // track's key once the definition naming the track's owner is placed
        if let Some(c) = b.chunk()
            && let Some(facts) = defs.track(c.head().track_number)
            && let Some(key) = self
                .indexer
                .key_of(c.head().track_number, Some(c.head().track_id))
        {
            self.relations.deferred(key, &c, facts);
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
            self.flush_deferred()?;
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
    ) -> Result<(), WriteError> {
        let head = c.head();
        if head.kind != BlockKind::Events {
            return Ok(());
        }
        let (Some(facts), Some(key)) = (
            defs.track(head.track_number),
            self.indexer.key_of(head.track_number, Some(head.track_id)),
        ) else {
            return Ok(());
        };
        self.relations
            .check(key, &facts, c, replacing)
            .map_err(WriteError::Describe)
    }

    /// Check `block` for [Self::defer_block] and the piece's room for the definitions it needs: those.
    fn defer_needs(
        &self,
        defs: &Definitions,
        b: &crate::Block<'_>,
    ) -> Result<Vec<Candidate<'static>>, WriteError> {
        if crate::index::is_structural(b.kind) {
            return Err(WriteError::Index(IndexError::Invalid(
                "index, finish, tail and piece head blocks are written by the writer",
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
    fn place_overdue(&mut self, ahead: usize) -> Result<(), WriteError> {
        let due = self.deferred.first().is_some_and(|d| {
            self.offset().saturating_add(wide(ahead)) > d.at.saturating_add(DEFER_WINDOW)
        });
        if due { self.flush_deferred() } else { Ok(()) }
    }

    /// Write every deferred block now, in the order they came; on an error the ones not yet placed stay.
    pub fn flush_deferred(&mut self) -> Result<(), WriteError> {
        while !self.deferred.is_empty() {
            let d = self.take_deferred();
            self.place(d.block)?;
        }
        Ok(())
    }

    /// The oldest deferred block, no longer waiting.
    fn take_deferred(&mut self) -> Deferred {
        let d = self.deferred.remove(0);
        // the sum of the deferred blocks' lengths
        self.deferred_bytes = self.deferred_bytes.saturating_sub(d.block.len());
        d
    }

    /// Place a deferred block (checked, and its definitions put, when it was deferred) as the writer's next counted
    /// block, nothing put before it, then what is due after it.
    fn place(&mut self, mut block: Vec<u8>) -> Result<(), WriteError> {
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
    ) -> Result<Placed, WriteError> {
        self.check()?;
        // the head and the column table are checked here and as the chunk is encoded: the payload is the caller's
        // columns, copied, not read back
        defs.check_chunk_head(head).map_err(WriteError::Describe)?;
        let specs: Vec<crate::ColumnSpec> = columns.iter().map(|(c, _)| *c).collect();
        if head.kind == crate::BlockKind::Events {
            defs.check_relation_columns(head.track_number, &specs)
                .map_err(WriteError::Describe)?;
        }
        // the longest the block can be, its padding at most the alignment: refused before anything is allocated or put
        let chunk_max =
            crate::chunk::chunk_len(head, &specs, crate::times::u32_to_usize(self.alignment()));
        if chunk_max > crate::MAX_BLOCK_LEN {
            return Err(WriteError::Chunk(crate::chunk::ChunkError::TooLong {
                len: chunk_max,
            }));
        }
        let needs = self.needed(defs, &[head.track_number]);
        self.room(0, !self.head_put)?;
        self.track_room(defs, &[head.track_number])?;
        self.ensure_head()?;
        let payload = specs.iter().fold(0usize, |n, c| n.saturating_add(c.len));
        let head_len = crate::chunk::chunk_head_len(head, columns.len(), payload);
        // a deferred block of the chunk's track goes before it, as in an unaligned file: so do those before it
        if self
            .deferred
            .iter()
            .any(|d| d.tracks.contains(&head.track_number))
        {
            self.flush_deferred()?;
        }
        if self.alignment() != 0 {
            // what this call may write: the deferred blocks, a checkpoint, the puts and the chunk
            let checkpoint = if self.index_due {
                self.indexer.checkpoint_bytes()
            } else {
                0
            };
            let ahead = self
                .deferred_bytes
                .saturating_add(checkpoint)
                .saturating_add(puts_len_upper(&needs))
                .saturating_add(chunk_max);
            self.place_overdue(ahead)?;
            loop {
                let gap = crate::chunk::chunk_padding(self.at()?, head_len, self.alignment())
                    .ok_or(WriteError::Alignment(self.alignment()))?;
                if !self
                    .deferred
                    .first()
                    .is_some_and(|next| next.block.len() <= gap)
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
        let chosen = self.choose(&needs)?;
        self.room(chosen.iter().filter(|c| **c).count(), false)?;
        let puts = puts_len(&chosen_records(&needs, &chosen))?;
        let at = self.at()?.saturating_add(wide(puts));
        let padding = crate::chunk::chunk_padding(at, head_len, self.alignment())
            .ok_or(WriteError::Alignment(self.alignment()))?;
        let len = crate::chunk::chunk_len(head, &specs, padding);
        let mut buf = core::mem::take(&mut self.chunk_buf);
        buf.resize(len, 0);
        let r = crate::chunk::write_chunk(&mut buf, self.next_id(), head, columns, padding)
            .map_err(WriteError::Chunk)
            .and_then(|n| {
                buf.truncate(n);
                if head.kind == crate::BlockKind::Events {
                    let chunk = crate::chunk::Chunk::read_head(&buf)
                        .map_err(crate::BlockError::Chunk)
                        .map_err(WriteError::Block)?;
                    crate::open::relation_columns_read(&chunk)?;
                    self.check_relations(defs, &chunk, false)?;
                }
                self.write_chosen(&needs, &chosen, true)?;
                if self.at()? != at {
                    return Err(WriteError::Index(IndexError::Invalid(
                        "the puts before a chunk took other bytes than chosen",
                    )));
                }
                self.write_own(&buf)
            });
        self.chunk_buf = buf;
        let placed = r?;
        self.after_block()?;
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
    ) -> Result<Option<Placed>, WriteError> {
        self.check()?;
        let b = crate::Block::read(block).map_err(WriteError::Block)?;
        if b.is_repeat() {
            return self.put_block(defs, block).map(|()| None);
        }
        self.write_read(defs, &b)
    }

    /// [Self::write_block] of a block that isn't a block of puts, read with its CRCs checked.
    fn write_read(
        &mut self,
        defs: &Definitions,
        b: &crate::Block<'_>,
    ) -> Result<Option<Placed>, WriteError> {
        let block = b.bytes();
        Self::check_data(defs, b)?;
        let tracks = data_tracks(b);
        self.track_room(defs, &tracks)?;
        let needs = self.needed(defs, &tracks);
        // a block of another session starts it: a reader takes a new session where track 0 comes first, so the
        // layout and the producer are put again in it
        let session = block_id(block).filter(|theirs| theirs.session != self.session);
        self.room(0, !self.head_put || session.is_some())?;
        // a block numbered the next id when called goes after the deferred blocks placed first: renumbered
        let asked = self.next_id();
        self.flush_deferred()?;
        let renumbered = block_id(block) == Some(asked) && self.next_id() != asked;
        if let Some(c) = b.chunk() {
            // a counted block with the last one's id takes its place
            let replacing = !renumbered
                && self
                    .last_counted
                    .is_some_and(|(id, ..)| block_id(block) == Some(id));
            self.check_relations(defs, &c, replacing)?;
        }
        if let Some(theirs) = session {
            self.session = theirs.session;
            self.next_seq = theirs.seq;
            self.head_put = false;
        }
        self.ensure_head()?;
        self.put_needed(&needs)?;
        let placed = if renumbered {
            let mut own = block.to_vec();
            crate::set_id_trusted(&mut own, self.next_id()).map_err(WriteError::Block)?;
            self.write_own(&own)?
        } else {
            self.write_own(block)?
        };
        self.after_block()?;
        Ok(Some(placed))
    }

    /// Append a block of any session as the writer's own: renumbered into the writer's session and next seq ([crate::set_id]),
    /// then as [Self::write_block]. On a raw medium a session starts only at a piece head naming the session before it, so a
    /// piece there is one session: a writer for raw media passes a producer's blocks (a tap's, of its own session) this way. A
    /// block passed twice gets two seqs, both data. Index, finish, tail and piece head blocks are the writer's own:
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
    ) -> Result<Option<Placed>, WriteError> {
        self.check()?;
        let b = crate::Block::read(block).map_err(WriteError::Block)?;
        if b.is_repeat() {
            return self.put_block(defs, block).map(|()| None);
        }
        let (kind, theirs) = (b.kind, BlockId::new(b.session, b.seq));
        if crate::index::is_structural(kind) {
            return Err(WriteError::Index(IndexError::Invalid(
                "index, finish, tail and piece head blocks are written by the writer",
            )));
        }
        Self::check_data(defs, &b)?;
        let tracks = data_tracks(&b);
        self.track_room(defs, &tracks)?;
        let needs = self.needed(defs, &tracks);
        self.room(0, !self.head_put)?;
        self.flush_deferred()?;
        if let Some(c) = b.chunk() {
            self.check_relations(defs, &c, false)?;
        }
        self.ensure_head()?;
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
        self.after_block()?;
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
    pub fn put_block(&mut self, defs: &Definitions, block: &[u8]) -> Result<(), WriteError> {
        self.check()?;
        let b = crate::Block::read(block).map_err(WriteError::Block)?;
        let mut buf = Vec::new();
        let b = b.decode(&mut buf).map_err(WriteError::Block)?;
        if b.kind != BlockKind::Records {
            return Err(WriteError::Unknown {
                version: b.version,
                kind: b.kind,
            });
        }
        let rule = self.definitions_rule();
        let mut cands: Vec<Candidate<'_>> = Vec::new();
        let mut seen = BTreeSet::new();
        // the hash of the newest record of each state track in the block: the same item again adds nothing
        let mut newest: std::collections::BTreeMap<u32, ItemHash> = Default::default();
        for r in b.records() {
            let r = r.map_err(WriteError::Block)?;
            let Some(at) = r.at else {
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
                        Describe::Track(t) => defs.needs(t.number.0),
                        Describe::Map(m) => vec![
                            Slot::Clock { number: m.from.0 },
                            Slot::Clock { number: m.to.0 },
                            slot,
                        ],
                        _ => vec![slot],
                    };
                    let mut needs = Vec::new();
                    self.push_needs(defs, named, &mut seen, &mut needs);
                    cands.extend(needs);
                    continue;
                }
                if let Describe::Item(item) = &d {
                    let bundle: Vec<u8> = item.ty.bundle.iter().filter_map(Result::ok).collect();
                    if let Some((slot, _)) = defs.bundle(&bundle) {
                        let mut needs = Vec::new();
                        self.push_needs(defs, [slot], &mut seen, &mut needs);
                        cands.extend(needs);
                    }
                }
                self.work.hash(r.item.len());
                let hash = ItemHash::of(r.item);
                cands.push(Candidate {
                    slot: Slot::of_definition(&d, || hash),
                    rule,
                    track: DESCRIBE_TRACK,
                    at: None,
                    hash,
                    item: Item::Given(r.item),
                });
                continue;
            };
            let facts = defs.check_on_track(r.track, at)?;
            let policy = facts
                .state
                .ok_or(DescribeError::NotState { track: r.track })?;
            if r.item.len() > STATE_ITEM_MAX {
                return Err(DescribeError::TooLong { len: r.item.len() }.into());
            }
            self.work.hash(r.item.len());
            let hash = ItemHash::of(r.item);
            if newest.insert(r.track, hash) == Some(hash) {
                continue;
            }
            let mut needs = Vec::new();
            self.push_needs(defs, defs.needs(r.track), &mut seen, &mut needs);
            cands.extend(needs);
            cands.push(Candidate {
                slot: Slot::State { track: r.track },
                rule: self.rule(policy),
                track: r.track,
                at: Some(at),
                hash,
                item: Item::Given(r.item),
            });
        }
        self.room(0, !self.head_put)?;
        self.flush_deferred()?;
        self.ensure_head()?;
        self.put_last(&cands).map(|_| ())
    }

    /// Pass on a live producer's block, as a relay does: a block of puts ([FLAG_REPEAT](crate::FLAG_REPEAT)) has its
    /// definitions held in `defs` ([Definitions::take_block]) and its records put ([Self::put_block]); any other
    /// block is written under `defs` ([Self::write_block]).
    pub fn relay_block(&mut self, defs: &mut Definitions, block: &[u8]) -> Result<(), WriteError> {
        let b = crate::Block::read(block).map_err(WriteError::Block)?;
        if !b.is_repeat() {
            self.check()?;
            return self.write_read(defs, &b).map(|_| ());
        }
        let mut buf = Vec::new();
        defs.take_block(&b.decode(&mut buf).map_err(WriteError::Block)?)?;
        self.put_block(defs, block)
    }

    /// Sync now if blocks wait and the interval of [SyncPolicy::Interval] is up; call it when no blocks come.
    pub fn tick(&mut self) -> Result<(), WriteError> {
        self.check()?;
        let by_time = self
            .indexer
            .policy()
            .interval
            .is_some_and(|t| self.indexer.pending() > 0 && self.last_checkpoint.elapsed() >= t);
        if by_time && self.alignment() == 0 {
            self.place_checkpoint()?;
        } else if by_time {
            self.index_due = true;
        }
        match self.policy {
            SyncPolicy::Interval(t) if self.unsynced > 0 && self.last_sync.elapsed() >= t => {
                self.sync()
            }
            _ => Ok(()),
        }
    }

    /// Sync the data written so far to the disk.
    pub fn sync(&mut self) -> Result<(), WriteError> {
        self.check()?;
        if let Err(error) = self.sink.sync() {
            // never retried, see the module docs
            self.failed = true;
            return Err(WriteError::Io {
                what: "syncing the file",
                error,
            });
        }
        self.unsynced = 0;
        self.last_sync = Instant::now();
        Ok(())
    }

    /// End the file as a complete piece ([FinishState::Complete]) and sync it; a new file gets its name, and its
    /// directory is synced.
    pub fn finish(self) -> Result<Finished, WriteError> {
        self.finish_as(FinishState::Complete)
    }

    /// End the file as a piece in `state`: the definitions made, the deferred blocks, the final index, the finish and
    /// the tail; then sync, rename and sync the directory as [Self::finish].
    pub fn finish_as(self, state: FinishState) -> Result<Finished, WriteError> {
        if state == FinishState::Continued {
            return Err(WriteError::Index(IndexError::Invalid(
                "a continued piece ends by cut",
            )));
        }
        self.end(state, true).map(|e| e.finished)
    }

    /// End this piece as [FinishState::Continued] and start the next piece of the chain on the sink `factory` makes
    /// for `next_path`, see [Self::cut_on].
    pub fn cut_with<F: SinkFactory<Sink = S>>(
        self,
        factory: &mut F,
        next_path: &Path,
    ) -> Result<(Finished, Self), WriteError> {
        self.cut_into(|| factory.create(next_path))
    }

    /// End this piece as [FinishState::Continued] and start the next piece of the chain on `next`: its id
    /// [PieceId::after] this one's id and final content hash (the finish's), its head naming this piece, the track
    /// starts moved on by this piece's chunks, then the layout and the producer put; a definition is put again
    /// before the next data that needs it. Seqs go on from this piece; the options carry over.
    pub fn cut_on(self, next: S) -> Result<(Finished, Self), WriteError> {
        self.cut_into(|| Ok(next))
    }

    /// [Self::cut_on] with the next sink made once this piece is finished.
    fn cut_into(
        self,
        next: impl FnOnce() -> Result<S, WriteError>,
    ) -> Result<(Finished, Self), WriteError> {
        let Some((head, starts)) = self.head.clone() else {
            return Err(WriteError::Index(IndexError::Invalid(
                "a cut needs a piece head",
            )));
        };
        let options = self.options();
        let tracks = self.indexer.next_starts(&starts);
        let newest = self.relations.newest().clone();
        let done = self.end(FinishState::Continued, true)?;
        let mut next_piece =
            NextPiece::after(&head, &done.finished, tracks, done.session, done.next_seq)?;
        next_piece.newest = newest;
        let next = TapeWriter::start_next(next()?, &options, next_piece)?;
        Ok((done.finished, next))
    }

    /// End this piece as [FinishState::Continued] and start the next piece of the chain right behind it in the same
    /// sink, as [Self::cut_on] does on a new one: the sink's window ([TapeSink::rebase]) moves to the end of this
    /// piece, a multiple of [PIECE_ALIGN] when this one started on one. The sink stays open: it isn't finished, a
    /// file keeps its name. A sink of one piece (`NAME.partial`, [TapeSink::holds_pieces] false) is refused before
    /// anything is written: a file of several pieces is written under its own name ([SinkFactory::create_whole]).
    ///
    /// A refusal before the cut hands the writer back in [CutError::writer], to go on with.
    pub fn cut_in_place(self) -> Result<(Finished, Self), CutError<S>> {
        let refuse = |w: Self, what| CutError {
            error: WriteError::Index(IndexError::Invalid(what)),
            writer: Some(Box::new(w)),
        };
        let Some((head, starts)) = self.head.clone() else {
            return Err(refuse(self, "a cut needs a piece head"));
        };
        if !self.sink.holds_pieces() {
            return Err(refuse(self, "a sink of one piece takes no cut in place"));
        }
        self.cut_in_place_with(&head, &starts)
            .map_err(CutError::from)
    }

    /// [Self::cut_in_place] once checked: the piece ends.
    fn cut_in_place_with(
        self,
        head: &PieceHead,
        starts: &[TrackStart],
    ) -> Result<(Finished, Self), WriteError> {
        let options = self.options();
        let tracks = self.indexer.next_starts(starts);
        let newest = self.relations.newest().clone();
        let mut done = self.end(FinishState::Continued, false)?;
        let mut next_piece =
            NextPiece::after(head, &done.finished, tracks, done.session, done.next_seq)?;
        next_piece.newest = newest;
        let base = done
            .finished
            .start
            .checked_add(done.finished.len)
            .ok_or(WriteError::Index(IndexError::Invalid(
                "a file past u64::MAX bytes",
            )))?;
        done.sink
            .rebase(base)
            .map_err(io_err("starting the next piece behind the last"))?;
        let next = TapeWriter::start_next(done.sink, &options, next_piece)?;
        Ok((done.finished, next))
    }

    /// The options the writer runs with: what [Self::resume] is to be given for this piece.
    pub fn options(&self) -> WriterOptions {
        WriterOptions {
            sync: self.policy,
            index: self.indexer.policy(),
            layout: self.layout,
            producer: self.producer.clone(),
            near: self.near_policy,
        }
    }

    /// A writer on `sink` starting the piece `next` of a chain: its head, then the layout and the producer put.
    /// Seqs go on from that piece.
    pub(crate) fn start_next(
        sink: S,
        options: &WriterOptions,
        next: NextPiece,
    ) -> Result<Self, WriteError> {
        let mut w = TapeWriter::create_on(sink, options.sync);
        w.apply(options)?;
        w.begin_next(next)?;
        Ok(w)
    }

    /// Start the empty sink as the piece `next` of a chain, see [Self::start_next].
    pub(crate) fn begin_next(&mut self, next: NextPiece) -> Result<(), WriteError> {
        self.next_seq = next.next_seq;
        self.session = next.session;
        self.relations.take_newest(&next.newest);
        self.write_piece_head(&next.head, &next.tracks)
    }

    /// Go on from `newest`, the newest values of the monotone relations of the pieces before: a resume's.
    pub(crate) fn take_newest(&mut self, newest: &relations::Newest) {
        self.relations.take_newest(newest);
    }

    /// End the piece in `state`; a continued piece with a head names the next piece's id, [PieceId::after] its own
    /// and the content hash in its finish. With `close` the sink is finished ([TapeSink::finish]), else it is
    /// synced and handed back for the next piece.
    fn end(mut self, state: FinishState, close: bool) -> Result<Ended<S>, WriteError> {
        self.flush_deferred()?;
        self.ensure_head()?;
        let mut prev = None;
        for part in self.indexer.final_parts() {
            let at = self.offset();
            let b = Indexer::final_block(&part, at, prev, self.structural_id())
                .map_err(WriteError::Index)?;
            self.write_own(&b)?;
            prev = Some(at);
        }
        let hash = ContentHash(*self.hash.clone().finalize().as_bytes());
        let ident = self.head.as_ref().map(|(h, _)| h.ident);
        let next = ident
            .filter(|_| state == FinishState::Continued)
            .map(|i| PieceId::after(i.id, &hash));
        let finish = Finish {
            state,
            hash,
            checkpoint: self.indexer.last_checkpoint(),
            final_index: prev,
            ident,
            next,
        };
        let ranges = self.indexer.track_ranges();
        let tracks = crate::index::next_starts(
            self.head
                .as_ref()
                .map(|(_, s)| s.as_slice())
                .unwrap_or_default(),
            &ranges,
        );
        let finish_offset = self.offset();
        // the piece ends, after its tail, on a multiple of PIECE_ALIGN
        let padding = finish_padding(finish_offset, ranges.len());
        let mut buf = vec![0u8; finish_len(ranges.len()).saturating_add(padding)];
        let n = write_finish(&mut buf, self.structural_id(), &finish, &ranges, padding)
            .map_err(WriteError::Index)?;
        buf.truncate(n);
        self.write_own(&buf)?;
        let too_long = WriteError::Index(IndexError::Invalid("a piece past u64::MAX bytes"));
        let piece_len = self.offset().checked_add(wide(TAIL_LEN)).ok_or(too_long)?;
        // finish_padding sizes the finish block so the tail ends the piece on PIECE_ALIGN; a piece that doesn't is
        // refused before its tail is written, so it never reads as finished
        if !piece_len.is_multiple_of(u64::from(PIECE_ALIGN)) {
            return Err(WriteError::Index(IndexError::Invalid(
                "the finish padding doesn't end the piece on PIECE_ALIGN bytes",
            )));
        }
        let tail = Tail {
            piece_len,
            finish_offset,
            finish_len: u32::try_from(n)
                .ok()
                .ok_or(WriteError::Index(IndexError::Invalid(
                    "finish longer than a block",
                )))?,
            state,
            hash,
            ident,
            next,
        };
        let mut tb = [0u8; TAIL_LEN];
        tail.write(&mut tb, self.structural_id());
        self.write_own(&tb)?;
        self.sync()?;
        if close {
            self.sink
                .finish()
                .map_err(io_err("giving the finished piece its name"))?;
        }
        let start = self.sink.base().saturating_add(self.start);
        Ok(Ended {
            finished: Finished {
                path: self.sink.path().to_path_buf(),
                hash,
                len: self.bytes,
                ident,
                next,
                start,
                tracks,
            },
            next_seq: self.next_seq,
            session: self.session,
            sink: self.sink,
        })
    }

    /// The index entries a checkpoint would list now, for tests and tools.
    pub fn pending_entries(&self) -> usize {
        self.indexer.pending()
    }

    fn check(&self) -> Result<(), WriteError> {
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
    Held(std::sync::Arc<[u8]>),
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

/// A record a put may write: its slot, the rule of its track, the record and its item's hash.
#[derive(Clone, Debug)]
struct Candidate<'a> {
    slot: Slot,
    rule: Rule,
    track: u32,
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
            track: DESCRIBE_TRACK,
            at: None,
            item: Item::Held(held.item.clone()),
            hash: held.hash,
        }
    }
}

/// The records of `cands` that `chosen` marks, in order.
fn chosen_records<'c>(cands: &'c [Candidate<'_>], chosen: &[bool]) -> Vec<RecordRef<'c>> {
    cands
        .iter()
        .zip(chosen)
        .filter(|(_, c)| **c)
        .map(|(c, _)| RecordRef {
            track: c.track,
            at: c.at,
            item: c.item.bytes(),
        })
        .collect()
}

/// A block waiting for the gap before an aligned chunk payload, its definitions put when it came, and where the
/// piece was then.
struct Deferred {
    block: Vec<u8>,
    at: u64,
    /// The data tracks it holds: placed before any later block of them
    tracks: Vec<u32>,
    /// Those of them without a row in the finish when it was deferred: counted as with data ([TapeWriter::track_room])
    keys: BTreeSet<TrackKey>,
}

/// Bytes of the blocks of puts holding `records`, as [TapeWriter::write_puts] writes them.
fn puts_len(records: &[RecordRef<'_>]) -> Result<usize, WriteError> {
    let mut len = 0usize;
    write_records(
        records,
        DEFINITION_BLOCK_LEN,
        crate::FLAG_REPEAT,
        || BlockId::new(SessionTag::NONE, 0),
        |b| {
            len = len.saturating_add(b.len());
            Ok::<(), DescribeError>(())
        },
    )?;
    Ok(len)
}

/// At least the bytes of the blocks of puts holding all of `cands`, without making them: each record in a block of
/// its own at most.
fn puts_len_upper(cands: &[Candidate<'_>]) -> usize {
    cands.iter().fold(0usize, |n, c| {
        n.saturating_add(c.item.bytes().len())
            .saturating_add(crate::MAX_RECORD_HEAD)
            .saturating_add(crate::open::EMPTY_LEN)
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
}

impl NextPiece {
    /// The piece after the one headed `head` that ended as `done`, continued: its id [PieceId::after] that one's id
    /// and content hash, its head naming that piece, its track starts `tracks`; seqs go on in `session` from
    /// `next_seq`.
    pub(crate) fn after(
        head: &PieceHead,
        done: &Finished,
        tracks: Vec<TrackStart>,
        session: SessionTag,
        next_seq: u64,
    ) -> Result<Self, WriteError> {
        let seq = head
            .ident
            .seq
            .checked_add(1)
            .ok_or(WriteError::Index(IndexError::Invalid(
                "piece seq past u64::MAX",
            )))?;
        Ok(NextPiece {
            head: PieceHead {
                ident: PieceIdent {
                    id: PieceId::after(head.ident.id, &done.hash),
                    owner: head.ident.owner,
                    seq,
                },
                prev: Some((done.hash, head.ident.id)),
                prev_session: session,
                integrity: crate::Integrity::Full,
            },
            tracks,
            session,
            next_seq,
            newest: relations::Newest::default(),
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

impl NextPiece {
    /// The piece a new write session starts after the piece headed `head` that ended as `done` (complete or
    /// aborted, or continued with no piece after it): its id [PieceId::after] that one's id and content hash, its
    /// head naming that piece and `prev_session`, the session that wrote it; its own session [SessionTag::of_piece]
    /// its id, seqs from 0; its track starts `tracks`.
    pub(crate) fn reopened(
        head: &PieceHead,
        done: &Finished,
        tracks: Vec<TrackStart>,
        prev_session: SessionTag,
    ) -> Result<Self, WriteError> {
        let mut next = Self::after(head, done, tracks, prev_session, 0)?;
        next.session = SessionTag::of_piece(&next.head.ident.id);
        Ok(next)
    }
}

/// A piece just ended, the session and seq after its last block, and its sink.
struct Ended<S> {
    finished: Finished,
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
fn data_tracks(b: &crate::Block<'_>) -> Vec<u32> {
    if let Some(c) = b.chunk() {
        return vec![c.head().track_number];
    }
    if let Some(s) = b.summary() {
        return vec![s.head().track_number];
    }
    let mut buf = Vec::new();
    let Ok(d) = b.decode(&mut buf) else {
        return Vec::new();
    };
    let mut tracks: Vec<u32> = d
        .records()
        .flatten()
        .map(|r| r.track)
        .filter(|t| *t != crate::DESCRIBE_TRACK)
        .collect();
    tracks.sort_unstable();
    tracks.dedup();
    tracks
}
