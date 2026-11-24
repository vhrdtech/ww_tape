//! [WriteError]: why a [TapeWriter] stopped, and [CutError].

use super::{TapeWriter, caller_flags};
use crate::piece::PieceId;
use crate::reader::ReadError;
use crate::sink::TapeSink;
use crate::{BlockKind, DescribeError, IndexError, Layout, SessionTag};
use alloc::boxed::Box;
use alloc::string::String;

/// Why a [TapeWriter] stopped; `E` is the error of its sink ([TapeSink::Error]).
///
/// Its text says what the writer was doing and the error it wraps (`creating a writer: x.wwt: locked by another
/// writer`); the wrapped error's own [source](core::error::Error::source), the system's error, is its source, so each
/// text is said once.
#[derive(Debug)]
#[non_exhaustive]
pub enum WriteError<E> {
    /// The sink or its store failed or refused (another writer holds the bytes, a new piece is there already),
    /// with what was being done.
    Sink { what: &'static str, error: E },
    /// An earlier write or sync failed (said then); the file takes no more blocks.
    Failed,
    /// A deferred block isn't a good block.
    Block(crate::BlockError),
    /// A chunk doesn't encode.
    Chunk(crate::ChunkError),
    /// An index, close, tail or piece head block doesn't encode, or a piece call came out of order (a piece head
    /// after other blocks, a cut of a file without a piece head).
    Index(IndexError),
    /// Reading back the piece to resume failed.
    Read(ReadError<E>),
    /// A put or a block's records don't agree with the definitions passed.
    Describe(DescribeError),
    /// The writer refused a caller's value, block or options; nothing is written.
    Input(InputError),
    /// A resume refused to cut the bytes of the piece; they stay as they are.
    Resume(ResumeError),
    /// A seal refused the piece ([TapeWriter::seal_on](super::TapeWriter::seal_on)).
    Seal(super::SealError),
}

/// What a caller passed that the writer refuses ([WriteError::Input]); nothing is written.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum InputError {
    /// An alignment that is neither 0 nor [crate::ALIGNMENT].
    Alignment(u32),
    /// A session tag a writer may not use ([SessionTag::is_session]).
    Session(SessionTag),
    /// The piece to resume describes another layout than the one `given` in the
    /// [WriterOptions](super::WriterOptions), or a layout set after the first block.
    Layout { described: Layout, given: Layout },
    /// The piece to resume names another producer than the one `given` in the
    /// [WriterOptions](super::WriterOptions).
    Producer { described: String, given: String },
    /// A caller's block the writer doesn't take: a kind it makes itself or doesn't know (the reserved kind 2), a
    /// block of another format version, or a body of a codec it doesn't know.
    Unknown { version: u8, kind: BlockKind },
    /// A block or chunk that would bring the tracks with data in the piece past `max`
    /// ([TRACKS_PER_PIECE_MAX](crate::TRACKS_PER_PIECE_MAX)), so its close would not fit a block. Nothing is
    /// written; cut the piece first.
    TooManyTracks { max: usize },
    /// The next piece's head would carry more than `max` tracks ([HEAD_TRACKS_MAX](crate::HEAD_TRACKS_MAX)): each
    /// track the chain's pieces carried or gave data. Nothing is written; a new chain takes the track.
    TooManyCarried { max: usize },
    /// The call's state records, the definitions its tracks need and its puts, take `needed` state entries with the
    /// layout and the producer, more than the index policy's `max`
    /// ([IndexPolicy::state_entries_max](crate::index::IndexPolicy)): no piece can hold the call, so no cut is made
    /// for it. Nothing is written. A cut is never refused for the definitions it carries: it leaves out what doesn't
    /// fit ([TrackStart::not_carried](crate::piece::TrackStart::not_carried)).
    CarriedEntries { needed: usize, max: usize },
    /// A caller's block of `kind` with header `flags` its kind doesn't take: reserved bits, or a flag only the
    /// writer sets. A records block may carry [FLAG_BODY_CODED](crate::FLAG_BODY_CODED), other kinds none.
    Flags { kind: BlockKind, flags: u16 },
    /// A close's summary body of `len` bytes, more than [CLOSE_SUMMARY_MAX](crate::piece::CLOSE_SUMMARY_MAX) (`max`).
    SummaryTooLong { len: usize, max: usize },
    /// Removed pieces ([TapeWriter::record_removed](super::TapeWriter::record_removed)) the piece's close can't name:
    /// see [RemovedRefusal].
    Removed(RemovedRefusal),
    /// The chunks of a unit ([TapeWriter::write_chunk_unit](super::TapeWriter::write_chunk_unit)) are of more than
    /// one track.
    UnitTracks,
    /// A chunk marked [FLAG_WITH_NEXT](crate::FLAG_WITH_NEXT), or the chunks of a
    /// [TapeWriter::write_chunk_unit](super::TapeWriter::write_chunk_unit), that would make a unit longer than `max`
    /// chunks ([UNIT_CHUNKS_MAX](super::UNIT_CHUNKS_MAX)). Nothing is written.
    UnitTooLong { max: usize },
    /// A group of puts ([TapeWriter::put_group](super::TapeWriter::put_group)) the writer refuses: see
    /// [GroupRefusal].
    Group(GroupRefusal),
    /// A call that would make a run of blocks marked [FLAG_WITH_NEXT](crate::FLAG_WITH_NEXT) and the block they
    /// belong with longer than `max` blocks ([RUN_BLOCKS_MAX](crate::RUN_BLOCKS_MAX)). Nothing is written.
    RunTooLong { max: usize },
    /// A put by key that would make the keys every cut carries take `needed` of `limit`'s `max`: a piece could not
    /// hold them with the layout, the producer and one put more. Nothing is written; a deletion makes room.
    CarryFull {
        needed: usize,
        max: usize,
        limit: CarryLimit,
    },
}

