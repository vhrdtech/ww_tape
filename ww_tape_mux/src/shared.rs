//! What the writing thread, the control and the handles share: the queue, the failure, the counters; and what goes
//! through the queue.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Instant;

use ww_tape::chunk::ChunkHead;
use ww_tape::{
    ChainPiece, CloseReason, ColumnSpec, Definitions, DescribeError, Tapes, TrackFacts, TrackNum,
    WriteError,
};

use crate::MuxStats;
use crate::error::{MuxError, Unit};
use crate::open::Seal;
use crate::progress::Watch;
use crate::queue::{Queue, lock};
use crate::reply::ReplyTx;
use crate::summaries::ChunkSummary;
use ww_tape::ClockNum;

/// Buffers a handle keeps for reuse, at most: those past it are freed.
pub const RETURNED_BUFFERS_MAX: usize = 64;

/// How a block of a [RelayHandle](crate::RelayHandle) is placed.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum RelayMode {
    /// As [PieceChain::relay_block](ww_tape::PieceChain::relay_block): a block of puts has its definitions held by
    /// the mux; other blocks are written in their producer's session.
    #[default]
    Relay,
    /// As [PieceChain::adopt_block](ww_tape::PieceChain::adopt_block): renumbered into the writer's session, a
    /// jump in the producer's seq kept as a jump.
    Adopt,
}

/// How the mux places a block.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum BlockHow {
    /// A records block renumbered as the writer's next block, then written.
    Records,
    Relay(RelayMode),
}

/// The reply to a control call.
pub(crate) type Done<E> = ReplyTx<Result<(), MuxError<E>>>;

/// The reply to a close: every piece of the chain.
pub(crate) type ClosedTx<T> =
    ReplyTx<Result<Vec<ChainPiece<<T as Tapes>::Key>>, MuxError<<T as Tapes>::Error>>>;

/// What goes through the queue.
pub(crate) enum Work<T: Tapes> {
    Chunk {
        from: Arc<Slot<T::Error>>,
        head: Box<ChunkHead>,
        columns: Vec<(ColumnSpec, Vec<u8>)>,
        /// The payload's hash, computed by the handle ([ww_tape::piece::columns_payload_hash])
        hash: [u8; 32],
        /// The leaves of the track's summaries the handle computed
        summary: Option<Box<ChunkSummary>>,
    },
    Block {
        from: Arc<Slot<T::Error>>,
        how: BlockHow,
        block: Vec<u8>,
    },
    Put {
        from: Arc<Slot<T::Error>>,
        track: TrackNum,
        ticks: i64,
        item: Vec<u8>,
    },
    /// A track, encoded ([crate::define::track]).
    DefineTrack {
        item: Vec<u8>,
        reply: ReplyTx<Result<TrackNum, DescribeError>>,
    },
    /// A clock and maps from it, encoded ([crate::define::clock]).
    DefineClock {
        items: Vec<Vec<u8>>,
        reply: ReplyTx<Result<ClockNum, DescribeError>>,
    },
    /// Maps, bundles and schemas, encoded, held as one batch.
    DefineAll {
        items: Vec<Vec<u8>>,
        reply: ReplyTx<Result<(), DescribeError>>,
    },
    Facts {
        track: TrackNum,
        reply: ReplyTx<Option<TrackFacts>>,
    },
    /// A handle of a track without summaries is made: a tree a resume rebuilt of the track is dropped.
    Plain {
        track: TrackNum,
        reply: ReplyTx<()>,
    },
    /// A track's definition, encoded.
    TrackDef {
        track: TrackNum,
        reply: ReplyTx<Option<Vec<u8>>>,
    },
    Definitions {
        reply: ReplyTx<Definitions>,
    },
    Cut {
        reply: Done<T::Error>,
    },
    /// Everything before it placed; with `sync`, durable.
    Flush {
        sync: bool,
        reply: Done<T::Error>,
    },
    Close {
        reason: CloseReason,
        reply: ClosedTx<T>,
    },
    /// The mux dropped: place what came before, leave the piece unclosed.
    Stop,
}

impl<T: Tapes> Work<T> {
    /// The unit a handle sent, for [MuxError::Full]; a control's work gives none.
    pub(crate) fn into_unit(self) -> Option<Unit> {
        match self {
            Self::Chunk { head, columns, .. } => Some(Unit::Chunk { head, columns }),
            Self::Block { block, .. } => Some(Unit::Block(block)),
            Self::Put { ticks, item, .. } => Some(Unit::Put { ticks, item }),
            _ => None,
        }
    }
}

/// A handle's own share: the writer's refusal of one of its units, and buffers given back for reuse.
pub(crate) struct Slot<E> {
    refused: Mutex<Option<WriteError<E>>>,
    buffers: Mutex<Vec<Vec<u8>>>,
}

