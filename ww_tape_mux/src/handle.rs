//! The handles producers send through: a chunk sender, records, relayed blocks and state puts.

use std::collections::VecDeque;
use std::sync::Arc;

use ww_tape::chunk::ChunkHead;
use ww_tape::{Block, BlockKind, ColumnSpec, PieceStore, TrackFacts, TrackNum};

use crate::error::{CallError, MuxError};
use crate::queue::Pushed;
use crate::reply::reply;
use crate::shared::{BlockHow, RelayMode, Shared, Slot, Work};
use crate::wait::block_on;

/// What a handle does when the queue has no room for a unit.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum Pressure {
    /// Block until the queue has room, behind the handles that waited first. Nothing is lost.
    #[default]
    Wait,
    /// Return [MuxError::Full] with the unit, for a producer that decides itself.
    Fail,
    /// Drop the unit and count it ([crate::MuxStats::drops]). A chunk sender's next chunk then starts past the
    /// dropped items, and a reader sees the gap in the item numbers as items lost.
    Drop,
}

/// What became of a unit sent.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Sent {
    /// In the queue: placed in order.
    Queued,
    /// Dropped for want of room ([Pressure::Drop]).
    Dropped,
}

/// What every handle holds: the mux's share, its own, its pressure and its track.
pub(crate) struct Sender<T: PieceStore> {
    pub(crate) shared: Arc<Shared<T>>,
    pub(crate) slot: Arc<Slot<T::Error>>,
    pub(crate) pressure: Pressure,
    track: Option<TrackNum>,
}

impl<T: PieceStore> Sender<T> {
    pub(crate) fn new(shared: Arc<Shared<T>>, track: Option<TrackNum>) -> Self {
        Self {
            shared,
            slot: Arc::new(Slot::new()),
            pressure: Pressure::default(),
            track,
        }
    }

    /// The writer's refusal of a unit sent before, or the mux's failure.
    pub(crate) fn ready(&self) -> Result<(), MuxError<T::Error>> {
        if let Some(e) = self.slot.take_refused() {
            return Err(MuxError::Refused(e));
        }
        match self.shared.failure.get() {
            Some(f) => Err(MuxError::Failed(f.clone())),
            None => Ok(()),
        }
    }

    pub(crate) fn send(&self, work: Work<T>, bytes: u64) -> Result<Sent, MuxError<T::Error>> {
        block_on(self.send_async(work, bytes))
    }

    pub(crate) async fn send_async(
        &self,
        work: Work<T>,
        bytes: u64,
    ) -> Result<Sent, MuxError<T::Error>> {
        let mut slot = Some(work);
        self.send_slot(&mut slot, bytes).await
    }

    /// Send the unit in `slot`, of `bytes`, as the handle's pressure says. Dropped while it waits for room, the unit
    /// stays in `slot`.
    pub(crate) async fn send_slot(
        &self,
        slot: &mut Option<Work<T>>,
        bytes: u64,
    ) -> Result<Sent, MuxError<T::Error>> {
        self.ready()?;
        let pushed = match self.pressure {
            Pressure::Wait => self.shared.queue.push_wait(slot, bytes).await,
            Pressure::Fail | Pressure::Drop => self.shared.queue.try_push(slot, bytes),
        };
        match pushed {
            Pushed::Queued => Ok(Sent::Queued),
            Pushed::Full if self.pressure == Pressure::Drop => {
                self.shared.counters.drop_on(self.track);
                self.recycle(slot.take());
                Ok(Sent::Dropped)
            }
            Pushed::Full => match slot.take().and_then(Work::into_unit) {
                Some(unit) => Err(MuxError::Full(unit)),
                None => Err(MuxError::Closed),
            },
            Pushed::Closed => {
                self.recycle(slot.take());
                Err(self.shared.ended())
            }
        }
    }

    /// Keep the buffers of a unit not sent.
    fn recycle(&self, work: Option<Work<T>>) {
        match work {
            Some(Work::Chunk {
                columns, overviews, ..
            }) => columns
                .into_iter()
                .chain(overviews.into_iter().flat_map(|o| o.columns))
                .for_each(|(_, b)| self.slot.give_back(b)),
            Some(Work::Block { block, .. }) => self.slot.give_back(block),
            Some(Work::Put { item, .. }) => self.slot.give_back(item),
            _ => {}
        }
    }

