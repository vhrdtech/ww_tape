//! A writing thread for [ww_tape] chains: producers on any thread write one chain through handles.
//!
//! A [TapeMux] owns a [PieceChain] and its [Definitions] on a thread of its own, the writing thread. Producers hold
//! handles; each encodes on its own thread and sends finished units through one queue:
//!
//! | Handle | Sends | Placed as |
//! |---|---|---|
//! | [EventsHandle] | items of one events track, encoded into chunks on its thread | [PieceChain::write_chunk_hashed] |
//! | [SeriesHandle] | samples of one series track on a grid, encoded the same way | [PieceChain::write_chunk_hashed] |
//! | [ChunkSender] | a chunk of one events or series track, encoded by the caller | [PieceChain::write_chunk] |
//! | [RecordsHandle] | a records block ([ww_tape::BlockWriter]) | the writer's next block ([PieceChain::write_block]) |
//! | [RelayHandle] | another producer's block | [PieceChain::relay_block] or [PieceChain::adopt_block] |
//! | [StateHandle] | a state item, on change | [PieceChain::put] |
//!
//! The writing thread places units in the order they came, and does the chores: puts of definitions and state,
//! cuts by the chain's policy, sync and checkpoints by time ([MuxOptions::tick]), the close. Handles never pass
//! definitions: the mux holds them, and the writer assigns each track its number when it is defined
//! ([TapeMux::define_track]).
//!
//! The typed handles ([EventsHandle], [SeriesHandle]) do the costly passes on the producer's thread: the chunk's
//! encoding, the BLAKE3 hash of its payload, and the leaves of the track's summaries (an event tree's runs, a
//! series' level-0 statistics). The writing thread places the chunk with its hash given and builds the summary
//! levels above the leaves: one tree per track and piece, ended before every cut and at the close, its blocks
//! deferred into the piece.
//!
//! # Latency
//!
//! A typed handle's open chunk is shared with the writing thread. The producer seals it when full; the writing
//! thread seals it when its first item has waited the handle's latency ([EventsOptions::latency]), so a quiet
//! producer's items still reach the tape. Either way the chunk goes into the queue behind every unit the handle
//! sent before.
//!
//! # The queue and back-pressure
//!
//! The queue is bounded by bytes ([MuxOptions::queue_bytes]), not units. A unit takes its bytes when sent and gives
//! them back when placed; a unit larger than the whole budget goes when the queue is empty. Senders that wait for
//! room are served in the order they came. Each handle has a [Pressure]: wait (the default, nothing lost), fail
//! with the unit back, or drop and count.
//!
//! # Order
//!
//! One handle's units are placed in the order it sent them. Units of different handles are placed in the order
//! they reached the queue, which isn't time order: each track is indexed on its own and readers merge tracks by
//! time. A definition is placed before any data that names it, since a handle is made only once its track is held.
//!
//! # Errors
//!
//! | Kind | Example | Where it surfaces |
//! |---|---|---|
//! | Refused at the call | a chunk of another track, a block that doesn't read | the call ([MuxError::Call]); nothing sent |
//! | Refused by the writer | a chunk on a track the definitions don't hold | the handle's next call, flush or sync ([MuxError::Refused]) |
//! | The mux failed | a failed write or sync | every handle's next call and the close ([MuxError::Failed]) |
//!
//! # Progress
//!
//! [TapeMux::watch] gives a [ProgressWatch]: the piece being written, its bytes written and synced, the newest chunk
//! of each track. A reader in the writer's process waits on it for more instead of polling. Readers in another
//! process follow the files (`ww_tape_fs::follow`).
//!
//! # Close and drop
//!
//! [TapeMux::close] takes nothing more: every handle's next call returns [MuxError::Closed]. Units sent before are
//! placed, then the chain is closed. A close doesn't wait for the handles to be dropped. A mux dropped unclosed
//! places what was sent, syncs, and leaves its piece unclosed, as after a crash: [TapeMux::resume] goes on.
//!
//! # Caps
//!
//! | Cap | Value | Bounds |
//! |---|---|---|
//! | [MuxOptions::queue_bytes] | 64 MiB by default | the bytes of units sent and not yet placed |
//! | [RETURNED_BUFFERS_MAX] | 64 | buffers a handle keeps for reuse |
//! | [ITEM_BYTES_MAX] | 16 MiB | the payload of one item an [EventsHandle] takes |

