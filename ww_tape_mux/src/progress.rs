//! What the writing thread has placed so far, published for readers in the writer's process.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use ww_tape::chunk::ChunkHead;
use ww_tape::{ItemNum, Tapes, TrackNum};

use crate::queue::lock;
use crate::wait::{block_on, block_on_until, register, wake_all};

/// The newest chunk the writing thread placed on a track.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TrackProgress {
    /// The item number after the chunk's last item: every item before it is placed.
    pub next_item: ItemNum,
    /// The chunk's last tick ([ChunkHead::last_tick]).
    pub last_tick: i64,
    /// The largest end of its items ([ChunkHead::latest_end]).
    pub latest_end: i64,
}

/// What a [TapeMux](crate::TapeMux) has placed so far: a snapshot of [ProgressWatch].
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Progress {
    /// Goes up by one each time the rest changes; a reader waits for a version past the one it holds.
    pub version: u64,
    /// The sequence number of the piece being written.
    pub piece: u64,
    /// Bytes of that piece written.
    pub piece_bytes: u64,
    /// Of them, those up to the last sync ([ww_tape::TapeWriter::synced_bytes]): durable once the sink's sync is.
    pub synced_bytes: u64,
    /// The newest chunk of each track that had one, by track number.
    pub tracks: BTreeMap<TrackNum, TrackProgress>,
    /// The writing thread ended: closed, dropped or failed. Nothing changes after.
    pub ended: bool,
}

struct State {
    progress: Progress,
    /// Readers waiting for a change, by thread or by task.
    waiting: Vec<Waker>,
}

/// The published progress and its waiters.
pub(crate) struct Watch {
    state: Mutex<State>,
}

impl Watch {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(State {
                progress: Progress::default(),
                waiting: Vec::new(),
            }),
        }
    }

    /// Publish where the writer is, with `chunk` the head of a chunk just placed; waiters wake when it changed.
    pub(crate) fn publish<T: Tapes>(
        &self,
        chain: &mut ww_tape::PieceChain<T>,
        chunk: Option<&ChunkHead>,
    ) {
        let piece = chain.seq();
        let (piece_bytes, synced_bytes) = match chain.writer() {
            Ok(w) => (w.bytes(), w.synced_bytes()),
            Err(_) => (0, 0),
        };
        let mut s = lock(&self.state);
        let p = &mut s.progress;
        let mut changed =
            (p.piece, p.piece_bytes, p.synced_bytes) != (piece, piece_bytes, synced_bytes);
        p.piece = piece;
        p.piece_bytes = piece_bytes;
        p.synced_bytes = synced_bytes;
        if let Some(h) = chunk
            && let Some(next_item) = h.first_item.checked_add(u64::from(h.count))
        {
            let t = TrackProgress {
                next_item,
                last_tick: h.last_tick,
                latest_end: h.latest_end,
            };
            changed |= p.tracks.insert(h.track_number, t) != Some(t);
        }
        if changed {
            p.version = p.version.saturating_add(1);
            let waiting = core::mem::take(&mut s.waiting);
            drop(s);
            wake_all(waiting);
        }
    }

    /// The writing thread ended.
    pub(crate) fn end(&self) {
        let mut s = lock(&self.state);
        if !s.progress.ended {
            s.progress.ended = true;
            s.progress.version = s.progress.version.saturating_add(1);
        }
        let waiting = core::mem::take(&mut s.waiting);
        drop(s);
        wake_all(waiting);
    }
}

/// Wakes a reader in the writer's process when the writing thread placed more ([crate::TapeMux::watch]).
///
/// A reader of the same tapes ([ww_tape::MemTapes], or files) waits here instead of polling, then reads the piece up
/// to [Progress::piece_bytes]. Cloned freely; it outlives the mux, whose end it reports ([Progress::ended]).
#[derive(Clone)]
pub struct ProgressWatch {
    watch: Arc<Watch>,
}

impl ProgressWatch {
    pub(crate) fn new(watch: Arc<Watch>) -> Self {
        Self { watch }
    }

    /// The progress now.
    pub fn get(&self) -> Progress {
        lock(&self.watch.state).progress.clone()
    }

    /// The progress once its version is past `version`, waiting as long as it takes; at once when the writing thread
    /// ended.
    pub fn wait_past(&self, version: u64) -> Progress {
        block_on(self.wait_past_async(version))
    }

    /// The progress once its version is past `version`, or None when `timeout` passed first.
    pub fn wait_past_for(&self, version: u64, timeout: Duration) -> Option<Progress> {
        let deadline = Instant::now().checked_add(timeout)?;
        block_on_until(self.wait_past_async(version), deadline)
    }

    /// The progress once its version is past `version`, as a future: [Self::wait_past] for a task.
    ///
    /// Woken from the writing thread; it needs no runtime. Dropped before, it takes nothing: a timeout of the
    /// caller's runtime bounds the wait.
    pub fn wait_past_async(&self, version: u64) -> WaitPast<'_> {
        WaitPast {
            watch: &self.watch,
            version,
        }
    }
}

/// A wait for progress past a version ([ProgressWatch::wait_past_async]).
pub struct WaitPast<'a> {
    watch: &'a Watch,
    version: u64,
}

impl Future for WaitPast<'_> {
    type Output = Progress;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Progress> {
        let mut s = lock(&self.watch.state);
        if is_past(&s.progress, self.version) {
            return Poll::Ready(s.progress.clone());
        }
        register(&mut s.waiting, cx.waker());
        Poll::Pending
    }
}

/// Whether `p` is what a reader holding `version` waits for: a later version, or the end.
fn is_past(p: &Progress, version: u64) -> bool {
    p.version > version || p.ended
}