    /// Wait until everything sent before is placed; with `sync`, durable. Then a refusal of one of them.
    pub(crate) fn flush(&self, sync: bool) -> Result<(), MuxError<T::Error>> {
        block_on(self.flush_async(sync))
    }

    pub(crate) async fn flush_async(&self, sync: bool) -> Result<(), MuxError<T::Error>> {
        let (tx, rx) = reply();
        self.shared
            .control_async(Work::Flush { sync, reply: tx }, rx)
            .await??;
        self.ready()
    }
}

/// Units a typed handle completed and has yet to queue, oldest first.
///
/// An async call dropped while it waits for room leaves its units here, and the handle's next call sends them
/// before its own, so no item is lost and the track's chunks stay in order.
pub(crate) struct Backlog<T: PieceStore>(VecDeque<(Option<Work<T>>, u64)>);

impl<T: PieceStore> Backlog<T> {
    pub(crate) fn new() -> Self {
        Self(VecDeque::new())
    }

    pub(crate) fn push(&mut self, (work, bytes): (Work<T>, u64)) {
        self.0.push_back((Some(work), bytes));
    }

    /// Send every unit through `s`: what became of the last one. A unit the call ends before stays.
    pub(crate) fn send(&mut self, s: &Sender<T>) -> Result<Option<Sent>, MuxError<T::Error>> {
        if self.0.is_empty() {
            return Ok(None);
        }
        block_on(self.send_async(s))
    }

    pub(crate) async fn send_async(
        &mut self,
        s: &Sender<T>,
    ) -> Result<Option<Sent>, MuxError<T::Error>> {
        let mut sent = None;
        while let Some((slot, bytes)) = self.0.front_mut() {
            let r = s.send_slot(slot, *bytes).await;
            // a unit refused by the handle's earlier refusal stays for the next call; any other left its slot
            if slot.is_none() {
                self.0.pop_front();
            }
            sent = Some(r?);
        }
        Ok(sent)
    }
}

/// The handle API every kind shares; `$s` names the field holding its [Sender].
macro_rules! common {
    ($name:ident) => {
        common!($name, s);
    };
    ($name:ident, $s:ident) => {
        impl<T: PieceStore> $name<T> {
            /// What the handle does when the queue has no room.
            #[must_use]
            pub fn pressure(mut self, pressure: Pressure) -> Self {
                self.$s.pressure = pressure;
                self
            }

            /// Return once everything this handle sent before is placed; a refusal of one of them.
            pub fn flush(&self) -> Result<(), MuxError<T::Error>> {
                self.$s.flush(false)
            }

            /// Return once everything this handle sent before is durable; a refusal of one of them.
            pub fn sync(&self) -> Result<(), MuxError<T::Error>> {
                self.$s.flush(true)
            }

            /// [Self::flush] as a future, woken by the writing thread. Dropped before, the flush is done all the
            /// same.
            pub async fn flush_async(&self) -> Result<(), MuxError<T::Error>> {
                self.$s.flush_async(false).await
            }

            /// [Self::sync] as a future; see [Self::flush_async].
            pub async fn sync_async(&self) -> Result<(), MuxError<T::Error>> {
                self.$s.flush_async(true).await
            }
        }
    };
}
pub(crate) use common;

/// The handle API of the handles that send buffers they fill.
macro_rules! buffered {
    ($name:ident) => {
        impl<T: PieceStore> $name<T> {
            /// An empty buffer for the next unit: one the mux gave back once placed, else a new one.
            pub fn buffer(&self) -> Vec<u8> {
                self.s.slot.buffer()
            }
        }
    };
}

/// Sends chunks of one events or series track, encoded by the caller.
///
/// The caller picks the payload codec of an events chunk as it encodes it:
/// [EncodedPayloads::from_parts](ww_tape::payloads::EncodedPayloads::from_parts) or
/// [encode_with](ww_tape::payloads::EncodedPayloads::encode_with) with a
/// [PayloadCodec](ww_tape::payloads::PayloadCodec), into a [Self::buffer].
///
/// The caller numbers the items: a chunk's `first_item` follows the last one sent. Many handles may serve other
/// tracks; this track has only this one.
pub struct ChunkSender<T: PieceStore> {
    s: Sender<T>,
    track: TrackNum,
    kind: BlockKind,
}

