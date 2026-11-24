//! Waiting for a chain's files to change: inotify on Linux, else the poll interval alone.
//!
//! A thread waits in [Wake::wait]. A task waits in [Wake::changed], a future woken by a thread of the follower's
//! own that does the same wait: no async runtime is needed, and none is blocked.

use std::future::Future;
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};
use std::thread::JoinHandle;
use std::time::Duration;

#[cfg(target_os = "linux")]
use rustix::fd::OwnedFd;

/// Wakes a follower when a file of its directory changed, or after a poll interval at most.
pub(crate) struct Wake {
    #[cfg(target_os = "linux")]
    inotify: Option<Arc<OwnedFd>>,
    /// The thread that waits for a task, started at the first [Self::changed].
    task: Option<TaskWait>,
}

impl Wake {
    /// A wake for changes in `dir`. Without inotify (another system, or `dir` can't be watched) it waits out the
    /// interval it is given.
    pub(crate) fn new(dir: &Path) -> Self {
        Self {
            #[cfg(target_os = "linux")]
            inotify: inotify(dir).map(Arc::new),
            task: None,
        }
    }

    /// Whether a change wakes it before the interval is up.
    pub(crate) fn watches(&self) -> bool {
        #[cfg(target_os = "linux")]
        return self.inotify.is_some();
        #[cfg(not(target_os = "linux"))]
        false
    }

    /// Return once a file of the directory changed, or `at_most` passed.
    pub(crate) fn wait(&mut self, at_most: Duration) {
        #[cfg(target_os = "linux")]
        if let Some(fd) = &self.inotify {
            wait_inotify(fd, None, at_most);
            return;
        }
        std::thread::sleep(at_most);
    }

    /// [Self::wait] as a future: ready once a file of the directory changed, or `at_most` passed. Dropped before,
    /// it takes nothing.
    pub(crate) fn changed(&mut self, at_most: Duration) -> Changed<'_> {
        Changed {
            wake: self,
            at_most,
            armed: false,
        }
    }

    /// The task's waiting thread, started now when it isn't yet.
    fn task_wait(&mut self) -> io::Result<&TaskWait> {
        let task = match self.task.take() {
            Some(t) => t,
            None => TaskWait::start(
                #[cfg(target_os = "linux")]
                self.inotify.clone(),
            )?,
        };
        Ok(self.task.insert(task))
    }
}

/// A task's wait for a change ([Wake::changed]).
pub(crate) struct Changed<'a> {
    wake: &'a mut Wake,
    at_most: Duration,
    /// Whether it asked the waiting thread to wait.
    armed: bool,
}

impl Future for Changed<'_> {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let at_most = this.at_most;
        let task = match this.wake.task_wait() {
            Ok(t) => t,
            Err(e) => return Poll::Ready(Err(e)),
        };
        let mut s = lock(&task.shared.state);
        if this.armed && s.fired {
            s.fired = false;
            return Poll::Ready(Ok(()));
        }
        s.waker = Some(cx.waker().clone());
        if !this.armed {
            // a change the thread saw for a wait dropped before is older than this one's look at the files
            s.fired = false;
            s.at_most = at_most;
            s.armed = true;
            this.armed = true;
            drop(s);
            task.shared.ask.notify_one();
        }
        Poll::Pending
    }
}

/// What the waiting thread and the task share.
struct TaskState {
    /// The task to wake.
    waker: Option<Waker>,
    /// How long the thread waits at most.
    at_most: Duration,
    /// A task waits: the thread is to wait for a change.
    armed: bool,
    /// The thread's wait ended since the task armed it.
    fired: bool,
    /// The follower is gone: the thread ends.
    stop: bool,
}

struct TaskShared {
    state: Mutex<TaskState>,
    /// The thread waiting to be armed or stopped.
    ask: Condvar,
}

/// The thread that waits for a follower's task, and how to stop it.
struct TaskWait {
    shared: Arc<TaskShared>,
    thread: Option<JoinHandle<()>>,
    /// The write end of a pipe the thread watches beside inotify: closed, it ends the thread's wait at once.
    #[cfg(target_os = "linux")]
    stop: Option<OwnedFd>,
}

impl TaskWait {
    fn start(#[cfg(target_os = "linux")] inotify: Option<Arc<OwnedFd>>) -> io::Result<Self> {
        let shared = Arc::new(TaskShared {
            state: Mutex::new(TaskState {
                waker: None,
                at_most: Duration::ZERO,
                armed: false,
                fired: false,
                stop: false,
            }),
            ask: Condvar::new(),
        });
        let theirs = shared.clone();
        #[cfg(target_os = "linux")]
        let (watch, stop) = match inotify {
            Some(fd) => {
                use rustix::pipe::{PipeFlags, pipe_with};
                let (rx, tx) = pipe_with(PipeFlags::CLOEXEC)?;
                (Some((fd, rx)), Some(tx))
            }
            None => (None, None),
        };
        let thread = std::thread::Builder::new()
            .name("ww_tape_follow".to_owned())
            .spawn(move || {
                run(
                    &theirs,
                    #[cfg(target_os = "linux")]
                    watch,
                );
            })?;
        Ok(Self {
            shared,
            thread: Some(thread),
            #[cfg(target_os = "linux")]
            stop,
        })
    }
}