mod define;
mod error;
mod events;
mod handle;
mod open;
mod progress;
mod queue;
mod reply;
mod series;
mod shared;
mod summaries;
mod thread;

use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use exact_units::Bytes;
use ww_tape::{
    ChainOptions, ChainPiece, ClockNum, ClockSpec, CloseReason, Definitions, Describe, MapSegment,
    OwnerId, PieceChain, Resumed, Tapes, TrackNum, TrackSpec, TrackStart,
};

pub use error::{CallError, EncodeError, ItemError, MuxError, Unit};
pub use events::{
    CHUNK_BYTES_DEFAULT, CHUNK_ITEMS_DEFAULT, EventsHandle, EventsOptions, ITEM_BYTES_MAX,
    LATENCY_DEFAULT,
};
pub use handle::{ChunkSender, Pressure, RecordsHandle, RelayHandle, Sent, StateHandle};
pub use progress::{Progress, ProgressWatch, TrackProgress};
pub use series::{CHUNK_SAMPLES_DEFAULT, SeriesHandle, SeriesOptions};
pub use shared::{RETURNED_BUFFERS_MAX, RelayMode};

use reply::reply;
use shared::{Shared, Work};

/// Default of [MuxOptions::queue_bytes].
pub const QUEUE_BYTES_DEFAULT: Bytes<u64> = Bytes::new(64 << 20);

/// Default of [MuxOptions::tick].
pub const TICK_DEFAULT: Duration = Duration::from_millis(100);

/// Options of a [TapeMux].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct MuxOptions {
    /// The bytes of units sent and not yet placed, at most; a unit larger than this goes alone.
    pub queue_bytes: Bytes<u64>,
    /// How often the writing thread does its chores while no unit comes: [ww_tape::TapeWriter::tick], and a cut
    /// by age ([ww_tape::CutPolicy::interval]) of a piece that holds a chunk.
    pub tick: Duration,
}

impl Default for MuxOptions {
    fn default() -> Self {
        Self {
            queue_bytes: QUEUE_BYTES_DEFAULT,
            tick: TICK_DEFAULT,
        }
    }
}

/// What a [TapeMux] did so far; read from any thread ([TapeMux::stats]).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct MuxStats {
    /// Bytes of units sent and not yet placed.
    pub queued_bytes: u64,
    /// The queue's budget ([MuxOptions::queue_bytes]).
    pub queue_bytes: u64,
    /// Sends that waited for room, counted when they start to wait.
    pub waits: u64,
    /// Sends waiting for room now.
    pub waiting: u64,
    /// Units dropped for want of room ([Pressure::Drop]).
    pub drops: u64,
    /// Of them, by track, for the handles of one track.
    pub drops_by_track: Vec<(TrackNum, u64)>,
    /// Units placed, and the bytes they took of the queue.
    pub placed_units: u64,
    pub placed_bytes: u64,
    /// Units the writer refused, and chores refused.
    pub refused: u64,
    /// Units sent before a failure and never placed.
    pub lost: u64,
    /// Open chunks of quiet handles the writing thread sealed when their latency was up.
    pub latency_seals: u64,
    /// Whether the mux failed ([MuxError::Failed]).
    pub failed: bool,
}

/// Writes one chain from a thread of its own, for producers on any thread; see the [crate docs](crate).
pub struct TapeMux<T: Tapes> {
    shared: Arc<Shared<T>>,
    thread: Option<JoinHandle<()>>,
}