common!(ChunkSender);
buffered!(ChunkSender);

impl<T: PieceStore> ChunkSender<T> {
    pub(crate) fn new(shared: Arc<Shared<T>>, track: TrackNum, facts: &TrackFacts) -> Self {
        let kind = match facts.period {
            Some(_) => BlockKind::Series,
            None => BlockKind::Events,
        };
        Self {
            s: Sender::new(shared, Some(track)),
            track,
            kind,
        }
    }

    /// The track this sender writes.
    pub fn track(&self) -> TrackNum {
        self.track
    }

    /// Send a chunk: its head and its columns, each spec's length that of its bytes. A chunk of another track or
    /// kind, or a column whose length is off, is refused before it is sent ([CallError]).
    pub fn send(
        &self,
        head: ChunkHead,
        columns: Vec<(ColumnSpec, Vec<u8>)>,
    ) -> Result<Sent, MuxError<T::Error>> {
        let (work, bytes) = self.unit(Box::new(head), columns)?;
        self.s.send(work, bytes)
    }

    /// [Self::send] as a future, waiting for room by waker. Dropped while it waits, the chunk is not sent.
    pub async fn send_async(
        &self,
        head: ChunkHead,
        columns: Vec<(ColumnSpec, Vec<u8>)>,
    ) -> Result<Sent, MuxError<T::Error>> {
        let (work, bytes) = self.unit(Box::new(head), columns)?;
        self.s.send_async(work, bytes).await
    }

    /// The unit of a chunk checked as [Self::send] says, its payload hashed.
    fn unit(
        &self,
        head: Box<ChunkHead>,
        columns: Vec<(ColumnSpec, Vec<u8>)>,
    ) -> Result<(Work<T>, u64), MuxError<T::Error>> {
        if head.track_number != self.track {
            return Err(CallError::WrongTrack {
                sender: self.track,
                chunk: head.track_number,
            }
            .into());
        }
        if head.kind != self.kind {
            return Err(CallError::Kind { track: self.track }.into());
        }
        if columns.iter().any(|(c, b)| c.len != b.len()) {
            return Err(CallError::ColumnLength.into());
        }
        let specs: Vec<ColumnSpec> = columns.iter().map(|(c, _)| *c).collect();
        let bytes = ww_tape::chunk::chunk_len(&head, &specs, 0);
        // the payload hashed here, on the producer's thread, not on the writing thread
        let hash = self.s.shared.hash_payloads.then(|| {
            ww_tape::piece::columns_payload_hash(columns.iter().map(|(_, b)| b.as_slice()))
        });
        let work = Work::Chunk {
            from: self.s.slot.clone(),
            head,
            columns,
            hash,
            summary: None,
            overviews: Vec::new(),
        };
        Ok((work, u64::try_from(bytes).unwrap_or(u64::MAX)))
    }
}

/// Sends records blocks built with [ww_tape::BlockWriter]: log lines, records of several tracks.
///
/// The mux numbers each block as the writer's next, whatever id it was built with. Any number of these handles may
/// write the same tracks.
pub struct RecordsHandle<T: PieceStore> {
    s: Sender<T>,
}

common!(RecordsHandle);
buffered!(RecordsHandle);

impl<T: PieceStore> RecordsHandle<T> {
    pub(crate) fn new(shared: Arc<Shared<T>>) -> Self {
        Self {
            s: Sender::new(shared, None),
        }
    }

    /// Send a records block. One that doesn't read, or of another kind, is refused before it is sent.
    pub fn send(&self, block: Vec<u8>) -> Result<Sent, MuxError<T::Error>> {
        let (work, bytes) = self.unit(block)?;
        self.s.send(work, bytes)
    }

