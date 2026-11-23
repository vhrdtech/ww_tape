//! The writing thread: takes units from the queue in order, places them on the chain, and does the chores.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ww_tape::chunk::ChunkHead;
use ww_tape::index::IndexEntry;
use ww_tape::{
    ChainPiece, CloseReason, ColumnSpec, Definitions, PieceChain, PieceStore, Slot as DefSlot,
    WriteError,
};

use crate::define;
use crate::error::MuxError;
use crate::open;
use crate::queue::Popped;
use crate::shared::{BlockHow, ClosedTx, Counters, RelayMode, Shared, Slot, Work};
use crate::summaries::{Added, ChunkSummary, Trees};

/// How the thread ended.
enum Exit<T: PieceStore> {
    Close(CloseReason, ClosedTx<T>),
    /// Dropped, or the queue ended after a failure: the piece stays unclosed.
    Stop,
}

/// Closes the queue when the thread ends, by return or by panic, so no sender waits for it.
struct CloseOnExit<'a, T: PieceStore>(&'a Shared<T>);

impl<T: PieceStore> Drop for CloseOnExit<'_, T> {
    fn drop(&mut self) {
        self.0.queue.close();
        self.0.watch.end();
    }
}

/// Whether `e` leaves the chain unable to write on: a failed write or sync, a cut that could not start the next
/// piece. Any other refusal wrote nothing.
fn stops<T: PieceStore>(chain: &mut PieceChain<T>, e: &WriteError<T::Error>) -> bool {
    matches!(
        e,
        WriteError::Sink { .. } | WriteError::Failed | WriteError::Read(_)
    ) || chain.writer().is_err()
}

/// What the writing thread works on besides the chain and its definitions.
struct State<'a, T: PieceStore> {
    shared: &'a Shared<T>,
    trees: Trees,
    /// Open chunks completed for their latency that found the queue closed: placed before the close
    late: Vec<(Work<T>, u64)>,
    /// The blocks a resume kept of the piece whose trees are still to be rebuilt: before the first write, so the
    /// tracks are defined again by then
    rebuild: Option<Vec<IndexEntry>>,
}

/// The thread: place every unit until a close or a stop, ticking every `tick`.
pub(crate) fn run<T: PieceStore>(
    mut chain: PieceChain<T>,
    mut defs: Definitions,
    rebuild: Option<Vec<IndexEntry>>,
    shared: Arc<Shared<T>>,
    tick: Duration,
) {
    let _close = CloseOnExit(&shared);
    let mut st = State {
        shared: &shared,
        trees: Trees::new(chain.seq()),
        late: Vec::new(),
        rebuild,
    };
    let mut next_tick = Instant::now().checked_add(tick);
    let exit = loop {
        let deadline = match (next_tick, open::next_due(&shared)) {
            (Some(t), Some(d)) => t.min(d),
            (t, d) => t.or(d).unwrap_or_else(far),
        };
        match shared.queue.pop(deadline) {
            Popped::Unit(work, bytes) => {
                let exit = place(&mut chain, &mut defs, &mut st, work, bytes);
                shared.queue.release(bytes);
                shared.watch.publish(&mut chain, None);
                if let Some(exit) = exit {
                    break exit;
                }
            }
            Popped::Timeout => {}
            Popped::Ended => break Exit::Stop,
        }
        let now = Instant::now();
        // a quiet handle's open chunk goes behind what its handle sent before
        let late = &mut st.late;
        open::complete_due(&shared, now, false, &mut |work, bytes| {
            if let Err(work) = shared.queue.push_control(work, false) {
                late.push((work, bytes));
            }
        });
        if next_tick.is_some_and(|t| now >= t) {
            chores(&mut chain, &defs, &mut st);
            shared.watch.publish(&mut chain, None);
            next_tick = now.checked_add(tick);
        }
    };
    match exit {
        Exit::Close(reason, reply) => {
            let r = close(chain, &mut defs, &mut st, reason);
            reply.send(r);
        }
        Exit::Stop => {
            if shared.failure.get().is_none()
                && let Ok(w) = chain.writer()
                && let Err(e) = w.sync()
            {
                shared.fail(e);
            }
        }
    }
}

/// A deadline that never comes in practice: a tick so long it overflows the clock.
fn far() -> Instant {
    let now = Instant::now();
    now.checked_add(Duration::from_secs(1 << 30)).unwrap_or(now)
}

