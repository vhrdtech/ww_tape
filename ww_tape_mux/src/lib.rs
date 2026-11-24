//! A writing thread for [ww_tape] chains: producers on any thread write one chain through handles.
//!
//! A [TapeMux] owns a [PieceChain] and its [Definitions] on a thread of its own, the writing thread. Producers hold
//! handles; each encodes on its own thread and sends finished units through one queue:
//!
//! | Handle | Sends | Placed as |
//! |---|---|---|
//! | [EventsHandle] | items of one events track, one by one or in batches ([Event]), encoded into chunks on its thread | [PieceChain::write_chunk_hashed] |
//! | [GridHandle] | time steps of one series track (its samples, or its tiles' cells), encoded the same way, a chunk per tile | [PieceChain::write_chunk_hashed] |
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
//! The typed handles ([EventsHandle], [GridHandle]) do the costly passes on the producer's thread: the chunk's
//! encoding, the BLAKE3 hash of its payload, and the leaves of the track's summaries (an event tree's runs, a
//! series' level-0 statistics). The writing thread places the chunk with its hash given and builds the summary
//! levels above the leaves: one tree per track and piece, ended before every cut and at the close, its blocks
//! deferred into the piece.
//!
//! # Latency
//!
//! A typed handle's open chunk is shared with the writing thread. The producer completes it when full; the writing
//! thread completes it when its first item has waited the handle's latency ([EventsOptions::latency]), so a quiet
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
//! # Sync and async
//!
//! Every call that waits has an async twin, named with `_async`: [ChunkSender::send_async],
//! [EventsHandle::push_async], [EventsHandle::finish_async], [TapeMux::flush_async], [TapeMux::close_async],
//! [ProgressWatch::wait_past_async] and the rest. The writing thread wakes the task; no runtime is needed, and the
//! crate depends on none. The writing thread stays an OS thread under either: its writes and syncs block.
//!
//! A sender waits for room by [Waker](core::task::Waker); a blocking call parks its thread on the same future. So
//! threads and tasks wait in one line, served in the order they came.
//!
//! The futures are cancel-safe. A send dropped while it waits leaves the line and sends nothing. A typed handle's
//! push dropped while it waits keeps the chunks it completed: the handle's next push, complete, finish or drop sends
//! them first, so no item is lost and the track stays in order. A control call dropped once sent is done all the same.
//! A timeout of the caller's runtime bounds a wait. A typed handle's drop sends its open chunk blocking: async code
//! ends a handle with its `finish_async`.
//!
//! # Close and drop
//!
//! [TapeMux::close] takes nothing more: every handle's next call returns [MuxError::Closed]. Units sent before are
//! placed, then the chain is closed. A close doesn't wait for the handles to be dropped. A mux dropped unclosed
//! places what was sent, syncs, and leaves its piece unclosed, as after a crash: [TapeMux::resume] goes on. It
//! rebuilds the summary trees of the piece it goes on with from the chunks the piece kept, so given the items the
//! crash lost, the chain gets the bytes of the run without the crash.
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
mod grid;
mod handle;
mod open;
mod overview;
mod progress;
mod queue;
mod reply;
mod shared;
mod summaries;
mod thread;
mod wait;

use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use exact_units::Bytes;
use ww_tape::index::IndexEntry;
use ww_tape::{
    ChainOptions, ChainPiece, ClockNum, ClockSpec, CloseReason, Definitions, Describe, MapSegment,
    OwnerId, PieceChain, PieceStore, Resumed, TrackNum, TrackSpec, TrackStart,
};

pub use error::{CallError, EncodeError, ItemError, MuxError, Unit};
pub use events::{
    CHUNK_BYTES_DEFAULT, CHUNK_ITEMS_DEFAULT, Event, EventsHandle, EventsOptions, ITEM_BYTES_MAX,
    LATENCY_DEFAULT,
};
pub use grid::{CHUNK_STEPS_DEFAULT, GridHandle, GridOptions};
pub use handle::{
    ChunkSender, GroupHandle, Pressure, RecordsHandle, RelayHandle, Sent, StateHandle,
};
pub use progress::{Progress, ProgressWatch, TrackProgress, WaitPast};
pub use shared::{RETURNED_BUFFERS_MAX, RelayMode};

use reply::reply;
use shared::{Shared, Work};
use wait::block_on;

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
    /// Open chunks of quiet handles the writing thread completed when their latency was up.
    pub latency_complete_count: u64,
    /// Whether the mux failed ([MuxError::Failed]).
    pub failed: bool,
}

