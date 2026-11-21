//! What the writing thread has placed so far, published for readers in the writer's process.

use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use ww_tape::chunk::ChunkHead;
use ww_tape::{ItemNum, Tapes, TrackNum};

use crate::queue::{lock, wait};

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
}

/// The published progress and its waiters.
pub(crate) struct Watch {
    state: Mutex<State>,
    changed: Condvar,
}

impl Watch {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(State {
                progress: Progress::default(),
            }),
            changed: Condvar::new(),
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
            drop(s);
            self.changed.notify_all();
        }
    }

    /// The writing thread ended.
    pub(crate) fn end(&self) {
        let mut s = lock(&self.state);
        if !s.progress.ended {
            s.progress.ended = true;
            s.progress.version = s.progress.version.saturating_add(1);
        }
        drop(s);
        self.changed.notify_all();
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
        let mut s = lock(&self.watch.state);
        while !is_past(&s.progress, version) {
            s = wait(&self.watch.changed, s);
        }
        s.progress.clone()
    }

    /// The progress once its version is past `version`, or None when `timeout` passed first.
    pub fn wait_past_for(&self, version: u64, timeout: Duration) -> Option<Progress> {
        let deadline = Instant::now().checked_add(timeout)?;
        let mut s = lock(&self.watch.state);
        while !is_past(&s.progress, version) {
            let left = deadline.checked_duration_since(Instant::now())?;
            #[expect(clippy::expect_used, reason = "as in lock")]
            let (g, _) = self
                .watch
                .changed
                .wait_timeout(s, left)
                .expect("a mux lock poisoned by a panic");
            s = g;
        }
        Some(s.progress.clone())
    }
}

/// Whether `p` is what a reader holding `version` waits for: a later version, or the end.
fn is_past(p: &Progress, version: u64) -> bool {
    p.version > version || p.ended
}
