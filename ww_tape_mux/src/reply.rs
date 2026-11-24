//! A reply from the writing thread to one caller waiting for it, by thread or by task.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use crate::queue::lock;

struct Slot<T> {
    value: Option<T>,
    /// The sending side is gone: sent, or dropped unsent (the writing thread stopped).
    done: bool,
    /// The caller waiting.
    waker: Option<Waker>,
}

/// The writing thread's side: [Self::send] once; dropped unsent, the caller wakes with nothing.
pub(crate) struct ReplyTx<T>(Arc<Mutex<Slot<T>>>);

/// The caller's side: a future of the reply, None when the writing thread stopped without one.
pub(crate) struct ReplyRx<T>(Arc<Mutex<Slot<T>>>);

/// A reply's two sides.
pub(crate) fn reply<T>() -> (ReplyTx<T>, ReplyRx<T>) {
    let slot = Arc::new(Mutex::new(Slot {
        value: None,
        done: false,
        waker: None,
    }));
    (ReplyTx(slot.clone()), ReplyRx(slot))
}

impl<T> ReplyTx<T> {
    pub(crate) fn send(self, value: T) {
        self.finish(Some(value));
    }

    fn finish(&self, value: Option<T>) {
        let mut s = lock(&self.0);
        if s.done {
            return;
        }
        s.value = value;
        s.done = true;
        let waker = s.waker.take();
        drop(s);
        waker.into_iter().for_each(Waker::wake);
    }
}

impl<T> Drop for ReplyTx<T> {
    fn drop(&mut self) {
        self.finish(None);
    }
}

impl<T> ReplyRx<T> {
    /// The reply once the sending side is gone, else pending; never waits. Taken once: a later call gives
    /// `Ready(None)`.
    pub(crate) fn try_take(&mut self) -> Poll<Option<T>> {
        let mut s = lock(&self.0);
        if s.done {
            Poll::Ready(s.value.take())
        } else {
            Poll::Pending
        }
    }
}

impl<T> Future for ReplyRx<T> {
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let mut s = lock(&self.0);
        if s.done {
            return Poll::Ready(s.value.take());
        }
        s.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}