/// Writes one chain from a thread of its own, for producers on any thread; see the [crate docs](crate).
pub struct TapeMux<T: PieceStore> {
    shared: Arc<Shared<T>>,
    thread: Option<JoinHandle<()>>,
}

impl<T> TapeMux<T>
where
    T: PieceStore + Send + 'static,
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
        Self::spawn(chain, defs, None, options)
    }

    /// Write `chain`, just resumed ([PieceChain::resume_on]) as `resumed` says, from a new writing thread: the
    /// definitions start empty and number as the resumed piece does ([Resumed::numbers]).
    ///
    /// The summary trees of the resumed piece are rebuilt from the chunks it kept before the mux writes anything:
    /// the tracks are defined again first. See [TapeMux::resume].
    pub fn start_resumed(
        chain: PieceChain<T>,
        resumed: &Resumed,
        options: MuxOptions,
    ) -> Result<Self, MuxError<T::Error>> {
        let defs = Definitions::with_numbers(resumed.numbers.clone());
        Self::spawn(chain, defs, Some(resumed.entries.clone()), options)
    }

    fn spawn(
        chain: PieceChain<T>,
        defs: Definitions,
        rebuild: Option<Vec<IndexEntry>>,
        options: MuxOptions,
    ) -> Result<Self, MuxError<T::Error>> {
        let hash = chain.integrity() == Some(ww_tape::Integrity::Full);
        let shared = Arc::new(Shared::new(options.queue_bytes.get(), hash));
        let theirs = shared.clone();
        let thread = std::thread::Builder::new()
            .name("ww_tape_mux".to_owned())
            .spawn(move || thread::run(chain, defs, rebuild, theirs, options.tick))
            .map_err(MuxError::Spawn)?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    /// Start chain `chain` of `owner` in `store` ([PieceChain::create_on]) and write it from a new writing thread.
    pub fn create(
        store: T,
        owner: OwnerId,
        chain: u64,
        tracks: &[TrackStart],
        options: ChainOptions,
        mux: MuxOptions,
    ) -> Result<Self, MuxError<T::Error>> {
        let chain =
            PieceChain::create_on(store, owner, chain, tracks, options).map_err(MuxError::Start)?;
        Self::start(chain, Definitions::new(), mux)
    }

    /// Reopen chain `chain` of `owner` in `store` ([PieceChain::resume_on]) and write on from a new writing thread.
    ///
    /// The definitions start empty and number as the resumed piece does ([Resumed::numbers]): a track or clock
    /// defined again gets its number back. Its items go on after those [Resumed::entries] and the closed pieces
    /// hold.
    ///
    /// The summary trees of the piece the crash tore are rebuilt, so the piece goes on as it would have without the
    /// crash: before the mux first writes (a unit placed, a cut, a sync, the close), each chunk the piece kept of a
    /// track defined again is read back and its leaves computed as a typed handle computed them; the summary blocks
    /// the piece kept are not written again, those the crash lost are. Define the tracks again before sending
    /// anything. Given again every item after the last chunk kept of each track, through typed handles cut as
    /// before, the chain gets the bytes of the run without the crash.
    ///
    /// The flags a [GridHandle] took ([GridHandle::push_flagged]) are in its chunks' flag runs, so the leaves
    /// of kept samples are rebuilt with them. A tree is rebuilt for each track a typed handle summarises by
    /// default; one is dropped when a handle without summaries is made for its track (a [ChunkSender], a handle
    /// with summaries off), or when the track's next chunk comes without a summary.
    pub fn resume(
        store: T,
        owner: OwnerId,
        chain: u64,
        options: ChainOptions,
        mux: MuxOptions,
    ) -> Result<(Self, Resumed), MuxError<T::Error>> {
        let (chain, resumed) =
            PieceChain::resume_on(store, owner, chain, options).map_err(MuxError::Start)?;
        Ok((Self::start_resumed(chain, &resumed, mux)?, resumed))
    }

    /// Define track `t` ([Definitions::define_track]) and give its number, once the writing thread holds it.
    pub fn define_track(&self, t: TrackSpec<'_>) -> Result<TrackNum, MuxError<T::Error>> {
        block_on(self.define_track_async(t))
    }

    /// [Self::define_track] as a future. Like every async twin of the mux, it is woken by the writing thread and
    /// needs no runtime; dropped once sent, the work is done all the same.
    pub async fn define_track_async(
        &self,
        t: TrackSpec<'_>,
    ) -> Result<TrackNum, MuxError<T::Error>> {
        let (tx, rx) = reply();
        let w = Work::DefineTrack {
            item: define::track(t)?,
            reply: tx,
        };
        Ok(self.shared.control_async(w, rx).await??)
    }

    /// Define clock `c` ([Definitions::define_clock]) and give its number.
    pub fn define_clock(&self, c: ClockSpec<'_>) -> Result<ClockNum, MuxError<T::Error>> {
        block_on(self.define_clock_with_maps_async(c, &[]))
    }

    /// [Self::define_clock] as a future.
    pub async fn define_clock_async(
        &self,
        c: ClockSpec<'_>,
    ) -> Result<ClockNum, MuxError<T::Error>> {
        self.define_clock_with_maps_async(c, &[]).await
    }

    /// Define clock `c` with maps from it to held clocks ([Definitions::define_clock_with_maps]) and give its number.
    pub fn define_clock_with_maps(
        &self,
        c: ClockSpec<'_>,
        maps: &[(ClockNum, &[MapSegment])],
    ) -> Result<ClockNum, MuxError<T::Error>> {
        block_on(self.define_clock_with_maps_async(c, maps))
    }

    /// [Self::define_clock_with_maps] as a future.
    pub async fn define_clock_with_maps_async(
        &self,
        c: ClockSpec<'_>,
        maps: &[(ClockNum, &[MapSegment])],
    ) -> Result<ClockNum, MuxError<T::Error>> {
        let (tx, rx) = reply();
        let w = Work::DefineClock {
            items: define::clock(c, maps)?,
            reply: tx,
        };
        Ok(self.shared.control_async(w, rx).await??)
    }

    /// Hold the maps, bundles and schemas `defs` as one batch ([Definitions::define_all]).
    pub fn define_all(&self, defs: &[Describe<'_>]) -> Result<(), MuxError<T::Error>> {
        block_on(self.define_all_async(defs))
    }

    /// [Self::define_all] as a future.
    pub async fn define_all_async(&self, defs: &[Describe<'_>]) -> Result<(), MuxError<T::Error>> {
        let items = defs
            .iter()
            .map(ww_tape::open::encode)
            .collect::<Result<Vec<_>, _>>()?;
        let (tx, rx) = reply();
        let w = Work::DefineAll { items, reply: tx };
        Ok(self.shared.control_async(w, rx).await??)
    }

    /// A copy of the definitions the mux holds, as of every unit sent before.
    pub fn definitions(&self) -> Result<Definitions, MuxError<T::Error>> {
        block_on(self.definitions_async())
    }

    /// [Self::definitions] as a future.
    pub async fn definitions_async(&self) -> Result<Definitions, MuxError<T::Error>> {
        let (tx, rx) = reply();
        self.shared
            .control_async(Work::Definitions { reply: tx }, rx)
            .await
    }

    /// A sender of chunks of events or series track `track`.
    pub fn chunks(&self, track: TrackNum) -> Result<ChunkSender<T>, MuxError<T::Error>> {
        block_on(self.chunks_async(track))
    }

    /// [Self::chunks] as a future.
    pub async fn chunks_async(
        &self,
        track: TrackNum,
    ) -> Result<ChunkSender<T>, MuxError<T::Error>> {
        match self.facts(track).await? {
            Some(f) if f.state.is_none() => {
                self.plain(track).await?;
                Ok(ChunkSender::new(self.shared.clone(), track, f))
            }
            _ => Err(CallError::Kind { track }.into()),
        }
    }

    /// A handle that puts on state track `track`.
    pub fn state(&self, track: TrackNum) -> Result<StateHandle<T>, MuxError<T::Error>> {
        block_on(self.state_async(track))
    }

    /// [Self::state] as a future.
    pub async fn state_async(&self, track: TrackNum) -> Result<StateHandle<T>, MuxError<T::Error>> {
        match self.facts(track).await? {
            Some(f) if f.state.is_some() => Ok(StateHandle::new(self.shared.clone(), track)),
            _ => Err(CallError::Kind { track }.into()),
        }
    }

    /// A handle that puts groups of items on state tracks `tracks`, each group taken whole or not at all.
    pub fn group(&self, tracks: &[TrackNum]) -> Result<GroupHandle<T>, MuxError<T::Error>> {
        block_on(self.group_async(tracks))
    }

    /// [Self::group] as a future.
    pub async fn group_async(
        &self,
        tracks: &[TrackNum],
    ) -> Result<GroupHandle<T>, MuxError<T::Error>> {
        for &track in tracks {
            match self.facts(track).await? {
                Some(f) if f.state.is_some() => {}
                _ => return Err(CallError::Kind { track }.into()),
            }
        }
        Ok(GroupHandle::new(self.shared.clone(), tracks.to_vec()))
    }

    /// A handle that sends records blocks.
    pub fn records(&self) -> RecordsHandle<T> {
        RecordsHandle::new(self.shared.clone())
    }

    /// A handle that passes on another producer's blocks as `mode` says.
    pub fn relay(&self, mode: RelayMode) -> RelayHandle<T> {
        RelayHandle::new(self.shared.clone(), mode)
    }

    /// Track `track`'s definition, encoded; refused when the mux doesn't hold it.
    async fn track_def(&self, track: TrackNum) -> Result<Vec<u8>, MuxError<T::Error>> {
        let (tx, rx) = reply();
        let bytes = self
            .shared
            .control_async(Work::TrackDef { track, reply: tx }, rx)
            .await?;
        Ok(bytes.ok_or(CallError::Kind { track })?)
    }

    /// Tell the writing thread that the handle being made writes no summaries of `track`.
    pub(crate) async fn plain(&self, track: TrackNum) -> Result<(), MuxError<T::Error>> {
        let (tx, rx) = reply();
        self.shared
            .control_async(Work::Plain { track, reply: tx }, rx)
            .await
    }

    async fn facts(
        &self,
        track: TrackNum,
    ) -> Result<Option<ww_tape::TrackFacts>, MuxError<T::Error>> {
        let (tx, rx) = reply();
        self.shared
            .control_async(Work::Facts { track, reply: tx }, rx)
            .await
    }

    /// Cut to a new piece once every unit sent before is placed ([PieceChain::cut_now]).
    pub fn cut(&self) -> Result<(), MuxError<T::Error>> {
        block_on(self.cut_async())
    }

    /// [Self::cut] as a future.
    pub async fn cut_async(&self) -> Result<(), MuxError<T::Error>> {
        let (tx, rx) = reply();
        self.shared
            .control_async(Work::Cut { reply: tx }, rx)
            .await?
    }

    /// Return once every unit sent before is placed.
    pub fn flush(&self) -> Result<(), MuxError<T::Error>> {
        block_on(self.flush_as(false))
    }

    /// Return once every unit sent before is durable.
    pub fn sync(&self) -> Result<(), MuxError<T::Error>> {
        block_on(self.flush_as(true))
    }

    /// [Self::flush] as a future.
    pub async fn flush_async(&self) -> Result<(), MuxError<T::Error>> {
        self.flush_as(false).await
    }

    /// [Self::sync] as a future.
    pub async fn sync_async(&self) -> Result<(), MuxError<T::Error>> {
        self.flush_as(true).await
    }

    async fn flush_as(&self, sync: bool) -> Result<(), MuxError<T::Error>> {
        let (tx, rx) = reply();
        self.shared
            .control_async(Work::Flush { sync, reply: tx }, rx)
            .await?
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
        block_on(self.close_as_async(CloseReason::Closed))
    }

    /// Close the chain for `reason`; see [Self::close]. Every handle's next call returns [MuxError::Closed].
    pub fn close_as(
        self,
        reason: CloseReason,
    ) -> Result<Vec<ChainPiece<T::Key>>, MuxError<T::Error>> {
        block_on(self.close_as_async(reason))
    }

    /// [Self::close] as a future.
    pub async fn close_async(self) -> Result<Vec<ChainPiece<T::Key>>, MuxError<T::Error>> {
        self.close_as_async(CloseReason::Closed).await
    }

    /// [Self::close_as] as a future. Dropped before the writing thread ended, the mux is dropped: the close goes on
    /// and the drop waits for it on this thread.
    pub async fn close_as_async(
        mut self,
        reason: CloseReason,
    ) -> Result<Vec<ChainPiece<T::Key>>, MuxError<T::Error>> {
        let (tx, rx) = reply();
        let w = Work::Close { reason, reply: tx };
        let r = match self.shared.queue.push_control(w, true) {
            Ok(()) => rx.await.unwrap_or_else(|| Err(self.shared.ended())),
            Err(_) => Err(self.shared.ended()),
        };
        // the thread ends right after its reply: joined once it says so, the join doesn't block a task
        self.watch().wait_past_async(u64::MAX).await;
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

impl<T: PieceStore> Drop for TapeMux<T> {
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