/// The limit a put by key would bring the carry of every live key past ([InputError::CarryFull]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CarryLimit {
    /// The index policy's state entries a piece lists
    /// ([IndexPolicy::state_entries_max](crate::index::IndexPolicy::state_entries_max)): the definitions of the
    /// keyed tracks, a record per live key, the layout, the producer and one put.
    StateEntries,
    /// The bytes of state items the writer keeps for its cuts
    /// ([NearPolicy::kept_bytes_max](super::NearPolicy::kept_bytes_max)): the items of the live keys.
    KeptBytes,
}

impl core::fmt::Display for CarryLimit {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::StateEntries => "the index policy's state_entries_max",
            Self::KeptBytes => "the near policy's kept_bytes_max",
        })
    }
}

/// Why [TapeWriter::put_group](super::TapeWriter::put_group) refuses a group, or a relay drops a producer's group it
/// holds ([TapeWriter::put_block](super::TapeWriter::put_block)): [InputError::Group]. None of the group is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum GroupRefusal {
    /// The group's items take `len` bytes, more than [GROUP_BYTES_MAX](super::GROUP_BYTES_MAX) (`max`).
    TooLong { len: usize, max: usize },
    /// The group puts on track `track` twice, or on a key of keyed track `track` twice: a slot takes one item per
    /// group, relayed or not.
    SlotTwice { track: crate::TrackNum },
    /// The producer of session `session` tore the group a relay held of it after `blocks` blocks: its block was the
    /// first of another group or none of a group's. That block is put; the group held is dropped.
    Torn { session: SessionTag, blocks: usize },
    /// A block of a group of the producer of session `session` whose group the relay doesn't hold: its first block
    /// never came, or the group was dropped. The block is passed over.
    NoStart { session: SessionTag },
    /// The first block of a group of the producer of session `session` while the relay holds groups of `max`
    /// producers ([RELAY_GROUPS_MAX](super::RELAY_GROUPS_MAX)). The group is passed over.
    TooManyHeld { session: SessionTag, max: usize },
    /// A block that would bring what the groups a relay holds cost to `len` bytes, each record its bytes and
    /// [RELAY_RECORD_OVERHEAD](super::RELAY_RECORD_OVERHEAD), more than
    /// [RELAY_HELD_BYTES_MAX](super::RELAY_HELD_BYTES_MAX) (`max`). The group of session `session` is dropped.
    HeldTooLong {
        session: SessionTag,
        len: usize,
        max: usize,
    },
}