/// The time-driven chores: sync and checkpoints by time; a cut by age while no chunk comes.
fn chores<T: PieceStore>(chain: &mut PieceChain<T>, defs: &Definitions, st: &mut State<'_, T>) {
    let shared = st.shared;
    // nothing is written before the trees of a resumed piece are rebuilt
    if shared.failure.get().is_some() || st.rebuild.is_some() {
        return;
    }
    let r = (|| {
        chain.writer()?.tick()?;
        let by_age = chain.cut_policy().interval.is_some();
        if by_age && chain.cut_due() && chain.writer()?.chunks() > 0 {
            cut(chain, defs, st)?;
        }
        Ok(())
    })();
    if let Err(e) = r {
        if stops(chain, &e) {
            shared.fail(e);
        } else {
            // a cut refused for its definitions: the next chunk cuts, or is refused, itself
            Counters::add(&shared.counters.refused, 1);
        }
    }
}

/// Rebuild the summary trees of a resumed piece, once, before the first write: a failure stops the chain, a refusal
/// is counted.
fn rebuilt<T: PieceStore>(chain: &mut PieceChain<T>, defs: &Definitions, st: &mut State<'_, T>) {
    let Some(entries) = st.rebuild.take() else {
        return;
    };
    let shared = st.shared;
    if shared.failure.get().is_some() {
        return;
    }
    match st.trees.rebuild(chain, defs, &entries) {
        Ok(refused) => Counters::add(&shared.counters.refused, refused),
        Err(e) if stops(chain, &e) => {
            shared.fail(e);
        }
        Err(_) => Counters::add(&shared.counters.refused, 1),
    }
}

/// Whether placing `work` writes: it waits for the rebuild of a resumed piece's trees.
fn writes<T: PieceStore>(work: &Work<T>) -> bool {
    matches!(
        work,
        Work::Chunk { .. }
            | Work::Block { .. }
            | Work::Put { .. }
            | Work::Cut { .. }
            | Work::Flush { sync: true, .. }
            | Work::Close { .. }
    )
}

/// End the summary trees of the piece and cut to a new one.
fn cut<T: PieceStore>(
    chain: &mut PieceChain<T>,
    defs: &Definitions,
    st: &mut State<'_, T>,
) -> Result<(), WriteError<T::Error>> {
    end_trees(chain, defs, st)?;
    chain.cut_now(defs)
}

/// End the summary trees of the piece: before a cut and the close.
fn end_trees<T: PieceStore>(
    chain: &mut PieceChain<T>,
    defs: &Definitions,
    st: &mut State<'_, T>,
) -> Result<(), WriteError<T::Error>> {
    let refused = st.trees.end(chain, defs)?;
    let n = u64::try_from(refused.len()).unwrap_or(u64::MAX);
    Counters::add(&st.shared.counters.refused, n);
    Ok(())
}

/// Place a typed handle's or a chunk sender's chunk, its summary into its track's tree.
fn place_chunk<T: PieceStore>(
    chain: &mut PieceChain<T>,
    defs: &Definitions,
    st: &mut State<'_, T>,
    head: &ChunkHead,
    columns: &[(ColumnSpec, Vec<u8>)],
    hash: Option<[u8; 32]>,
    summary: Option<Box<ChunkSummary>>,
) -> Result<(), WriteError<T::Error>> {
    // the chain cuts before the chunk: the trees end in the piece their items are in
    if chain.cut_due() {
        end_trees(chain, defs, st)?;
    }
    let refs: Vec<(ColumnSpec, &[u8])> = columns.iter().map(|(c, b)| (*c, b.as_slice())).collect();
    match hash {
        Some(hash) => chain.write_chunk_hashed(defs, head, &refs, hash)?,
        None => chain.write_chunk(defs, head, &refs)?,
    };
    match summary {
        Some(s) => {
            if let Added::Refused = st.trees.add(chain, defs, head, *s)? {
                Counters::add(&st.shared.counters.refused, 1);
            }
        }
        None => st.trees.plain(head.track_number),
    }
    Ok(())
}

