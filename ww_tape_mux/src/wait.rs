//! Waiting on the mux's futures from a plain thread: each blocking call runs its async twin to the end, parked
//! between polls. One way of waiting serves both, so a sync and an async waiter take their turns in one order.

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread::Thread;
use std::time::Instant;

/// Unparks the thread that waits.
struct Unpark(Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

thread_local! {
    // owned by this module: the waker of a thread's blocking calls, made once per thread instead of once per call
    static WAKER: Waker = this_thread();
}

fn this_thread() -> Waker {
    Waker::from(Arc::new(Unpark(std::thread::current())))
}

/// Run `f` with the calling thread's waker; a new one while the thread's locals are destroyed.
fn with_waker<R>(f: impl FnOnce(&Waker) -> R) -> R {
    match WAKER.try_with(Waker::clone) {
        Ok(w) => f(&w),
        Err(_) => f(&this_thread()),
    }
}

/// `f`'s output, the thread parked while it is pending.
pub(crate) fn block_on<F: Future>(f: F) -> F::Output {
    let mut f = pin!(f);
    with_waker(|w| {
        let mut cx = Context::from_waker(w);
        loop {
            if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
                return v;
            }
            std::thread::park();
        }
    })
}

/// `f`'s output, or None when `deadline` came first; `f` is dropped then, so it must be cancel-safe.
pub(crate) fn block_on_until<F: Future>(f: F, deadline: Instant) -> Option<F::Output> {
    let mut f = pin!(f);
    with_waker(|w| {
        let mut cx = Context::from_waker(w);
        loop {
            if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
                return Some(v);
            }
            let left = deadline.checked_duration_since(Instant::now())?;
            if left.is_zero() {
                return None;
            }
            std::thread::park_timeout(left);
        }
    })
}

/// Wake every waker of `wakers`, after the lock that held them is released.
pub(crate) fn wake_all(wakers: Vec<Waker>) {
    wakers.into_iter().for_each(Waker::wake);
}

/// Keep `w` in `wakers` unless it would wake the same task as one there already.
pub(crate) fn register(wakers: &mut Vec<Waker>, w: &Waker) {
    if !wakers.iter().any(|k| k.will_wake(w)) {
        wakers.push(w.clone());
    }
}