impl<E> Slot<E> {
    pub(crate) fn new() -> Self {
        Self {
            refused: Mutex::new(None),
            buffers: Mutex::new(Vec::new()),
        }
    }

    /// Keep the writer's refusal of a unit for the handle's next call; the first one, when several wait.
    pub(crate) fn refuse(&self, e: WriteError<E>) {
        lock(&self.refused).get_or_insert(e);
    }

    pub(crate) fn take_refused(&self) -> Option<WriteError<E>> {
        lock(&self.refused).take()
    }

    /// Keep `buf` for the handle to reuse, emptied.
    pub(crate) fn give_back(&self, mut buf: Vec<u8>) {
        let mut b = lock(&self.buffers);
        if b.len() < RETURNED_BUFFERS_MAX && buf.capacity() > 0 {
            buf.clear();
            b.push(buf);
        }
    }

    /// A buffer given back, or a new one.
    pub(crate) fn buffer(&self) -> Vec<u8> {
        lock(&self.buffers).pop().unwrap_or_default()
    }
}

/// Counters for [MuxStats].
#[derive(Default)]
pub(crate) struct Counters {
    pub drops: AtomicU64,
    pub drops_by_track: Mutex<BTreeMap<TrackNum, u64>>,
    pub placed_units: AtomicU64,
    pub placed_bytes: AtomicU64,
    pub refused: AtomicU64,
    pub lost: AtomicU64,
    pub latency_seals: AtomicU64,
}

impl Counters {
    pub(crate) fn add(c: &AtomicU64, n: u64) {
        c.fetch_add(n, Ordering::Relaxed);
    }

    pub(crate) fn drop_on(&self, track: Option<TrackNum>) {
        Self::add(&self.drops, 1);
        if let Some(t) = track {
            let mut m = lock(&self.drops_by_track);
            let n = m.entry(t).or_insert(0);
            *n = n.saturating_add(1);
        }
    }
}

/// What the writing thread, the control and the handles share.
pub(crate) struct Shared<T: Tapes> {
    pub queue: Queue<Work<T>>,
    /// The failure that stopped the chain, set once.
    pub failure: OnceLock<Arc<WriteError<T::Error>>>,
    pub counters: Counters,
    pub queue_bytes: u64,
    pub watch: Arc<Watch>,
    /// The open chunks of the typed handles, which the writing thread seals when their latency is up
    pub seals: Mutex<Vec<Weak<dyn Seal<T>>>>,
    /// What the open chunks' deadlines count from
    pub epoch: Instant,
}

impl<T: Tapes> Shared<T> {
    pub(crate) fn new(queue_bytes: u64) -> Self {
        Self {
            queue: Queue::new(queue_bytes),
            failure: OnceLock::new(),
            counters: Counters::default(),
            queue_bytes,
            watch: Arc::new(Watch::new()),
            seals: Mutex::new(Vec::new()),
            epoch: Instant::now(),
        }
    }

    /// Why the queue took nothing: the failure when there was one, else closed.
    pub(crate) fn ended(&self) -> MuxError<T::Error> {
        match self.failure.get() {
            Some(f) => MuxError::Failed(f.clone()),
            None => MuxError::Closed,
        }
    }

    /// Stop the chain for good on `e`: every handle's next call returns it.
    pub(crate) fn fail(&self, e: WriteError<T::Error>) -> Arc<WriteError<T::Error>> {
        let failure = self.failure.get_or_init(|| Arc::new(e)).clone();
        self.queue.close();
        failure
    }

    pub(crate) fn stats(&self) -> MuxStats {
        let c = &self.counters;
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let (queued_bytes, waits, waiting) = self.queue.levels();
        MuxStats {
            queued_bytes,
            queue_bytes: self.queue_bytes,
            waits,
            waiting,
            drops: get(&c.drops),
            drops_by_track: lock(&c.drops_by_track)
                .iter()
                .map(|(t, n)| (*t, *n))
                .collect(),
            placed_units: get(&c.placed_units),
            placed_bytes: get(&c.placed_bytes),
            refused: get(&c.refused),
            lost: get(&c.lost),
            latency_seals: get(&c.latency_seals),
            failed: self.failure.get().is_some(),
        }
    }

    /// Send control work `w` and wait for its reply from `rx`, as a future. Dropped once sent, the work is done all
    /// the same.
    pub(crate) async fn control_async<R>(
        &self,
        w: Work<T>,
        rx: crate::reply::ReplyRx<R>,
    ) -> Result<R, MuxError<T::Error>> {
        if self.queue.push_control(w, false).is_err() {
            return Err(self.ended());
        }
        rx.await.ok_or_else(|| self.ended())
    }
}
