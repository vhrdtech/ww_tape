//! The mux's queue: units in the order they came, bounded by the bytes they hold, waiters served in arrival order.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};
use std::time::Instant;

use crate::wait::wake_all;

/// `m` locked; a lock poisoned by a panic on another thread panics here too.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    #[expect(
        clippy::expect_used,
        reason = "a poisoned lock means a thread panicked inside the mux: the mux stops, as the rules for locks say"
    )]
    m.lock().expect("a mux lock poisoned by a panic")
}

/// What became of a unit sent; a unit not queued stays in the caller's slot.
pub(crate) enum Pushed {
    /// In the queue.
    Queued,
    /// No room, and the sender would not wait.
    Full,
    /// The queue takes no more units.
    Closed,
}

/// What the writing thread took.
pub(crate) enum Popped<U> {
    /// The oldest unit and the bytes it holds of the budget, given back by [Queue::release] once it is placed.
    Unit(U, u64),
    /// Nothing came before the deadline.
    Timeout,
    /// The queue is closed and empty: nothing more comes.
    Ended,
}

/// A sender waiting for room: its ticket, and how to wake it when it is first in line.
struct Waiter {
    ticket: u64,
    waker: Option<Waker>,
}

struct State<U> {
    units: VecDeque<(U, u64)>,
    /// Bytes of the units sent and not yet placed.
    queued: u64,
    /// No more units: closed or stopped.
    closed: bool,
    /// Senders waiting for room, in the order they came: the first is served next.
    waiters: VecDeque<Waiter>,
    /// The ticket the next waiting sender takes.
    next_ticket: u64,
    /// Senders that waited for room, counted when they start to wait.
    waits: u64,
    /// The taker is to look at its deadline again ([Queue::wake]).
    woken: bool,
}

impl<U> State<U> {
    /// The waker of the first waiter, taken: it is to look whether its unit fits now.
    fn first(&mut self) -> Option<Waker> {
        self.waiters.front_mut().and_then(|w| w.waker.take())
    }

    /// The wakers of every waiter, taken.
    fn everyone(&mut self) -> Vec<Waker> {
        self.waiters
            .iter_mut()
            .filter_map(|w| w.waker.take())
            .collect()
    }
}

/// A queue of units bounded by bytes, for many senders and one taker.
///
/// Senders that wait for room wait by [Waker], served in the order they came; a thread waits on the same future
/// ([crate::wait::block_on]). The taker waits on a [Condvar] with a deadline.
pub(crate) struct Queue<U> {
    state: Mutex<State<U>>,
    budget: u64,
    /// The taker waiting for a unit.
    work: Condvar,
}

impl<U> Queue<U> {
    /// An empty queue of `budget` bytes.
    pub(crate) fn new(budget: u64) -> Self {
        Self {
            state: Mutex::new(State {
                units: VecDeque::new(),
                queued: 0,
                closed: false,
                waiters: VecDeque::new(),
                next_ticket: 0,
                waits: 0,
                woken: false,
            }),
            budget,
            work: Condvar::new(),
        }
    }

    /// Whether `bytes` more fit: within the budget, or the queue is empty, so a unit larger than the budget still
    /// goes alone.
    fn fits(&self, s: &State<U>, bytes: u64) -> bool {
        s.queued == 0 || s.queued.saturating_add(bytes) <= self.budget
    }

    /// Append the unit in `slot`, of `bytes`, when it fits now and nobody waits; else leave it there.
    pub(crate) fn try_push(&self, slot: &mut Option<U>, bytes: u64) -> Pushed {
        let s = lock(&self.state);
        if s.closed {
            return Pushed::Closed;
        }
        if s.waiters.is_empty() && self.fits(&s, bytes) {
            if let Some(unit) = slot.take() {
                self.append(s, unit, bytes);
            }
            return Pushed::Queued;
        }
        Pushed::Full
    }

    /// Append the unit in `slot`, of `bytes`, once it fits behind the senders that waited before: [Pushed::Queued],
    /// or [Pushed::Closed] with the unit left in `slot`. Dropped before, the unit stays in `slot` and the sender
    /// leaves the line.
    pub(crate) fn push_wait<'a>(&'a self, slot: &'a mut Option<U>, bytes: u64) -> PushWait<'a, U> {
        PushWait {
            queue: self,
            slot,
            bytes,
            ticket: None,
        }
    }

    fn append(&self, mut s: MutexGuard<'_, State<U>>, unit: U, bytes: u64) {
        s.queued = s.queued.saturating_add(bytes);
        s.units.push_back((unit, bytes));
        drop(s);
        self.work.notify_one();
    }