/// Why [TapeWriter::record_removed](super::TapeWriter::record_removed) refuses removed pieces ([InputError::Removed]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RemovedRefusal {
    /// The piece has no head, so no chain: a file written without one.
    NoChain,
    /// The piece of sequence number `seq` is not before the piece written: only earlier pieces are removed.
    NotBefore { seq: u64 },
    /// The piece of sequence number `seq` is named already with another id or hash.
    Conflict { seq: u64 },
    /// The close would name more than `max` removed pieces
    /// ([REMOVED_PIECES_MAX](crate::piece::REMOVED_PIECES_MAX)).
    TooMany { max: usize },
    /// The piece of sequence number `seq` is not a closed piece of the chain at hand: [PieceChain::remove](crate::PieceChain::remove)
    /// takes only those.
    NotAtHand { seq: u64 },
    /// The piece of sequence number `seq` lies inside a tape of several pieces, after one that stays: only the front
    /// of such a tape is freed.
    InTape { seq: u64 },
}

/// Why a resume cuts nothing ([WriteError::Resume]): the bytes of the piece stay as they are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ResumeError {
    /// There is no piece to resume: no unclosed piece, or no first piece of the chain.
    NoPiece,
    /// The resume would cut off a good block of the piece at `offset`, behind bytes the reader didn't take: more
    /// than a torn end.
    GoodBlockAfterCut { offset: u64 },
    /// The resume looked for good blocks past where the reader stopped and ran out of its look
    /// ([RESUME_LOOK_MAX](super::RESUME_LOOK_MAX)) or of the places it reads whole
    /// ([RESUME_PROBES_MAX](super::RESUME_PROBES_MAX)) at `offset`, before the end of the bytes: it can't tell the
    /// bytes from there on hold no good block, so it cuts nothing.
    LookSpent { offset: u64 },
    /// The content-hash walk of the bytes a resume keeps spent its budget of missed blocks
    /// ([crate::piece::WALK_MISS_PER_BYTE_MAX]): a block written at `end` would not verify.
    HashWalkSpent { end: u64 },
    /// The piece to resume is closed by a cut and goes on in the piece `next`: only the last piece of a chain is
    /// written on, so the links between pieces never move.
    Continued { next: PieceId },
    /// The bytes a resume would cut hold a whole record at `offset` that fails its check, or a tail that doesn't end
    /// the close before it: damage, not a torn end. Only an end the bytes stop inside of, or blank (zero) bytes, is
    /// cut.
    Damaged { offset: u64 },
    /// The pieces of sequence numbers `from..to` are missing from the middle of the chain, and the closes at hand don't
    /// name them as removed on purpose ([TapeWriter::record_removed](super::TapeWriter::record_removed)) or name them
    /// with ends that don't link to the pieces on either side.
    Gap { from: u64, to: u64 },
    /// A piece after removed pieces was cut off inside its head by a crash: the piece before it is gone, so its head
    /// can't be written again. A damaged head with more bytes after it is refused as the piece's own resume refuses it
    /// ([Self::GoodBlockAfterCut], [Self::Damaged]). `seq` is its sequence number, None when nothing at hand gives it
    /// (the front of a tape of several whose earlier pieces were dropped).
    HeadLost { seq: Option<u64> },
    /// The piece's carry of keyed state track `track`, which its head promises ([TrackStart::keys_carried]), was lost
    /// in a crash, and the writer doesn't know every live key to put it again: only the pieces before hold them.
    /// Resume the piece through its chain ([PieceChain::resume_on](crate::PieceChain::resume_on)).
    ///
    /// [TrackStart::keys_carried]: crate::piece::TrackStart::keys_carried
    KeysUnknown { track: crate::TrackNum },
    /// The piece's carry of keyed state track `track`, which its head promises, was lost in a crash, and the call's
    /// definitions don't hold the track under its row's id: the carry can't be put again without it. Pass the
    /// definitions the resume read back ([Resumed](super::Resumed)).
    KeysUndefined { track: crate::TrackNum },
}

