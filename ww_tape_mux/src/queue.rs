//! The mux's queue: units in the order they came, bounded by the bytes they hold, waiters served in arrival order.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Instant;

/// `m` locked; a lock poisoned by a panic on another thread panics here too.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    #[expect(
        clippy::expect_used,
        reason = "a poisoned lock means a thread panicked inside the mux: the mux stops, as the rules for locks say"
    )]
    m.lock().expect("a mux lock poisoned by a panic")
}

/// Wait on `c` with `g`; poisoned as [lock].
pub(crate) fn wait<'a, T>(c: &Condvar, g: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
    #[expect(clippy::expect_used, reason = "as in lock")]
    c.wait(g).expect("a mux lock poisoned by a panic")
}

/// What became of a unit sent.
pub(crate) enum Pushed<U> {
    /// In the queue.
    Queued,
    /// No room, and the sender would not wait: the unit back.
    Full(U),
    /// The queue takes no more units: the unit back.
    Closed(U),
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

struct State<U> {
    units: VecDeque<(U, u64)>,
    /// Bytes of the units sent and not yet placed.
    queued: u64,
    /// No more units: closed or stopped.
    closed: bool,
    /// The next ticket a waiting sender takes, and the ticket served next: equal when nobody waits.
    next_ticket: u64,
    serving: u64,
    /// Senders that waited for room, counted when they start to wait.
    waits: u64,
}

/// A queue of units bounded by bytes, for many senders and one taker.
pub(crate) struct Queue<U> {
    state: Mutex<State<U>>,
    budget: u64,
    /// Senders waiting for room.
    room: Condvar,
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
                next_ticket: 0,
                serving: 0,
                waits: 0,
            }),
            budget,
            room: Condvar::new(),
            work: Condvar::new(),
        }
    }

    /// Whether `bytes` more fit: within the budget, or the queue is empty, so a unit larger than the budget still
    /// goes alone.
    fn fits(&self, s: &State<U>, bytes: u64) -> bool {
        s.queued == 0 || s.queued.saturating_add(bytes) <= self.budget
    }

    /// Append `unit` of `bytes`. With `wait`, block until it fits behind the senders that waited before; else hand
    /// it back when it doesn't fit now or others wait.
    pub(crate) fn push(&self, unit: U, bytes: u64, wait_for_room: bool) -> Pushed<U> {
        let mut s = lock(&self.state);
        if s.closed {
            return Pushed::Closed(unit);
        }
        let nobody_waits = s.next_ticket == s.serving;
        if nobody_waits && self.fits(&s, bytes) {
            self.append(s, unit, bytes);
            return Pushed::Queued;
        }
        if !wait_for_room {
            return Pushed::Full(unit);
        }
        let ticket = s.next_ticket;
        s.next_ticket = s.next_ticket.wrapping_add(1);
        s.waits = s.waits.saturating_add(1);
        while !s.closed && !(s.serving == ticket && self.fits(&s, bytes)) {
            s = wait(&self.room, s);
        }
        if s.closed {
            return Pushed::Closed(unit);
        }
        s.serving = s.serving.wrapping_add(1);
        self.append(s, unit, bytes);
        // the next waiter may fit too
        self.room.notify_all();
        Pushed::Queued
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
        drop(s);
        self.work.notify_one();
        if last {
            self.room.notify_all();
        }
        Ok(())
    }

    /// Take no more units; senders waiting for room get theirs back. Units queued stay to be taken.
    pub(crate) fn close(&self) {
        lock(&self.state).closed = true;
        self.room.notify_all();
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
            if left.is_zero() {
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

    /// Give back the `bytes` of a unit placed.
    pub(crate) fn release(&self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let mut s = lock(&self.state);
        s.queued = s.queued.saturating_sub(bytes);
        drop(s);
        self.room.notify_all();
    }

    /// Bytes of the units sent and not yet placed, senders that waited for room so far, and those waiting now.
    pub(crate) fn levels(&self) -> (u64, u64, u64) {
        let s = lock(&self.state);
        (s.queued, s.waits, s.next_ticket.wrapping_sub(s.serving))
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
    use std::sync::Arc;
    use std::time::Duration;

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    #[test]
    fn a_unit_larger_than_the_budget_goes_alone() {
        let q = Queue::new(10);
        assert!(matches!(q.push(1, 100, false), Pushed::Queued));
        assert!(matches!(q.push(2, 1, false), Pushed::Full(2)));
        let Popped::Unit(1, 100) = q.pop(soon()) else {
            panic!("unit 1")
        };
        q.release(100);
        assert!(matches!(q.push(2, 1, false), Pushed::Queued));
    }

    /// A sender that finds others waiting waits behind them, even when its unit would fit.
    #[test]
    fn waiters_are_served_in_arrival_order() {
        let q = Arc::new(Queue::new(10));
        assert!(matches!(q.push(0, 10, false), Pushed::Queued));
        let big = {
            let q = q.clone();
            std::thread::spawn(move || q.push(1, 8, true))
        };
        while lock(&q.state).next_ticket == 0 {
            std::thread::yield_now();
        }
        // room for 0 bytes: a sender that won't wait is refused while one waits
        assert!(matches!(q.push(2, 0, false), Pushed::Full(2)));
        let Popped::Unit(0, 10) = q.pop(soon()) else {
            panic!("unit 0")
        };
        q.release(10);
        assert!(matches!(big.join().unwrap(), Pushed::Queued));
        let Popped::Unit(1, 8) = q.pop(soon()) else {
            panic!("unit 1")
        };
    }

    #[test]
    fn a_closed_queue_hands_waiters_their_units_back() {
        let q = Arc::new(Queue::new(1));
        assert!(matches!(q.push(0, 1, false), Pushed::Queued));
        let waiter = {
            let q = q.clone();
            std::thread::spawn(move || q.push(1, 1, true))
        };
        while lock(&q.state).next_ticket == 0 {
            std::thread::yield_now();
        }
        assert!(q.push_control(9, true).is_ok());
        assert!(matches!(waiter.join().unwrap(), Pushed::Closed(1)));
        assert!(matches!(q.push_control(10, false), Err(10)));
        let Popped::Unit(0, 1) = q.pop(soon()) else {
            panic!("unit 0")
        };
        let Popped::Unit(9, 0) = q.pop(soon()) else {
            panic!("the last unit")
        };
    }
}
