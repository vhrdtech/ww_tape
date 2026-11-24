//! Where the writing thread placed a chunk a [ChunkSender](crate::ChunkSender) sent: a receipt per chunk, for
//! readers in the writer's process that read the chunk back from its piece.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use ww_tape::{ItemNum, TrackNum, UnitHash};

use crate::reply::{ReplyRx, ReplyTx, reply};
use crate::wait::block_on;

/// Where a chunk was placed: in which piece, at which bytes, under which unit hash.
///
/// The block is in the piece's sink once the receipt is out, so a read of `len` bytes at [Self::tape_offset] of the
/// piece's tape (its file, or the one file of [PieceTapes::One](ww_tape::PieceTapes::One)) gives it, before any
/// sync.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Placement {
    /// The chunk's track.
    pub track: TrackNum,
    /// Its first item, and how many it holds.
    pub first_item: ItemNum,
    pub count: u32,
    /// The sequence number of the piece it is in.
    pub piece: u64,
    /// Where that piece starts in its tape: 0 for a piece of its own file.
    pub piece_start: u64,
    /// Where the chunk block starts, from the start of the piece, and its whole length.
    pub offset: u64,
    pub len: u32,
    /// The chunk's unit hash as the writer states it in the piece's index ([ww_tape::ChunkPlaced::unit]).
    pub unit: UnitHash,
}

impl Placement {
    /// Where the chunk block starts in its tape.
    pub fn tape_offset(&self) -> u64 {
        self.piece_start.saturating_add(self.offset)
    }
}

/// Why a chunk sent has no [Placement].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Unplaced {
    /// The writer refused it: the sender's next call returns why ([MuxError::Refused](crate::MuxError::Refused)).
    Refused,
    /// The mux failed or stopped before it was placed.
    Lost,
}

impl core::fmt::Display for Unplaced {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Refused => f.write_str("the writer refused the chunk"),
            Self::Lost => f.write_str("the mux ended before the chunk was placed"),
        }
    }
}

impl core::error::Error for Unplaced {}

/// The writing thread's side of a receipt.
pub(crate) type PlacedTx = ReplyTx<Result<Placement, Unplaced>>;

/// The receipt of one chunk sent: its [Placement] once the writing thread placed it.
///
/// A future, woken by the writing thread; [Self::wait] blocks a thread on it, [Self::try_take] never waits. One is
/// held per chunk in the queue, so the queue's bound ([MuxOptions::queue_bytes](crate::MuxOptions::queue_bytes))
/// bounds those not yet answered.
pub struct Placing {
    rx: ReplyRx<Result<Placement, Unplaced>>,
}

impl Placing {
    /// A receipt and the writing thread's side of it.
    pub(crate) fn new() -> (PlacedTx, Self) {
        let (tx, rx) = reply();
        (tx, Self { rx })
    }

    /// Wait for the placement.
    pub fn wait(self) -> Result<Placement, Unplaced> {
        block_on(self)
    }

    /// The placement when the writing thread answered, else None; never waits.
    pub fn try_take(&mut self) -> Option<Result<Placement, Unplaced>> {
        self.rx.try_take().map(|r| r.unwrap_or(Err(Unplaced::Lost)))
    }
}

impl Future for Placing {
    type Output = Result<Placement, Unplaced>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.rx)
            .poll(cx)
            .map(|r| r.unwrap_or(Err(Unplaced::Lost)))
    }
}