    /// Append `unit`, outside the budget and ahead of no one; then, with `last`, close the queue behind it, so it
    /// is the last unit. The unit back when the queue is closed.
    pub(crate) fn push_control(&self, unit: U, last: bool) -> Result<(), U> {
        let mut s = lock(&self.state);
        if s.closed {
            return Err(unit);
        }
        s.closed = last;
        s.units.push_back((unit, 0));
        let waiting = if last { s.everyone() } else { Vec::new() };
        drop(s);
        self.work.notify_one();
        wake_all(waiting);
        Ok(())
    }

    /// Take no more units; senders waiting for room get theirs back. Units queued stay to be taken.
    pub(crate) fn close(&self) {
        let mut s = lock(&self.state);
        s.closed = true;
        let waiting = s.everyone();
        drop(s);
        wake_all(waiting);
    }

    /// The oldest unit, waiting for one until `deadline`; [Popped::Ended] once closed and empty.
    pub(crate) fn pop(&self, deadline: Instant) -> Popped<U> {
        let mut s = lock(&self.state);
        loop {
            if let Some((unit, bytes)) = s.units.pop_front() {
                return Popped::Unit(unit, bytes);
            }
            if s.closed {
                return Popped::Ended;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || core::mem::take(&mut s.woken) {
                return Popped::Timeout;
            }
            #[expect(clippy::expect_used, reason = "as in lock")]
            let (g, _) = self
                .work
                .wait_timeout(s, left)
                .expect("a mux lock poisoned by a panic");
            s = g;
        }
    }

    /// Wake the taker waiting for a unit: its wait ends as at its deadline, so it takes a nearer one.
    pub(crate) fn wake(&self) {
        lock(&self.state).woken = true;
        self.work.notify_one();
    }

    /// Give back the `bytes` of a unit placed.
    pub(crate) fn release(&self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let mut s = lock(&self.state);
        s.queued = s.queued.saturating_sub(bytes);
        let first = s.first();
        drop(s);
        first.into_iter().for_each(Waker::wake);
    }

    /// Bytes of the units sent and not yet placed, senders that waited for room so far, and those waiting now.
    pub(crate) fn levels(&self) -> (u64, u64, u64) {
        let s = lock(&self.state);
        let waiting = u64::try_from(s.waiters.len()).unwrap_or(u64::MAX);
        (s.queued, s.waits, waiting)
    }
}

/// A sender's wait for room ([Queue::push_wait]).
pub(crate) struct PushWait<'a, U> {
    queue: &'a Queue<U>,
    slot: &'a mut Option<U>,
    bytes: u64,
    /// Its place in line, once it waits.
    ticket: Option<u64>,
}

impl<U> Future for PushWait<'_, U> {
    type Output = Pushed;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Pushed> {
        let this = self.get_mut();
        let q = this.queue;
        let mut s = lock(&q.state);
        if s.closed {
            if let Some(t) = this.ticket.take() {
                s.waiters.retain(|w| w.ticket != t);
            }
            return Poll::Ready(Pushed::Closed);
        }
        let first = s.waiters.front().map(|w| w.ticket);
        let served = match this.ticket {
            None => first.is_none(),
            Some(t) => first == Some(t),
        };
        if served && q.fits(&s, this.bytes) {
            if this.ticket.take().is_some() {
                s.waiters.pop_front();
            }
            // the next waiter may fit too
            let next = s.first();
            if let Some(unit) = this.slot.take() {
                q.append(s, unit, this.bytes);
            }
            next.into_iter().for_each(Waker::wake);
            return Poll::Ready(Pushed::Queued);
        }
        match this.ticket {
            None => {
                let ticket = s.next_ticket;
                s.next_ticket = s.next_ticket.wrapping_add(1);
                s.waits = s.waits.saturating_add(1);
                s.waiters.push_back(Waiter {
                    ticket,
                    waker: Some(cx.waker().clone()),
                });
                this.ticket = Some(ticket);
            }
            Some(t) => {
                if let Some(w) = s.waiters.iter_mut().find(|w| w.ticket == t) {
                    w.waker = Some(cx.waker().clone());
                }
            }
        }
        Poll::Pending
    }
}

