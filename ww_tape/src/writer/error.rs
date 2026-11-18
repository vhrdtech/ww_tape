//! [WriteError]: why a [TapeWriter] stopped, and [CutError].

use std::io;
use std::path::PathBuf;

use super::{TapeWriter, caller_flags};
use crate::piece::FinishState;
use crate::reader::ReadError;
use crate::sink::TapeSink;
use crate::{BlockKind, DescribeError, IndexError, Layout, SessionTag};

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
    /// The piece to resume describes another layout than the one `given` in the [WriterOptions](super::WriterOptions), or a layout set
    /// after the first block.
    Layout { described: Layout, given: Layout },
    /// The piece to resume names another producer than the one `given` in the [WriterOptions](super::WriterOptions).
    Producer { described: String, given: String },
    /// A caller's block the writer doesn't take: a kind it makes itself or doesn't know (the reserved kind 2), a
    /// block of another format version, or a body of a codec it doesn't know.
    Unknown { version: u8, kind: BlockKind },
    /// A block or chunk that would bring the tracks with data in the piece past `max`
    /// ([TRACKS_PER_PIECE_MAX](crate::TRACKS_PER_PIECE_MAX)), so its finish would not fit a block. Nothing is
    /// written; cut the piece first.
    TooManyTracks { max: usize },
    /// The next piece's head would carry more than `max` tracks ([HEAD_TRACKS_MAX](crate::HEAD_TRACKS_MAX)): each
    /// track the chain's pieces carried or gave data. Nothing is written; a new chain takes the track.
    TooManyCarried { max: usize },
    /// A caller's block of `kind` with header `flags` its kind doesn't take: reserved bits, or a flag only the
    /// writer sets. A records block may carry [FLAG_BODY_CODED](crate::FLAG_BODY_CODED), other kinds none.
    Flags { kind: BlockKind, flags: u16 },
    /// A resume would cut off a good block of the piece at `offset`, behind bytes the reader didn't take: more than a
    /// torn end. The bytes stay as they are.
    GoodBlockAfterCut { offset: u64 },
    /// A resume looked for good blocks past where the reader stopped and ran out of its look ([RESUME_LOOK_MAX](super::RESUME_LOOK_MAX)) or
    /// of the places it reads whole ([RESUME_PROBES_MAX](super::RESUME_PROBES_MAX)) at `offset`, before the end of the bytes: it can't tell the
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

impl<S: TapeSink> core::error::Error for CutError<S> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
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
            WriteError::TooManyCarried { max } => write!(
                f,
                "more than {max} tracks carried into the next piece's head: start a new chain"
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

impl core::error::Error for WriteError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            WriteError::Io { error, .. } => Some(error),
            WriteError::Read(e) => Some(e),
            WriteError::Describe(e) => Some(e),
            _ => None,
        }
    }
}
