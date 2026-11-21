//! The writing thread: takes units from the queue in order, places them on the chain, and does the chores.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ww_tape::{ChainPiece, CloseReason, ColumnSpec, Definitions, PieceChain, Tapes, WriteError};

use crate::define;
use crate::error::MuxError;
use crate::queue::Popped;
use crate::shared::{BlockHow, ClosedTx, Counters, RelayMode, Shared, Slot, Work};

/// How the thread ended.
enum Exit<T: Tapes> {
    Close(CloseReason, ClosedTx<T>),
    /// Dropped, or the queue ended after a failure: the piece stays unclosed.
    Stop,
}

/// Closes the queue when the thread ends, by return or by panic, so no sender waits for it.
struct CloseOnExit<'a, T: Tapes>(&'a Shared<T>);

impl<T: Tapes> Drop for CloseOnExit<'_, T> {
    fn drop(&mut self) {
        self.0.queue.close();
        self.0.watch.end();
    }
}

/// Whether `e` leaves the chain unable to write on: a failed write or sync, a cut that could not start the next
/// piece. Any other refusal wrote nothing.
fn stops<T: Tapes>(chain: &mut PieceChain<T>, e: &WriteError<T::Error>) -> bool {
    matches!(
        e,
        WriteError::Sink { .. } | WriteError::Failed | WriteError::Read(_)
    ) || chain.writer().is_err()
}

/// The thread: place every unit until a close or a stop, ticking every `tick`.
pub(crate) fn run<T: Tapes>(
    mut chain: PieceChain<T>,
    mut defs: Definitions,
    shared: Arc<Shared<T>>,
    tick: Duration,
) {
    let _close = CloseOnExit(&shared);
    let mut next_tick = Instant::now().checked_add(tick);
    let exit = loop {
        let deadline = next_tick.unwrap_or_else(far);
        match shared.queue.pop(deadline) {
            Popped::Unit(work, bytes) => {
                let exit = place(&mut chain, &mut defs, &shared, work, bytes);
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
        if next_tick.is_some_and(|t| now >= t) {
            chores(&mut chain, &defs, &shared);
            shared.watch.publish(&mut chain, None);
            next_tick = now.checked_add(tick);
        }
    };
    match exit {
        Exit::Close(reason, reply) => reply.send(close(chain, &shared, reason)),
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
fn chores<T: Tapes>(chain: &mut PieceChain<T>, defs: &Definitions, shared: &Shared<T>) {
    if shared.failure.get().is_some() {
        return;
    }
    let r = (|| {
        chain.writer()?.tick()?;
        let by_age = chain.cut_policy().interval.is_some();
        if by_age && chain.cut_due() && chain.writer()?.chunks() > 0 {
            chain.cut_now(defs)?;
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

/// Place `work` of `bytes`; the exit when it ends the thread.
fn place<T: Tapes>(
    chain: &mut PieceChain<T>,
    defs: &mut Definitions,
    shared: &Shared<T>,
    work: Work<T>,
    bytes: u64,
) -> Option<Exit<T>> {
    let failed = shared.failure.get().is_some();
    match work {
        Work::Chunk {
            from,
            head,
            columns,
        } => {
            if !failed {
                let refs: Vec<(ColumnSpec, &[u8])> =
                    columns.iter().map(|(c, b)| (*c, b.as_slice())).collect();
                let r = chain.write_chunk(defs, &head, &refs).map(|_| ());
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
        Work::Definitions { reply } => reply.send(defs.clone()),
        Work::Cut { reply } => {
            let r = match shared.failure.get() {
                Some(f) => Err(MuxError::Failed(f.clone())),
                None => chain.cut_now(defs).map_err(|e| refused(chain, shared, e)),
            };
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
            reply.send(r);
        }
        Work::Close { reason, reply } => return Some(Exit::Close(reason, reply)),
        Work::Stop => return Some(Exit::Stop),
    }
    None
}

/// A block placed as `how` says.
fn place_block<T: Tapes>(
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
fn placed<T: Tapes>(
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
fn refused<T: Tapes>(
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

/// Close the chain for `reason`: every piece.
fn close<T: Tapes>(
    chain: PieceChain<T>,
    shared: &Shared<T>,
    reason: CloseReason,
) -> Result<Vec<ChainPiece<T::Key>>, MuxError<T::Error>> {
    if let Some(f) = shared.failure.get() {
        return Err(MuxError::Failed(f.clone()));
    }
    chain
        .close_as(reason)
        .map_err(|e| MuxError::Failed(shared.fail(e)))
}