impl<T> TapeMux<T>
where
    T: Tapes + Send + 'static,
    T::Sink: Send,
    T::Key: Send,
    T::Error: Send + Sync,
{
    /// Write `chain` from a new writing thread, with the definitions `defs` already holds.
    pub fn start(
        chain: PieceChain<T>,
        defs: Definitions,
        options: MuxOptions,
    ) -> Result<Self, MuxError<T::Error>> {
        let shared = Arc::new(Shared::new(options.queue_bytes.get()));
        let theirs = shared.clone();
        let thread = std::thread::Builder::new()
            .name("ww_tape_mux".to_owned())
            .spawn(move || thread::run(chain, defs, theirs, options.tick))
            .map_err(MuxError::Spawn)?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    /// Start chain `chain` of `owner` in `tapes` ([PieceChain::create_on]) and write it from a new writing thread.
    pub fn create(
        tapes: T,
        owner: OwnerId,
        chain: u64,
        tracks: &[TrackStart],
        options: ChainOptions,
        mux: MuxOptions,
    ) -> Result<Self, MuxError<T::Error>> {
        let chain =
            PieceChain::create_on(tapes, owner, chain, tracks, options).map_err(MuxError::Start)?;
        Self::start(chain, Definitions::new(), mux)
    }

    /// Reopen chain `chain` of `owner` in `tapes` ([PieceChain::resume_on]) and write on from a new writing thread.
    ///
    /// The definitions start empty and number as the resumed piece does ([Resumed::numbers]): a track or clock
    /// defined again gets its number back. Its items go on after those [Resumed::entries] and the closed pieces
    /// hold.
    pub fn resume(
        tapes: T,
        owner: OwnerId,
        chain: u64,
        options: ChainOptions,
        mux: MuxOptions,
    ) -> Result<(Self, Resumed), MuxError<T::Error>> {
        let (chain, resumed) =
            PieceChain::resume_on(tapes, owner, chain, options).map_err(MuxError::Start)?;
        let defs = Definitions::with_numbers(resumed.numbers.clone());
        Ok((Self::start(chain, defs, mux)?, resumed))
    }

    /// Define track `t` ([Definitions::define_track]) and give its number, once the writing thread holds it.
    pub fn define_track(&self, t: TrackSpec<'_>) -> Result<TrackNum, MuxError<T::Error>> {
        let (tx, rx) = reply();
        let w = Work::DefineTrack {
            item: define::track(t)?,
            reply: tx,
        };
        Ok(self.shared.control(w, rx)??)
    }

    /// Define clock `c` ([Definitions::define_clock]) and give its number.
    pub fn define_clock(&self, c: ClockSpec<'_>) -> Result<ClockNum, MuxError<T::Error>> {
        self.define_clock_with_maps(c, &[])
    }

    /// Define clock `c` with maps from it to held clocks ([Definitions::define_clock_with_maps]) and give its number.
    pub fn define_clock_with_maps(
        &self,
        c: ClockSpec<'_>,
        maps: &[(ClockNum, &[MapSegment])],
    ) -> Result<ClockNum, MuxError<T::Error>> {
        let (tx, rx) = reply();
        let w = Work::DefineClock {
            items: define::clock(c, maps)?,
            reply: tx,
        };
        Ok(self.shared.control(w, rx)??)
    }

    /// Hold the maps, bundles and schemas `defs` as one batch ([Definitions::define_all]).
    pub fn define_all(&self, defs: &[Describe<'_>]) -> Result<(), MuxError<T::Error>> {
        let items = defs
            .iter()
            .map(ww_tape::open::encode)
            .collect::<Result<Vec<_>, _>>()?;
        let (tx, rx) = reply();
        let w = Work::DefineAll { items, reply: tx };
        Ok(self.shared.control(w, rx)??)
    }

    /// A copy of the definitions the mux holds, as of every unit sent before.
    pub fn definitions(&self) -> Result<Definitions, MuxError<T::Error>> {
        let (tx, rx) = reply();
        self.shared.control(Work::Definitions { reply: tx }, rx)
    }

    /// A sender of chunks of events or series track `track`.
    pub fn chunks(&self, track: TrackNum) -> Result<ChunkSender<T>, MuxError<T::Error>> {
        match self.facts(track)? {
            Some(f) if f.state.is_none() => Ok(ChunkSender::new(self.shared.clone(), track, f)),
            _ => Err(CallError::Kind { track }.into()),
        }
    }

    /// A handle that puts on state track `track`.
    pub fn state(&self, track: TrackNum) -> Result<StateHandle<T>, MuxError<T::Error>> {
        match self.facts(track)? {
            Some(f) if f.state.is_some() => Ok(StateHandle::new(self.shared.clone(), track)),
            _ => Err(CallError::Kind { track }.into()),
        }
    }

    /// A handle that sends records blocks.
    pub fn records(&self) -> RecordsHandle<T> {
        RecordsHandle::new(self.shared.clone())
    }

    /// A handle that passes on another producer's blocks as `mode` says.
    pub fn relay(&self, mode: RelayMode) -> RelayHandle<T> {
        RelayHandle::new(self.shared.clone(), mode)
    }

    fn facts(&self, track: TrackNum) -> Result<Option<ww_tape::TrackFacts>, MuxError<T::Error>> {
        let (tx, rx) = reply();
        self.shared.control(Work::Facts { track, reply: tx }, rx)
    }

    /// Cut to a new piece once every unit sent before is placed ([PieceChain::cut_now]).
    pub fn cut(&self) -> Result<(), MuxError<T::Error>> {
        let (tx, rx) = reply();
        self.shared.control(Work::Cut { reply: tx }, rx)?
    }

    /// Return once every unit sent before is placed.
    pub fn flush(&self) -> Result<(), MuxError<T::Error>> {
        self.flush_as(false)
    }

    /// Return once every unit sent before is durable.
    pub fn sync(&self) -> Result<(), MuxError<T::Error>> {
        self.flush_as(true)
    }

    fn flush_as(&self, sync: bool) -> Result<(), MuxError<T::Error>> {
        let (tx, rx) = reply();
        self.shared.control(Work::Flush { sync, reply: tx }, rx)?
    }

    /// A watch of what the writing thread placed, for readers in this process ([ProgressWatch]).
    pub fn watch(&self) -> ProgressWatch {
        ProgressWatch::new(self.shared.watch.clone())
    }

    /// What the mux did so far.
    pub fn stats(&self) -> MuxStats {
        self.shared.stats()
    }

    /// Close the chain ([CloseReason::Closed]) once every unit sent before is placed; every piece.
    pub fn close(self) -> Result<Vec<ChainPiece<T::Key>>, MuxError<T::Error>> {
        self.close_as(CloseReason::Closed)
    }

    /// Close the chain for `reason`; see [Self::close]. Every handle's next call returns [MuxError::Closed].
    pub fn close_as(
        mut self,
        reason: CloseReason,
    ) -> Result<Vec<ChainPiece<T::Key>>, MuxError<T::Error>> {
        let (tx, rx) = reply();
        let w = Work::Close { reason, reply: tx };
        let r = match self.shared.queue.push_control(w, true) {
            Ok(()) => rx.wait().unwrap_or_else(|| Err(self.shared.ended())),
            Err(_) => Err(self.shared.ended()),
        };
        self.join();
        r
    }

    fn join(&mut self) {
        if let Some(t) = self.thread.take()
            && let Err(panic) = t.join()
        {
            // the writing thread's panic is a bug: it goes on in the caller, so it isn't lost
            std::panic::resume_unwind(panic);
        }
    }
}

impl<T: Tapes> Drop for TapeMux<T> {
    /// Place what was sent, sync, and leave the piece unclosed, as after a crash.
    fn drop(&mut self) {
        if let Some(t) = self.thread.take() {
            // refused when the queue ended already: the thread is ending
            if self.shared.queue.push_control(Work::Stop, true).is_err() {
                self.shared.queue.close();
            }
            // a panic of the writing thread while dropping is not raised again: a drop doesn't panic
            drop(t.join());
        }
    }
}
