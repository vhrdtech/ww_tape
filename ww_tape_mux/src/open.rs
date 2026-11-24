//! The open chunk of a typed handle: filled on the producer's thread, completed there when full, and by the writing
//! thread when its latency is up, so a quiet producer's items still reach the tape.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, TryLockError, Weak};
use std::time::{Duration, Instant};

use ww_tape::chunk::ChunkHead;
use ww_tape::{ColumnSpec, PieceStore};

use crate::error::EncodeError;
use crate::queue::lock;
use crate::shared::{Counters, Follower, Shared, Slot, Work};
use crate::summaries::ChunkSummary;

/// A chunk completed by its encoder: what goes into the queue.
pub(crate) struct Completed {
    pub head: ChunkHead,
    pub columns: Vec<(ColumnSpec, Vec<u8>)>,
    pub summary: Option<ChunkSummary>,
    /// A grid tile chunk's overview chunks, placed right after it, level by level: one unit with it
    pub overviews: Vec<Self>,
}

/// Where an encoder takes buffers for its columns: those the mux gave back once placed.
pub(crate) trait Buffers {
    fn buffer(&self) -> Vec<u8>;
}

impl<E> Buffers for Slot<E> {
    fn buffer(&self) -> Vec<u8> {
        Self::buffer(self)
    }
}

/// What a typed handle's open chunk does.
pub(crate) trait Encoder: Send {
    /// Complete the items so far into chunks, their columns in buffers of `buffers`, each handed to `out` in the
    /// order they go to the tape; none when there are no items. On an error none is handed out.
    fn complete(
        &mut self,
        buffers: &dyn Buffers,
        out: &mut dyn FnMut(Completed),
    ) -> Result<(), EncodeError>;
}

/// An open chunk the writing thread may complete.
pub(crate) trait Complete<T: PieceStore>: Send + Sync {
    /// When its latency is up, in nanoseconds from the mux's epoch; None while it holds no item.
    fn due_ns(&self) -> Option<u64>;
    /// Complete it and hand the unit to `out` while the chunk is still held, so the unit is queued before any later
    /// chunk of its handle. With `due_by`, only when its latency is up by then, and not when its producer holds it
    /// (the producer completes it then); without, waiting for the producer. Whether it completed a unit.
    fn complete_unit(&self, due_by: Option<u64>, out: &mut dyn FnMut(Work<T>, u64)) -> bool;
}

/// A handle's open chunk, shared with the writing thread.
pub(crate) struct Open<T: PieceStore, E> {
    pub encoder: Mutex<E>,
    /// One past the deadline in nanoseconds from the epoch; 0 while empty
    due: AtomicU64,
    latency: Option<Duration>,
    epoch: Instant,
    pub from: Arc<Slot<T::Error>>,
    shared: Weak<Shared<T>>,
}

impl<T: PieceStore + 'static, E: Encoder + 'static> Open<T, E>
where
    T::Error: Send + Sync,
    T::Key: Send,
    T::Sink: Send,
{
    /// The open chunk of `encoder`, registered with the mux to complete for its latency when `latency` is given.
    pub(crate) fn register(
        shared: &Arc<Shared<T>>,
        from: Arc<Slot<T::Error>>,
        encoder: E,
        latency: Option<Duration>,
    ) -> Arc<Self> {
        let open = Arc::new(Self {
            encoder: Mutex::new(encoder),
            due: AtomicU64::new(0),
            latency,
            epoch: shared.epoch,
            from,
            shared: Arc::downgrade(shared),
        });
        let mine: Arc<dyn Complete<T>> = open.clone();
        let weak = Arc::downgrade(&mine);
        let mut open_chunks = lock(&shared.open_chunks);
        open_chunks.retain(|w| w.strong_count() > 0);
        open_chunks.push(weak);
        open
    }

    /// The chunk took its first item now: its latency runs from here.
    pub(crate) fn opened(&self) {
        let Some(latency) = self.latency else {
            return;
        };
        if self.due.load(Ordering::Relaxed) == 0 {
            let at = self.epoch.elapsed().saturating_add(latency);
            let ns = u64::try_from(at.as_nanos()).unwrap_or(u64::MAX - 1);
            self.due.store(ns.saturating_add(1), Ordering::Relaxed);
            // the writing thread may sleep past this deadline: wake it to look
            if let Some(s) = self.shared.upgrade() {
                s.queue.wake();
            }
        }
    }
}

