//! What a mux call or a handle's call can fail with.

use std::sync::Arc;

use ww_tape::chunk::ChunkHead;
use ww_tape::{BlockError, BlockKind, ColumnSpec, DescribeError, TrackNum, WriteError};

/// A unit a handle sent, handed back when the queue had no room and the handle's [Pressure](crate::Pressure) is
/// [Fail](crate::Pressure::Fail).
#[derive(Debug)]
#[non_exhaustive]
pub enum Unit {
    /// A chunk of a [ChunkSender](crate::ChunkSender).
    Chunk {
        head: Box<ChunkHead>,
        columns: Vec<(ColumnSpec, Vec<u8>)>,
    },
    /// A block of a [RecordsHandle](crate::RecordsHandle) or [RelayHandle](crate::RelayHandle).
    Block(Vec<u8>),
    /// A put of a [StateHandle](crate::StateHandle).
    Put { ticks: i64, item: Vec<u8> },
}

/// What a handle refuses at the call, before anything is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CallError {
    /// A chunk of another track than the sender's.
    WrongTrack { sender: TrackNum, chunk: TrackNum },
    /// A track the mux holds no definition of, or of another kind than the handle needs: a [ChunkSender] needs an
    /// events or series track and a chunk of its kind, a [StateHandle] a state track.
    ///
    /// [ChunkSender]: crate::ChunkSender
    /// [StateHandle]: crate::StateHandle
    Kind { track: TrackNum },
    /// A block of a kind the handle doesn't send: a [RecordsHandle](crate::RecordsHandle) sends records blocks.
    BlockKind(BlockKind),
    /// A column whose length differs from its bytes.
    ColumnLength,
    /// A block that doesn't read.
    Block(BlockError),
}

impl core::fmt::Display for CallError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::WrongTrack { sender, chunk } => write!(
                f,
                "a chunk of track {} sent through the sender of track {}",
                chunk.get(),
                sender.get()
            ),
            Self::Kind { track } => write!(
                f,
                "track {} isn't held or isn't of the kind the handle needs",
                track.get()
            ),
            Self::BlockKind(kind) => write!(f, "a {kind:?} block where a records block goes"),
            Self::ColumnLength => f.write_str("a column's length differs from its bytes"),
            Self::Block(e) => write!(f, "the block doesn't read: {e}"),
        }
    }
}

impl core::error::Error for CallError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Block(e) => Some(e),
            _ => None,
        }
    }
}

/// Why a call on a [TapeMux](crate::TapeMux) or one of its handles failed; `E` is the error of the tapes.
#[derive(Debug)]
#[non_exhaustive]
pub enum MuxError<E> {
    /// The mux is closed or dropped: it takes nothing more.
    Closed,
    /// A write or sync failed, or a cut could not start the next piece: the chain takes no more blocks. Every
    /// handle's next call, flush and the close return this. Shared, as every handle gets it.
    Failed(Arc<WriteError<E>>),
    /// The writer refused something it was sent: a unit of this handle (returned by the handle's next call), or a
    /// control call's work (a cut refused). Nothing of it was written; the mux writes on.
    Refused(WriteError<E>),
    /// The handle refused the call; nothing was sent.
    Call(CallError),
    /// A definition the mux's definitions refuse.
    Describe(DescribeError),
    /// No room in the queue, and the handle's pressure is [Fail](crate::Pressure::Fail): the unit back.
    Full(Unit),
    /// The chain could not be started or resumed.
    Start(WriteError<E>),
    /// The writing thread could not be started.
    Spawn(std::io::Error),
}

impl<E> From<CallError> for MuxError<E> {
    fn from(e: CallError) -> Self {
        Self::Call(e)
    }
}

impl<E> From<DescribeError> for MuxError<E> {
    fn from(e: DescribeError) -> Self {
        Self::Describe(e)
    }
}

impl<E: core::fmt::Display + core::fmt::Debug> core::fmt::Display for MuxError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Closed => f.write_str("the mux is closed"),
            Self::Failed(e) => write!(f, "the mux failed: {e}"),
            Self::Refused(e) => write!(f, "the writer refused a unit: {e}"),
            Self::Call(e) => write!(f, "{e}"),
            Self::Describe(e) => write!(f, "a definition refused: {e}"),
            Self::Full(_) => f.write_str("the mux's queue is full"),
            Self::Start(e) => write!(f, "the chain could not be started: {e}"),
            Self::Spawn(e) => write!(f, "the writing thread could not be started: {e}"),
        }
    }
}

impl<E: core::error::Error + 'static> core::error::Error for MuxError<E> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Failed(e) => Some(e.as_ref()),
            Self::Refused(e) => Some(e),
            Self::Call(e) => Some(e),
            Self::Describe(e) => Some(e),
            Self::Start(e) => Some(e),
            Self::Spawn(e) => Some(e),
            Self::Closed | Self::Full(_) => None,
        }
    }
}