impl Drop for TaskWait {
    fn drop(&mut self) {
        lock(&self.shared.state).stop = true;
        self.shared.ask.notify_one();
        #[cfg(target_os = "linux")]
        drop(self.stop.take());
        if let Some(t) = self.thread.take() {
            // the thread's wait ends at once: stopped above. A panic in it is not raised again: a drop doesn't panic
            drop(t.join());
        }
    }
}

/// The waiting thread: each time a task arms it, wait for a change, then wake the task.
#[cfg_attr(
    target_os = "linux",
    expect(
        clippy::needless_pass_by_value,
        reason = "the thread owns the watch's descriptors: they close when it exits"
    )
)]
fn run(shared: &TaskShared, #[cfg(target_os = "linux")] watch: Option<(Arc<OwnedFd>, OwnedFd)>) {
    loop {
        let at_most = {
            let mut s = lock(&shared.state);
            while !s.armed && !s.stop {
                #[expect(clippy::expect_used, reason = "as in lock")]
                let g = shared
                    .ask
                    .wait(s)
                    .expect("a follower lock poisoned by a panic");
                s = g;
            }
            if s.stop {
                return;
            }
            s.at_most
        };
        #[cfg(target_os = "linux")]
        if let Some((fd, stop)) = &watch {
            wait_inotify(fd, Some(stop), at_most);
        } else {
            wait_stop(shared, at_most);
        }
        #[cfg(not(target_os = "linux"))]
        wait_stop(shared, at_most);
        let mut s = lock(&shared.state);
        s.armed = false;
        s.fired = true;
        let waker = s.waker.take();
        let stop = s.stop;
        drop(s);
        waker.into_iter().for_each(Waker::wake);
        if stop {
            return;
        }
    }
}

/// Wait `at_most`, or until the follower is gone.
fn wait_stop(shared: &TaskShared, at_most: Duration) {
    let s = lock(&shared.state);
    #[expect(clippy::expect_used, reason = "as in lock")]
    drop(
        shared
            .ask
            .wait_timeout_while(s, at_most, |s| !s.stop)
            .expect("a follower lock poisoned by a panic"),
    );
}

/// `m` locked; a lock poisoned by a panic on another thread panics here too.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    #[expect(
        clippy::expect_used,
        reason = "a poisoned lock means the waiting thread or a task panicked holding it: the follower stops"
    )]
    m.lock().expect("a follower lock poisoned by a panic")
}

/// Wait until `fd` reports a change (then read its events), `stop` is readable or closed, or `at_most` passed.
#[cfg(target_os = "linux")]
fn wait_inotify(fd: &OwnedFd, stop: Option<&OwnedFd>, at_most: Duration) {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    let timeout = Timespec::try_from(at_most).ok();
    let mut fds = vec![PollFd::new(fd, PollFlags::IN)];
    if let Some(s) = stop {
        fds.push(PollFd::new(s, PollFlags::IN));
    }
    // an interrupted or failed poll wakes early: the follower reads again and waits again
    if poll(&mut fds, timeout.as_ref()).is_ok_and(|n| n > 0) {
        drain(fd);
    }
}

/// Bytes of inotify events read at once.
#[cfg(target_os = "linux")]
const EVENTS_BUF: usize = 4096;

/// An inotify watch on `dir` for files created, written, cut back, renamed into it; None when one can't be had.
#[cfg(target_os = "linux")]
fn inotify(dir: &Path) -> Option<OwnedFd> {
    use rustix::fs::inotify::{CreateFlags, WatchFlags, add_watch, init};
    let fd = init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK).ok()?;
    let flags = WatchFlags::CREATE
        | WatchFlags::MODIFY
        | WatchFlags::CLOSE_WRITE
        | WatchFlags::MOVED_TO
        | WatchFlags::ATTRIB;
    add_watch(&fd, dir, flags).ok()?;
    Some(fd)
}

/// Read the events waiting, which say only that something changed.
#[cfg(target_os = "linux")]
fn drain(fd: &OwnedFd) {
    let mut buf = [0u8; EVENTS_BUF];
    while rustix::io::read(fd, &mut buf).is_ok_and(|n| n > 0) {}
}