/// Place `work` of `bytes`; the exit when it ends the thread.
fn place<T: PieceStore>(
    chain: &mut PieceChain<T>,
    defs: &mut Definitions,
    st: &mut State<'_, T>,
    work: Work<T>,
    bytes: u64,
) -> Option<Exit<T>> {
    let shared = st.shared;
    if writes(&work) {
        rebuilt(chain, defs, st);
    }
    let failed = shared.failure.get().is_some();
    match work {
        Work::Chunk {
            from,
            head,
            columns,
            hash,
            summary,
        } => {
            if !failed {
                let r = place_chunk(chain, defs, st, &head, &columns, hash, summary);
                let ok = r.is_ok();
                placed(chain, shared, &from, r, bytes);
                if ok {
                    shared.watch.publish(chain, Some(&head));
                }
            } else {
                Counters::add(&shared.counters.lost, 1);
            }
            for (_, buf) in columns {
                from.give_back(buf);
            }
        }
        Work::Block {
            from,
            how,
            mut block,
        } => {
            if !failed {
                let r = place_block(chain, defs, how, &mut block);
                placed(chain, shared, &from, r, bytes);
            } else {
                Counters::add(&shared.counters.lost, 1);
            }
            from.give_back(block);
        }
        Work::Put {
            from,
            track,
            ticks,
            item,
        } => {
            if !failed {
                let r = chain.put(defs, track, ticks, &item).map(|_| ());
                placed(chain, shared, &from, r, bytes);
            } else {
                Counters::add(&shared.counters.lost, 1);
            }
            from.give_back(item);
        }
        Work::DefineTrack { item, reply } => reply.send(define::hold_track(defs, &item)),
        Work::DefineClock { items, reply } => reply.send(define::hold_clock(defs, &items)),
        Work::DefineAll { items, reply } => reply.send(define::hold_all(defs, &items)),
        Work::Facts { track, reply } => reply.send(defs.track(track)),
        Work::Plain { track, reply } => {
            // the handle writes no summaries: the tree rebuilt of its kept chunks is none the run built
            st.trees.drop_rebuilt(track);
            reply.send(());
        }
        Work::TrackDef { track, reply } => reply.send(
            defs.get(&DefSlot::Track { number: track })
                .map(<[u8]>::to_vec),
        ),
        Work::Definitions { reply } => reply.send(defs.clone()),
        Work::Cut { reply } => {
            let r = match shared.failure.get() {
                Some(f) => Err(MuxError::Failed(f.clone())),
                None => cut(chain, defs, st).map_err(|e| refused(chain, shared, e)),
            };
            // the caller may ask the watch as soon as it has the reply
            shared.watch.publish(chain, None);
            reply.send(r);
        }
        Work::Flush { sync, reply } => {
            let r = match shared.failure.get() {
                Some(f) => Err(MuxError::Failed(f.clone())),
                None if sync => chain
                    .writer()
                    .and_then(|w| w.sync())
                    .map_err(|e| refused(chain, shared, e)),
                None => Ok(()),
            };
            shared.watch.publish(chain, None);
            reply.send(r);
        }
        Work::Close { reason, reply } => return Some(Exit::Close(reason, reply)),
        Work::Stop => return Some(Exit::Stop),
    }
    None
}

/// A block placed as `how` says.
fn place_block<T: PieceStore>(
    chain: &mut PieceChain<T>,
    defs: &mut Definitions,
    how: BlockHow,
    block: &mut [u8],
) -> Result<(), WriteError<T::Error>> {
    match how {
        BlockHow::Records => {
            let id = chain.writer()?.next_id();
            ww_tape::set_id(block, id).map_err(WriteError::Block)?;
            chain.write_block(defs, block)
        }
        BlockHow::Relay(RelayMode::Relay) => chain.relay_block(defs, block),
        BlockHow::Relay(RelayMode::Adopt) => chain.adopt_block(defs, block),
    }
}

/// Count a data unit of `bytes` placed; a refusal goes to its handle, a failure stops the chain.
fn placed<T: PieceStore>(
    chain: &mut PieceChain<T>,
    shared: &Shared<T>,
    from: &Slot<T::Error>,
    r: Result<(), WriteError<T::Error>>,
    bytes: u64,
) {
    let c = &shared.counters;
    match r {
        Ok(()) => {
            Counters::add(&c.placed_units, 1);
            Counters::add(&c.placed_bytes, bytes);
        }
        Err(e) if stops(chain, &e) => {
            Counters::add(&c.lost, 1);
            shared.fail(e);
        }
        Err(e) => {
            Counters::add(&c.refused, 1);
            from.refuse(e);
        }
    }
}

/// A control call's error: a failure stops the chain, any other refusal is the caller's.
fn refused<T: PieceStore>(
    chain: &mut PieceChain<T>,
    shared: &Shared<T>,
    e: WriteError<T::Error>,
) -> MuxError<T::Error> {
    if stops(chain, &e) {
        MuxError::Failed(shared.fail(e))
    } else {
        MuxError::Refused(e)
    }
}

/// Close the chain for `reason`: every open chunk completed and placed, the summary trees ended; every piece.
fn close<T: PieceStore>(
    mut chain: PieceChain<T>,
    defs: &mut Definitions,
    st: &mut State<'_, T>,
    reason: CloseReason,
) -> Result<Vec<ChainPiece<T::Key>>, MuxError<T::Error>> {
    let shared = st.shared;
    let mut last = std::mem::take(&mut st.late);
    open::complete_due(shared, Instant::now(), true, &mut |work, bytes| {
        last.push((work, bytes));
    });
    for (work, bytes) in last {
        let exit = place(&mut chain, defs, st, work, bytes);
        debug_assert!(exit.is_none(), "a data unit gives no exit");
    }
    if let Some(f) = shared.failure.get() {
        return Err(MuxError::Failed(f.clone()));
    }
    end_trees(&mut chain, defs, st).map_err(|e| MuxError::Failed(shared.fail(e)))?;
    chain
        .close_as(reason)
        .map_err(|e| MuxError::Failed(shared.fail(e)))
}
