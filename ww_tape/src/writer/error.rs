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
    /// The definitions the next piece's head carries, with the layout and the producer, would take `needed` state
    /// entries, more than the index policy's `max` ([IndexPolicy::state_entries_max](crate::index::IndexPolicy)). A
    /// write that would bring a new track there is refused, nothing written. A cut is never refused for them: it
    /// leaves out what doesn't fit ([TrackStart::not_carried](crate::piece::TrackStart::not_carried)).
    CarriedEntries { needed: usize, max: usize },
    /// A caller's block of `kind` with header `flags` its kind doesn't take: reserved bits, or a flag only the
    /// writer sets. A records block may carry [FLAG_BODY_CODED](crate::FLAG_BODY_CODED), other kinds none.
    Flags { kind: BlockKind, flags: u16 },
    /// A close's summary body of `len` bytes, more than [CLOSE_SUMMARY_MAX](crate::piece::CLOSE_SUMMARY_MAX) (`max`).
    SummaryTooLong { len: usize, max: usize },
    /// Removed pieces ([TapeWriter::record_removed](super::TapeWriter::record_removed)) the piece's close can't name:
    /// see [RemovedRefusal].
    Removed(RemovedRefusal),
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
                "the next piece's head would carry definitions taking {needed} state entries, more than {max}: start a new chain"
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
