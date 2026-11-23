//! The open chunk of a typed handle: filled on the producer's thread, sealed there when full, and by the writing
//! thread when its latency is up, so a quiet producer's items still reach the tape.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, TryLockError, Weak};
use std::time::{Duration, Instant};

use ww_tape::chunk::ChunkHead;
use ww_tape::{ColumnSpec, Tapes};

use crate::error::EncodeError;
use crate::queue::lock;
use crate::shared::{Counters, Shared, Slot, Work};
use crate::summaries::ChunkSummary;

/// A chunk sealed by its encoder: what goes into the queue.
pub(crate) struct Sealed {
    pub head: ChunkHead,
    pub columns: Vec<(ColumnSpec, Vec<u8>)>,
    pub summary: Option<ChunkSummary>,
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
    /// Seal the items so far into a chunk, its columns in buffers of `buffers`; None when there are none.
    fn seal(&mut self, buffers: &dyn Buffers) -> Result<Option<Sealed>, EncodeError>;
}

/// An open chunk the writing thread may seal.
pub(crate) trait Seal<T: Tapes>: Send + Sync {
    /// When its latency is up, in nanoseconds from the mux's epoch; None while it holds no item.
    fn due_ns(&self) -> Option<u64>;
    /// Seal it and hand the unit to `out` while the chunk is still held, so the unit is queued before any later
    /// chunk of its handle. With `due_by`, only when its latency is up by then, and not when its producer holds it
    /// (the producer seals it then); without, waiting for the producer. Whether it sealed a unit.
    fn seal_unit(&self, due_by: Option<u64>, out: &mut dyn FnMut(Work<T>, u64)) -> bool;
}

/// A handle's open chunk, shared with the writing thread.
pub(crate) struct Open<T: Tapes, E> {
    pub encoder: Mutex<E>,
    /// One past the deadline in nanoseconds from the epoch; 0 while empty
    due: AtomicU64,
    latency: Option<Duration>,
    epoch: Instant,
    pub from: Arc<Slot<T::Error>>,
    shared: Weak<Shared<T>>,
}

impl<T: Tapes + 'static, E: Encoder + 'static> Open<T, E>
where
    T::Error: Send + Sync,
    T::Key: Send,
    T::Sink: Send,
{
    /// The open chunk of `encoder`, registered with the mux for latency seals when `latency` is given.
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
        let mine: Arc<dyn Seal<T>> = open.clone();
        let weak = Arc::downgrade(&mine);
        let mut seals = lock(&shared.seals);
        seals.retain(|w| w.strong_count() > 0);
        seals.push(weak);
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

impl<T: Tapes, E: Encoder> Open<T, E> {
    /// Seal `encoder`, held by the caller: the unit and the bytes it takes of the queue's budget.
    pub(crate) fn seal_locked(
        &self,
        encoder: &mut E,
    ) -> Result<Option<(Work<T>, u64)>, EncodeError> {
        self.due.store(0, Ordering::Relaxed);
        let Some(s) = encoder.seal(self.from.as_ref())? else {
            return Ok(None);
        };
        let specs: Vec<ColumnSpec> = s.columns.iter().map(|(c, _)| *c).collect();
        let bytes = ww_tape::chunk::chunk_len(&s.head, &specs, 0);
        // hashed here, on the thread that seals; below Integrity::Full not at all
        let hash = self
            .shared
            .upgrade()
            .is_some_and(|sh| sh.hash_payloads)
            .then(|| {
                ww_tape::piece::columns_payload_hash(s.columns.iter().map(|(_, b)| b.as_slice()))
            });
        let work = Work::Chunk {
            from: self.from.clone(),
            head: Box::new(s.head),
            columns: s.columns,
            hash,
            summary: s.summary.map(Box::new),
        };
        Ok(Some((work, u64::try_from(bytes).unwrap_or(u64::MAX))))
    }
}

impl<T: Tapes + 'static, E: Encoder + 'static> Seal<T> for Open<T, E>
where
    T::Error: Send + Sync,
    T::Key: Send,
    T::Sink: Send,
{
    fn due_ns(&self) -> Option<u64> {
        self.due.load(Ordering::Relaxed).checked_sub(1)
    }

    fn seal_unit(&self, due_by: Option<u64>, out: &mut dyn FnMut(Work<T>, u64)) -> bool {
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
        // looked at again with the chunk held: its producer may have sealed it since
        if let Some(now) = due_by
            && !self.due_ns().is_some_and(|d| d <= now)
        {
            return false;
        }
        match self.seal_locked(&mut e) {
            Ok(Some((work, bytes))) => {
                out(work, bytes);
                true
            }
            Ok(None) => false,
            Err(_) => {
                // items the handle checked as they came always encode: a refusal here is a bug, counted
                if let Some(s) = self.shared.upgrade() {
                    Counters::add(&s.counters.refused, 1);
                }
                false
            }
        }
    }
}

/// The earliest deadline of an open chunk, as an instant.
pub(crate) fn next_due<T: Tapes>(shared: &Shared<T>) -> Option<Instant> {
    let ns = lock(&shared.seals)
        .iter()
        .filter_map(|w| w.upgrade()?.due_ns())
        .min()?;
    shared.epoch.checked_add(Duration::from_nanos(ns))
}

/// Seal the open chunks whose latency is up at `now`, each unit handed to `out` while its chunk is held; with `all`,
/// every open chunk, waiting for its producer.
pub(crate) fn seal_due<T: Tapes>(
    shared: &Shared<T>,
    now: Instant,
    all: bool,
    out: &mut dyn FnMut(Work<T>, u64),
) {
    let now_ns =
        u64::try_from(now.saturating_duration_since(shared.epoch).as_nanos()).unwrap_or(u64::MAX);
    let open: Vec<Arc<dyn Seal<T>>> = lock(&shared.seals)
        .iter()
        .filter_map(Weak::upgrade)
        .collect();
    for o in open {
        let due_by = (!all).then_some(now_ns);
        if (all || o.due_ns().is_some_and(|d| d <= now_ns)) && o.seal_unit(due_by, out) {
            Counters::add(&shared.counters.latency_seal_count, u64::from(!all));
        }
    }
}
