//! A reply from the writing thread to one caller waiting for it.

use std::sync::{Arc, Condvar, Mutex};

use crate::queue::lock;

struct Slot<T> {
    value: Option<T>,
    /// The sending side is gone: sent, or dropped unsent (the writing thread stopped).
    done: bool,
}

struct Inner<T> {
    slot: Mutex<Slot<T>>,
    ready: Condvar,
}

/// The writing thread's side: [Self::send] once; dropped unsent, the caller wakes with nothing.
pub(crate) struct ReplyTx<T>(Arc<Inner<T>>);

/// The caller's side.
pub(crate) struct ReplyRx<T>(Arc<Inner<T>>);

/// A reply's two sides.
pub(crate) fn reply<T>() -> (ReplyTx<T>, ReplyRx<T>) {
    let inner = Arc::new(Inner {
        slot: Mutex::new(Slot {
            value: None,
            done: false,
        }),
        ready: Condvar::new(),
    });
    (ReplyTx(inner.clone()), ReplyRx(inner))
}

impl<T> ReplyTx<T> {
    pub(crate) fn send(self, value: T) {
        let mut s = lock(&self.0.slot);
        s.value = Some(value);
        s.done = true;
        drop(s);
        self.0.ready.notify_all();
    }
}

impl<T> Drop for ReplyTx<T> {
    fn drop(&mut self) {
        let mut s = lock(&self.0.slot);
        if !s.done {
            s.done = true;
            drop(s);
            self.0.ready.notify_all();
        }
    }
}

impl<T> ReplyRx<T> {
    /// The reply; None when the writing thread stopped without one.
    pub(crate) fn wait(self) -> Option<T> {
        let mut s = lock(&self.0.slot);
        while !s.done {
            #[expect(clippy::expect_used, reason = "as in crate::queue::lock")]
            let g = self
                .0
                .ready
                .wait(s)
                .expect("a mux lock poisoned by a panic");
            s = g;
        }
        s.value.take()
    }
}
