//! What a mux call or a handle's call can fail with.

use std::sync::Arc;

use exact_units::Bytes;

use ww_tape::chunk::ChunkHead;
use ww_tape::{
    BlockError, BlockKind, ColumnSpec, DescribeError, ItemNum, SampleError, SeriesError, TrackNum,
    WriteError,
};
use ww_tape_summary::BuildError;

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
    /// An item a typed handle refuses: it would be item `item` of its track.
    Item { item: ItemNum, why: ItemError },
    /// A typed handle could not encode its open chunk or its summaries.
    Encode(EncodeError),
    /// A grid a grid handle refuses: it starts after the next time step, or too close to the last one.
    Grid(SeriesError),
    /// A box of cells that isn't one range per fixed axis of the grid, each non-empty, within the axis' extent and
    /// starting and ending at a tile's edge.
    TileBox,
    /// One time step of a tile takes `bytes` bytes, more than a chunk holds: `max`, the smaller of the handle's
    /// chunk bytes and what fits a block.
    StepTooLarge { bytes: Bytes<u64>, max: Bytes<u64> },
}

/// Why a typed handle refuses an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ItemError {
    /// It starts before the item before it.
    Order,
    /// It ends before it starts, or is a span on a track of instants.
    Span,
    /// It has flags on a track whose items have none.
    Flags,
    /// It has `values` relation values where the track stores `stored`.
    Relations { values: usize, stored: usize },
    /// Its value of monotone relation `relation` is below the item's before it.
    RelationDecreases { relation: u16 },
    /// Its payload is longer than [ITEM_BYTES_MAX](crate::ITEM_BYTES_MAX).
    TooLong { len: usize },
    /// Values that are not whole time steps of `scalars` scalars each.
    PartialStep { scalars: usize },
}

impl core::fmt::Display for ItemError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Order => f.write_str("it starts before the item before it"),
            Self::Span => {
                f.write_str("it ends before it starts, or is a span on a track of instants")
            }
            Self::Flags => f.write_str("it has flags on a track whose items have none"),
            Self::Relations { values, stored } => {
                write!(
                    f,
                    "{values} relation values where the track stores {stored}"
                )
            }
            Self::RelationDecreases { relation } => {
                write!(f, "its value of monotone relation {relation} decreases")
            }
            Self::TooLong { len } => write!(f, "a payload of {len} bytes"),
            Self::PartialStep { scalars } => {
                write!(
                    f,
                    "values that are not whole time steps of {scalars} scalars"
                )
            }
        }
    }
}

/// What a typed handle failed to encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EncodeError {
    /// The times, or a relation's values, of an events chunk.
    Times(ww_tape::times::EncodeError),
    /// The payloads of an events chunk.
    Payloads(ww_tape::payloads::EncodeError),
    /// The samples of a series chunk.
    Samples(SampleError),
    /// The track's summaries.
    Summary(BuildError),
    /// The flag runs of a series chunk.
    FlagRuns(ww_tape::FlagRunsError),
    /// A grid tile's space overviews.
    Overview(ww_tape_summary::overview::OverviewError),
}

impl core::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Times(e) => write!(f, "encoding times: {e}"),
            Self::Payloads(e) => write!(f, "encoding payloads: {e}"),
            Self::Samples(e) => write!(f, "encoding samples: {e}"),
            Self::Summary(e) => write!(f, "building summaries: {e}"),
            Self::FlagRuns(e) => write!(f, "encoding flag runs: {e}"),
            Self::Overview(e) => write!(f, "building overviews: {e}"),
        }
    }
}

impl core::error::Error for EncodeError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Times(e) => Some(e),
            Self::Payloads(e) => Some(e),
            Self::Samples(e) => Some(e),
            Self::Summary(e) => Some(e),
            Self::FlagRuns(e) => Some(e),
            Self::Overview(e) => Some(e),
        }
    }
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
            Self::Item { item, why } => write!(f, "item {} refused: {why}", item.get()),
            Self::Encode(e) => write!(f, "{e}"),
            Self::Grid(e) => write!(f, "a grid refused: {e}"),
            Self::TileBox => f.write_str(
                "a box of cells that isn't one non-empty, tile-aligned range per fixed axis within its extent",
            ),
            Self::StepTooLarge { bytes, max } => write!(
                f,
                "one time step of a tile takes {} bytes, more than a chunk holds ({})",
                bytes.get(),
                max.get()
            ),
        }
    }
}

impl core::error::Error for CallError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Block(e) => Some(e),
            Self::Encode(e) => Some(e),
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