impl<T: PieceStore, E: Encoder> Open<T, E> {
    /// Complete `encoder`, held by the caller: each unit and the bytes it takes of the queue's budget handed to
    /// `out`, in order. Whether it completed a unit.
    pub(crate) fn complete_locked(
        &self,
        encoder: &mut E,
        out: &mut dyn FnMut((Work<T>, u64)),
    ) -> Result<bool, EncodeError> {
        self.due.store(0, Ordering::Relaxed);
        // hashed here, on the thread that completes; below Integrity::Full not at all
        let hash_payloads = self.shared.upgrade().is_some_and(|sh| sh.hash_payloads);
        let mut any = false;
        encoder.complete(self.from.as_ref(), &mut |s| {
            let mut bytes = 0usize;
            let mut placed = |c: &Completed| {
                let specs: Vec<ColumnSpec> = c.columns.iter().map(|(c, _)| *c).collect();
                bytes = bytes.saturating_add(ww_tape::chunk::chunk_len(&c.head, &specs, 0));
                hash_payloads.then(|| {
                    ww_tape::piece::columns_payload_hash(
                        c.columns.iter().map(|(_, b)| b.as_slice()),
                    )
                })
            };
            let hash = placed(&s);
            let overviews = s
                .overviews
                .iter()
                .map(&mut placed)
                .collect::<Vec<_>>()
                .into_iter()
                .zip(s.overviews)
                .map(|(hash, o)| Follower {
                    head: o.head,
                    columns: o.columns,
                    hash,
                })
                .collect();
            let work = Work::Chunk {
                from: self.from.clone(),
                head: Box::new(s.head),
                columns: s.columns,
                hash,
                summary: s.summary.map(Box::new),
                overviews,
            };
            any = true;
            out((work, u64::try_from(bytes).unwrap_or(u64::MAX)));
        })?;
        Ok(any)
    }

    /// [Self::complete_locked] where no caller takes its error: items the handle checked as they came always
    /// encode, so a refusal is a bug, counted. Whether it completed a unit.
    pub(crate) fn complete_counted(
        &self,
        encoder: &mut E,
        out: &mut dyn FnMut((Work<T>, u64)),
    ) -> bool {
        self.complete_locked(encoder, out).unwrap_or_else(|_e| {
            if let Some(s) = self.shared.upgrade() {
                Counters::add(&s.counters.refused, 1);
            }
            false
        })
    }
}

impl<T: PieceStore + 'static, E: Encoder + 'static> Complete<T> for Open<T, E>
where
    T::Error: Send + Sync,
    T::Key: Send,
    T::Sink: Send,
{
    fn due_ns(&self) -> Option<u64> {
        self.due.load(Ordering::Relaxed).checked_sub(1)
    }

    fn complete_unit(&self, due_by: Option<u64>, out: &mut dyn FnMut(Work<T>, u64)) -> bool {
        let mut e = match due_by {
            None => lock(&self.encoder),
            Some(_) => match self.encoder.try_lock() {
                Ok(e) => e,
                Err(TryLockError::WouldBlock) => return false,
                Err(TryLockError::Poisoned(p)) => {
                    // as lock does: a producer panicked holding its chunk
                    drop(p);
                    lock(&self.encoder)
                }
            },
        };
        // looked at again with the chunk held: its producer may have completed it since
        if let Some(now) = due_by
            && !self.due_ns().is_some_and(|d| d <= now)
        {
            return false;
        }
        self.complete_counted(&mut e, &mut |(work, bytes)| out(work, bytes))
    }
}

/// The earliest deadline of an open chunk, as an instant.
pub(crate) fn next_due<T: PieceStore>(shared: &Shared<T>) -> Option<Instant> {
    let ns = lock(&shared.open_chunks)
        .iter()
        .filter_map(|w| w.upgrade()?.due_ns())
        .min()?;
    shared.epoch.checked_add(Duration::from_nanos(ns))
}

/// Complete the open chunks whose latency is up at `now`, each unit handed to `out` while its chunk is held; with
/// `all`, every open chunk, waiting for its producer.
pub(crate) fn complete_due<T: PieceStore>(
    shared: &Shared<T>,
    now: Instant,
    all: bool,
    out: &mut dyn FnMut(Work<T>, u64),
) {
    let now_ns =
        u64::try_from(now.saturating_duration_since(shared.epoch).as_nanos()).unwrap_or(u64::MAX);
    let open: Vec<Arc<dyn Complete<T>>> = lock(&shared.open_chunks)
        .iter()
        .filter_map(Weak::upgrade)
        .collect();
    for o in open {
        let due_by = (!all).then_some(now_ns);
        if (all || o.due_ns().is_some_and(|d| d <= now_ns)) && o.complete_unit(due_by, out) {
            Counters::add(&shared.counters.latency_complete_count, u64::from(!all));
        }
    }
}
