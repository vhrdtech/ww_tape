//! [FileLock]: one writer per file, by a lock file beside it that only writers open.
//!
//! The lock of the file `NAME` is held on `NAME.lock`, never on `NAME` or `NAME.partial` themselves:
//!
//! - On unix it is a POSIX record lock (`fcntl(F_SETLK)`), which belongs to the process. A child forked while a
//!   writer holds it inherits the open lock file but not the lock, so the writer's resume right after it lets go is
//!   never refused because a child has not yet run `exec` (an `flock` belongs to the open file description, which
//!   the child shares).
//! - A record lock goes away when its process closes any descriptor of the file. Readers open `NAME` and
//!   `NAME.partial` and close them as they like; nobody but the lock holder opens `NAME.lock`.
//! - A record lock doesn't keep apart two writers of one process, so a table of the lock files this process holds
//!   does. It is checked before the lock file is opened, so a refused writer never opens (and closes) a lock file
//!   the process holds.
//! - The holder removes the lock file when it lets go, while it still holds the lock. A writer that opened the file
//!   before it was removed takes the lock on a file no longer named `NAME.lock`, sees so and opens the new one
//!   ([LOCK_TRIES_MAX]). A lock file a crash left holds no lock and is taken as it is.
//!
//! Elsewhere (Windows) the lock file holds the file system's lock ([File::try_lock]): a child process gets no
//! handle there unless the parent passes it.
//!
//! A name that is there and isn't a regular file (a device such as a raw SD card, whose directory takes no lock file)
//! is locked itself, by the file system's lock on a handle of its own: a child forked while it is held holds it too,
//! until it runs `exec`.

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::fs::{FsError, names};

/// How often a file sink opens its lock file again after a holder removed the one it opened: each time is a
/// holder that let go meanwhile.
pub const LOCK_TRIES_MAX: usize = 8;

/// The lock files this process holds, by their absolute path: a record lock doesn't keep apart two writers of one
/// process. Owned by [FileLock]: taken in [FileLock::take], given back in its drop.
static HELD: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());

/// The exclusive lock of a file, held until dropped, see the [module docs](self).
#[derive(Debug)]
pub(crate) struct FileLock {
    /// The open lock file; taken in the drop, so it closes before the entry goes
    file: Option<File>,
    /// Its name: removed when let go, unless the lock is on the file itself (a device)
    path: Option<PathBuf>,
    /// Its entry in [HELD]
    key: PathBuf,
}

/// `NAME.lock` next to `NAME`.
pub fn lock_name(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

impl FileLock {
    /// The lock of the file `name` (its final name, also while it is written as `NAME.partial`):
    /// [FsError::Locked] while another writer, of this process or another, holds it.
    pub(crate) fn take(name: &Path) -> Result<Self, FsError> {
        if std::fs::metadata(name).is_ok_and(|m| !m.is_file() && !m.is_dir()) {
            return Self::take_itself(name);
        }
        let path = lock_name(name);
        let key = absolute(&path).map_err(io_err("finding the lock file"))?;
        let entry = Entry::claim(key, name)?;
        for _ in 0..LOCK_TRIES_MAX {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .map_err(io_err("opening the lock file"))?;
            if !try_lock(&file).map_err(io_err("locking the lock file"))? {
                return Err(FsError::Locked(name.to_path_buf()));
            }
            if names(&path, &file).map_err(io_err("checking the lock file's name"))? {
                return Ok(FileLock {
                    file: Some(file),
                    path: Some(path),
                    key: entry.keep(),
                });
            }
            // its holder removed it as it let go: the lock is on a file nobody else finds
        }
        Err(FsError::Locked(name.to_path_buf()))
    }

    /// The lock of `name`, not a regular file, on the file itself.
    fn take_itself(name: &Path) -> Result<Self, FsError> {
        let key = absolute(name).map_err(io_err("finding the file to lock"))?;
        let entry = Entry::claim(key, name)?;
        let file = File::open(name).map_err(io_err("opening the file to lock"))?;
        match file.try_lock() {
            Ok(()) => Ok(FileLock {
                file: Some(file),
                path: None,
                key: entry.keep(),
            }),
            Err(std::fs::TryLockError::WouldBlock) => Err(FsError::Locked(name.to_path_buf())),
            Err(std::fs::TryLockError::Error(error)) => Err(FsError::Io {
                what: "locking the file",
                error,
            }),
        }
    }
}

impl Drop for FileLock {
    /// Remove the lock file while it is held, then close it (which lets the lock go) and give back the entry.
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            if let Some(path) = &self.path
                && matches!(names(path, &file), Ok(true))
            {
                // a lock file left behind holds no lock: the next writer takes it as it is
                _ = std::fs::remove_file(path);
            }
            // closed before the entry goes: a writer of this process that opened the same file and closed it
            // after taking it would let its lock go
            drop(file);
        }
        held().remove(&self.key);
    }
}

/// An entry of [HELD] taken for a lock not yet held: given back when dropped unless kept.
struct Entry {
    key: PathBuf,
    kept: bool,
}

impl Entry {
    /// The entry `key` of the lock of `name`; [FsError::Locked] when this process holds it.
    fn claim(key: PathBuf, name: &Path) -> Result<Self, FsError> {
        if !held().insert(key.clone()) {
            return Err(FsError::Locked(name.to_path_buf()));
        }
        Ok(Entry { key, kept: false })
    }

    /// The entry, now owned by a [FileLock].
    fn keep(mut self) -> PathBuf {
        self.kept = true;
        core::mem::take(&mut self.key)
    }
}

impl Drop for Entry {
    fn drop(&mut self) {
        if !self.kept {
            held().remove(&self.key);
        }
    }
}

#[expect(
    clippy::expect_used,
    reason = "a poisoned lock panics: the table may be half changed"
)]
fn held() -> std::sync::MutexGuard<'static, BTreeSet<PathBuf>> {
    HELD.lock()
        .expect("a writer panicked while it took or gave back a file lock")
}

/// `path` with its directory resolved, so two names of one lock file are one entry.
fn absolute(path: &Path) -> io::Result<PathBuf> {
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    Ok(std::fs::canonicalize(dir)?.join(name))
}

fn io_err(what: &'static str) -> impl FnOnce(io::Error) -> FsError {
    move |error| FsError::Io { what, error }
}

/// Take the record lock on all of `file`: false when another process holds it.
#[cfg(unix)]
fn try_lock(file: &File) -> io::Result<bool> {
    use rustix::fs::{FlockOperation, fcntl_lock};
    use rustix::io::Errno;
    match fcntl_lock(file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(true),
        Err(e) if e == Errno::AGAIN || e == Errno::ACCESS => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Take the file system's lock on `file`: false when another handle holds it.
#[cfg(not(unix))]
fn try_lock(file: &File) -> io::Result<bool> {
    match file.try_lock() {
        Ok(()) => Ok(true),
        Err(std::fs::TryLockError::WouldBlock) => Ok(false),
        Err(std::fs::TryLockError::Error(e)) => Err(e),
    }
}