    /// [Self::send] as a future, waiting for room by waker. Dropped while it waits, the block is not sent.
    pub async fn send_async(&self, block: Vec<u8>) -> Result<Sent, MuxError<T::Error>> {
        let (work, bytes) = self.unit(block)?;
        self.s.send_async(work, bytes).await
    }

    fn unit(&self, block: Vec<u8>) -> Result<(Work<T>, u64), MuxError<T::Error>> {
        let kind = Block::read(&block).map_err(CallError::Block)?.kind;
        if kind != BlockKind::Records {
            return Err(CallError::BlockKind(kind).into());
        }
        let bytes = block_bytes(&block);
        let work = Work::Block {
            from: self.s.slot.clone(),
            how: BlockHow::Records,
            block,
        };
        Ok((work, bytes))
    }
}

/// Passes on another producer's blocks: a link tap's, or those that came over a socket from another process.
pub struct RelayHandle<T: PieceStore> {
    s: Sender<T>,
    mode: RelayMode,
}

common!(RelayHandle);
buffered!(RelayHandle);

impl<T: PieceStore> RelayHandle<T> {
    pub(crate) fn new(shared: Arc<Shared<T>>, mode: RelayMode) -> Self {
        Self {
            s: Sender::new(shared, None),
            mode,
        }
    }

    /// Send a block as its producer made it. One that doesn't read is refused before it is sent.
    pub fn send(&self, block: Vec<u8>) -> Result<Sent, MuxError<T::Error>> {
        let (work, bytes) = self.unit(block)?;
        self.s.send(work, bytes)
    }

    /// [Self::send] as a future, waiting for room by waker. Dropped while it waits, the block is not sent.
    pub async fn send_async(&self, block: Vec<u8>) -> Result<Sent, MuxError<T::Error>> {
        let (work, bytes) = self.unit(block)?;
        self.s.send_async(work, bytes).await
    }

    fn unit(&self, block: Vec<u8>) -> Result<(Work<T>, u64), MuxError<T::Error>> {
        Block::read(&block).map_err(CallError::Block)?;
        let bytes = block_bytes(&block);
        let work = Work::Block {
            from: self.s.slot.clone(),
            how: BlockHow::Relay(self.mode),
            block,
        };
        Ok((work, bytes))
    }
}

/// Puts the items of one state track, on change: the writer puts them again where readers need them.
///
/// Any number of these handles may put on the same track.
pub struct StateHandle<T: PieceStore> {
    s: Sender<T>,
    track: TrackNum,
}

common!(StateHandle);
buffered!(StateHandle);

impl<T: PieceStore> StateHandle<T> {
    pub(crate) fn new(shared: Arc<Shared<T>>, track: TrackNum) -> Self {
        Self {
            s: Sender::new(shared, Some(track)),
            track,
        }
    }

    /// The track this handle puts on.
    pub fn track(&self) -> TrackNum {
        self.track
    }

    /// Put `item` at `ticks` of the track's clock.
    pub fn put(&self, ticks: i64, item: &[u8]) -> Result<Sent, MuxError<T::Error>> {
        let (work, bytes) = self.unit(ticks, item);
        self.s.send(work, bytes)
    }

    /// [Self::put] as a future, waiting for room by waker. Dropped while it waits, the item is not put.
    pub async fn put_async(&self, ticks: i64, item: &[u8]) -> Result<Sent, MuxError<T::Error>> {
        let (work, bytes) = self.unit(ticks, item);
        self.s.send_async(work, bytes).await
    }

    fn unit(&self, ticks: i64, item: &[u8]) -> (Work<T>, u64) {
        let mut buf = self.s.slot.buffer();
        buf.extend_from_slice(item);
        let bytes = block_bytes(&buf).saturating_add(PUT_OVERHEAD_BYTES);
        let work = Work::Put {
            from: self.s.slot.clone(),
            track: self.track,
            ticks,
            item: buf,
        };
        (work, bytes)
    }
}

/// What a put takes of the queue's budget besides its item: its record's head and its share of a block.
const PUT_OVERHEAD_BYTES: u64 = 64;

/// What `block` takes of the queue's budget.
fn block_bytes(block: &[u8]) -> u64 {
    u64::try_from(block.len()).unwrap_or(u64::MAX)
}