/// Why a cut ([TapeWriter::cut_on], [TapeWriter::cut_with], [TapeWriter::cut_in_place]) failed: the error, and the
/// writer when the cut was refused before anything was written.
pub struct CutError<S: TapeSink> {
    /// Why.
    pub error: WriteError<S::Error>,
    /// The writer as it was, when the cut was refused before the piece ended; None once it failed past that.
    pub writer: Option<Box<TapeWriter<S>>>,
}

impl<S: TapeSink> From<WriteError<S::Error>> for CutError<S> {
    fn from(error: WriteError<S::Error>) -> Self {
        Self {
            error,
            writer: None,
        }
    }
}

impl<S: TapeSink> From<CutError<S>> for WriteError<S::Error> {
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

impl<S: TapeSink> core::error::Error for CutError<S> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        // the error's text is in this one's
        core::error::Error::source(&self.error)
    }
}

impl<E> From<InputError> for WriteError<E> {
    fn from(e: InputError) -> Self {
        Self::Input(e)
    }
}

impl<E> From<ResumeError> for WriteError<E> {
    fn from(e: ResumeError) -> Self {
        Self::Resume(e)
    }
}

impl<E> From<super::SealError> for WriteError<E> {
    fn from(e: super::SealError) -> Self {
        Self::Seal(e)
    }
}

impl<E> From<DescribeError> for WriteError<E> {
    fn from(e: DescribeError) -> Self {
        Self::Describe(e)
    }
}

impl<E: core::fmt::Display + core::fmt::Debug> core::fmt::Display for WriteError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Sink { what, error } => write!(f, "{what}: {error}"),
            Self::Failed => write!(
                f,
                "an earlier write or sync failed, the file takes no more blocks"
            ),
            Self::Block(e) => write!(f, "not a good block: {e}"),
            Self::Chunk(e) => write!(f, "chunk doesn't encode: {e}"),
            Self::Index(e) => write!(f, "index or piece block: {e}"),
            Self::Read(e) => write!(f, "reading back the piece to resume: {e}"),
            Self::Describe(e) => write!(f, "{e}"),
            Self::Input(e) => write!(f, "{e}"),
            Self::Resume(e) => write!(f, "{e}"),
            Self::Seal(e) => write!(f, "sealing: {e}"),
        }
    }
}