impl<U> Drop for PushWait<'_, U> {
    /// A sender that leaves the line while it waits lets the next one in.
    fn drop(&mut self) {
        let Some(t) = self.ticket else {
            return;
        };
        let mut s = lock(&self.queue.state);
        let was_first = s.waiters.front().is_some_and(|w| w.ticket == t);
        s.waiters.retain(|w| w.ticket != t);
        let next = if was_first { s.first() } else { None };
        drop(s);
        next.into_iter().for_each(Waker::wake);
    }
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::panic,
        clippy::arithmetic_side_effects,
        reason = "tests fail by panicking"
    )]

    use super::*;
    use crate::wait::block_on;
    use std::sync::Arc;
    use std::time::Duration;

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    /// Push `unit` without waiting: what became of it, and the unit when it wasn't queued.
    fn push(q: &Queue<u32>, unit: u32, bytes: u64) -> (Pushed, Option<u32>) {
        let mut slot = Some(unit);
        (q.try_push(&mut slot, bytes), slot)
    }

    /// Push `unit`, waiting for room on this thread.
    fn push_waiting(q: &Queue<u32>, unit: u32, bytes: u64) -> (Pushed, Option<u32>) {
        let mut slot = Some(unit);
        (block_on(q.push_wait(&mut slot, bytes)), slot)
    }

    fn waiting(q: &Queue<u32>) -> usize {
        lock(&q.state).waiters.len()
    }

    #[test]
    fn a_unit_larger_than_the_budget_goes_alone() {
        let q = Queue::new(10);
        assert!(matches!(push(&q, 1, 100), (Pushed::Queued, None)));
        assert!(matches!(push(&q, 2, 1), (Pushed::Full, Some(2))));
        let Popped::Unit(1, 100) = q.pop(soon()) else {
            panic!("unit 1")
        };
        q.release(100);
        assert!(matches!(push(&q, 2, 1), (Pushed::Queued, None)));
    }

    /// A sender that finds others waiting waits behind them, even when its unit would fit.
    #[test]
    fn waiters_are_served_in_arrival_order() {
        let q = Arc::new(Queue::new(10));
        assert!(matches!(push(&q, 0, 10), (Pushed::Queued, None)));
        let big = {
            let q = q.clone();
            std::thread::spawn(move || push_waiting(&q, 1, 8))
        };
        while waiting(&q) == 0 {
            std::thread::yield_now();
        }
        // room for 0 bytes: a sender that won't wait is refused while one waits
        assert!(matches!(push(&q, 2, 0), (Pushed::Full, Some(2))));
        let Popped::Unit(0, 10) = q.pop(soon()) else {
            panic!("unit 0")
        };
        q.release(10);
        assert!(matches!(big.join().unwrap(), (Pushed::Queued, None)));
        let Popped::Unit(1, 8) = q.pop(soon()) else {
            panic!("unit 1")
        };
    }

    #[test]
    fn a_closed_queue_hands_waiters_their_units_back() {
        let q = Arc::new(Queue::new(1));
        assert!(matches!(push(&q, 0, 1), (Pushed::Queued, None)));
        let waiter = {
            let q = q.clone();
            std::thread::spawn(move || push_waiting(&q, 1, 1))
        };
        while waiting(&q) == 0 {
            std::thread::yield_now();
        }
        assert!(q.push_control(9, true).is_ok());
        assert!(matches!(waiter.join().unwrap(), (Pushed::Closed, Some(1))));
        assert!(matches!(q.push_control(10, false), Err(10)));
        let Popped::Unit(0, 1) = q.pop(soon()) else {
            panic!("unit 0")
        };
        let Popped::Unit(9, 0) = q.pop(soon()) else {
            panic!("the last unit")
        };
    }

    /// A waiter dropped while first in line keeps its unit and lets the one behind it in.
    #[test]
    fn a_waiter_that_leaves_lets_the_next_in() {
        let q = Queue::new(10);
        assert!(matches!(push(&q, 0, 10), (Pushed::Queued, None)));
        let mut first = Some(1);
        let mut second = Some(2);
        let mut cx = Context::from_waker(Waker::noop());
        let mut a = q.push_wait(&mut first, 5);
        let mut b = q.push_wait(&mut second, 5);
        assert!(Pin::new(&mut a).poll(&mut cx).is_pending());
        assert!(Pin::new(&mut b).poll(&mut cx).is_pending());
        assert_eq!(waiting(&q), 2);
        drop(a);
        let Popped::Unit(0, 10) = q.pop(soon()) else {
            panic!("unit 0")
        };
        q.release(10);
        assert!(matches!(
            Pin::new(&mut b).poll(&mut cx),
            Poll::Ready(Pushed::Queued)
        ));
        drop(b);
        assert_eq!(first, Some(1));
        assert_eq!(second, None);
        assert_eq!(waiting(&q), 0);
    }
}
