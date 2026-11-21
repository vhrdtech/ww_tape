//! Waiting for a chain's files to change: inotify on Linux, else the poll interval alone.

use std::path::Path;
use std::time::Duration;

/// Wakes a follower when a file of its directory changed, or after a poll interval at most.
pub(crate) struct Wake {
    #[cfg(target_os = "linux")]
    inotify: Option<rustix::fd::OwnedFd>,
}

impl Wake {
    /// A wake for changes in `dir`. Without inotify (another system, or `dir` can't be watched) it waits out the
    /// interval it is given.
    pub(crate) fn new(dir: &Path) -> Self {
        Self {
            #[cfg(target_os = "linux")]
            inotify: inotify(dir),
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
            use rustix::event::{PollFd, PollFlags, Timespec, poll};
            let timeout = Timespec::try_from(at_most).ok();
            let mut fds = [PollFd::new(fd, PollFlags::IN)];
            // an interrupted or failed poll wakes early: the follower reads again and waits again
            if poll(&mut fds, timeout.as_ref()).is_ok_and(|n| n > 0) {
                drain(fd);
            }
            return;
        }
        std::thread::sleep(at_most);
    }
}

/// Bytes of inotify events read at once.
#[cfg(target_os = "linux")]
const EVENTS_BUF: usize = 4096;

/// An inotify watch on `dir` for files created, written, cut back, renamed into it; None when one can't be had.
#[cfg(target_os = "linux")]
fn inotify(dir: &Path) -> Option<rustix::fd::OwnedFd> {
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
fn drain(fd: &rustix::fd::OwnedFd) {
    let mut buf = [0u8; EVENTS_BUF];
    while rustix::io::read(fd, &mut buf).is_ok_and(|n| n > 0) {}
}