impl core::fmt::Display for InputError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Alignment(a) => {
                write!(f, "alignment {a} is neither 0 nor {}", crate::ALIGNMENT)
            }
            Self::Session(r) => write!(f, "session tag {r:?} is not one a writer may use"),
            Self::Layout { described, given } => write!(
                f,
                "the piece describes layout {described:?}, the writer was given {given:?} (or a layout set after the first block)"
            ),
            Self::Producer { described, given } => write!(
                f,
                "the piece names producer {described:?}, the writer was given {given:?}"
            ),
            Self::TooManyTracks { max } => write!(
                f,
                "more than {max} tracks with data in one piece: cut the piece first"
            ),
            Self::TooManyCarried { max } => write!(
                f,
                "more than {max} tracks carried into the next piece's head: start a new chain"
            ),
            Self::CarriedEntries { needed, max } => write!(
                f,
                "the call's definitions and puts take {needed} state entries with the layout and the producer, more than the index policy's {max}"
            ),
            Self::CarryFull { needed, max, limit } => write!(
                f,
                "the live keys a cut carries would take {needed}, more than {limit} ({max}): delete keys first"
            ),
            Self::UnitTracks => f.write_str("a unit of chunks of more than one track"),
            Self::UnitTooLong { max } => write!(f, "a unit of more than {max} chunks"),
            Self::RunTooLong { max } => write!(
                f,
                "a run of blocks belonging with the next of more than {max} blocks"
            ),
            Self::Group(GroupRefusal::TooLong { len, max }) => write!(
                f,
                "a group of puts of {len} bytes of items: a group holds at most {max}"
            ),
            Self::Group(GroupRefusal::SlotTwice { track }) => {
                write!(f, "a group of puts names track {track:?} twice")
            }
            Self::Group(GroupRefusal::Torn { session, blocks }) => write!(
                f,
                "producer {session:?} tore the group of puts relayed after {blocks} blocks: the group is dropped"
            ),
            Self::Group(GroupRefusal::NoStart { session }) => write!(
                f,
                "a block of a group of puts of producer {session:?} whose group no relay holds: passed over"
            ),
            Self::Group(GroupRefusal::TooManyHeld { session, max }) => write!(
                f,
                "a group of puts of producer {session:?} while groups of {max} producers are held: passed over"
            ),
            Self::Group(GroupRefusal::HeldTooLong { session, len, max }) => write!(
                f,
                "the group of puts of producer {session:?} would bring the groups held to {len} bytes, more than {max}: dropped"
            ),
            Self::SummaryTooLong { len, max } => write!(
                f,
                "a close's summary of {len} bytes: it holds at most {max}"
            ),
            Self::Removed(RemovedRefusal::NoChain) => {
                write!(f, "removed pieces named by a piece without a head")
            }
            Self::Removed(RemovedRefusal::NotBefore { seq }) => {
                write!(f, "removed piece {seq} is not before the piece written")
            }
            Self::Removed(RemovedRefusal::Conflict { seq }) => write!(
                f,
                "removed piece {seq} is named already with another id or hash"
            ),
            Self::Removed(RemovedRefusal::TooMany { max }) => {
                write!(f, "more than {max} removed pieces in one close")
            }
            Self::Removed(RemovedRefusal::NotAtHand { seq }) => {
                write!(f, "piece {seq} is not a closed piece of the chain at hand")
            }
            Self::Removed(RemovedRefusal::InTape { seq }) => write!(
                f,
                "piece {seq} lies inside the chain's tape after a piece that stays: only the tape's front is freed"
            ),
            Self::Flags { kind, flags } => write!(
                f,
                "a caller's block of kind {kind:?} with header flags {flags:#06x}: it takes {:#06x}",
                caller_flags(*kind)
            ),
            Self::Unknown { version, kind } => write!(
                f,
                "a caller's block of version {version}, kind {kind:?}: the writer takes records, events, series and summary blocks of version {} it knows the body of",
                crate::VERSION
            ),
        }
    }
}

impl core::error::Error for InputError {}

impl core::fmt::Display for ResumeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoPiece => write!(f, "there is no piece to resume"),
            Self::GoodBlockAfterCut { offset } => write!(
                f,
                "a resume would cut off a good block at {offset}, more than a torn end"
            ),
            Self::LookSpent { offset } => write!(
                f,
                "a resume looked for good blocks it would cut off up to {offset}, short of the end of the bytes"
            ),
            Self::HashWalkSpent { end } => write!(
                f,
                "the bytes to resume claim too many blocks that don't read: a block written at {end} would not verify"
            ),
            Self::Continued { next } => write!(
                f,
                "the piece goes on in the next piece ({:02x?}): only the last piece of a chain is written on",
                next.0
            ),
            Self::Gap { from, to } => write!(
                f,
                "pieces {from}..{to} of the chain are missing and not named as removed on purpose"
            ),
            Self::HeadLost { seq: Some(seq) } => write!(
                f,
                "piece {seq}, after removed pieces, was cut off inside its head"
            ),
            Self::KeysUndefined { track } => write!(
                f,
                "the piece's carry of keyed track {track} was lost and the call's definitions don't hold the track: pass those the resume read"
            ),
            Self::KeysUnknown { track } => write!(
                f,
                "the piece's carry of keyed track {track} was lost and its keys lie only in the pieces before: resume it through its chain"
            ),
            Self::HeadLost { seq: None } => write!(
                f,
                "the first piece kept in the tape was cut off inside its head"
            ),
            Self::Damaged { offset } => write!(
                f,
                "the end to cut holds a damaged record at {offset}, not a torn end: nothing is cut"
            ),
        }
    }
}

impl core::error::Error for ResumeError {}

impl<E: core::error::Error + 'static> core::error::Error for WriteError<E> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            // the wrapped error's text is in this one's: its source comes next
            Self::Sink { error, .. } => error.source(),
            Self::Read(e) => e.source(),
            Self::Describe(e) => e.source(),
            Self::Input(e) => e.source(),
            Self::Resume(e) => e.source(),
            _ => None,
        }
    }
}
