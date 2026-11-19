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
//! - One file has one lock whatever name reaches it. The lock file is named after the file's canonical name, with
//!   links resolved and a `.partial` suffix dropped, so `NAME`, a link to it and `NAME.partial` share `NAME.lock`. A
//!   hard link is another name of the same file that no name tells apart: the table also keys each file a sink
//!   opened by its device and inode, so two writers of one process are refused through hard links too. Two
//!   processes writing one file through two hard-linked names are not refused: both append to the same bytes, and
//!   their blocks interleave into a file that reads as damaged. A lock on the file itself would catch that, but
//!   every kind either passes to a forked child (`flock`, open file description locks) or goes away when a reader
//!   of the holder's process closes the file (record locks).
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

use super::fs::{FsError, names, partial_name};

/// How often a file sink opens its lock file again after a holder removed the one it opened: each time is a
/// holder that let go meanwhile.
pub const LOCK_TRIES_MAX: usize = 8;

/// The files this process holds the lock of: a record lock doesn't keep apart two writers of one process. Owned by
/// [FileLock]: taken in [FileLock::take] and [FileLock::bind], given back in its drop.
static HELD: Mutex<BTreeSet<Held>> = Mutex::new(BTreeSet::new());

/// An entry of [HELD].
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Held {
    /// A lock file, by its absolute path
    Name(PathBuf),
    /// A file a sink opened, by its device and inode
    File { dev: u64, ino: u64 },
}

/// The exclusive lock of a file, held until dropped, see the [module docs](self).
#[derive(Debug)]
pub(crate) struct FileLock {
    /// The open lock files; taken in the drop, so they close before the entries go
    files: Vec<LockFile>,
    /// Its entries in [HELD]
    keys: Vec<Held>,
}

/// One lock file held.
#[derive(Debug)]
struct LockFile {
    file: File,
    /// Its name: removed when let go, unless the lock is on the file itself (a device)
    path: Option<PathBuf>,
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
        let mut lock = FileLock {
            files: Vec::new(),
            keys: Vec::new(),
        };
        // the names `name` and its `.partial` stand for; one unless a link points elsewhere
        let mut keys = BTreeSet::new();
        keys.insert(lock_key(name).map_err(io_err("finding the file to lock"))?);
        let partial = partial_name(name);
        if std::fs::symlink_metadata(&partial).is_ok() {
            keys.insert(lock_key(&partial).map_err(io_err("finding the file to lock"))?);
        }
        for key in keys {
            let path = lock_name(&key);
            let entry = Entry::claim(Held::Name(path.clone()), name)?;
            let file = Self::lock_file(&path, name)?;
            lock.keys.push(entry.keep());
            lock.files.push(LockFile {
                file,
                path: Some(path),
            });
        }
        Ok(lock)
    }

    /// The lock file `path` of `name`, opened and locked.
    fn lock_file(path: &Path, name: &Path) -> Result<File, FsError> {
        for _ in 0..LOCK_TRIES_MAX {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)
                .map_err(io_err("opening the lock file"))?;
            if !try_lock(&file).map_err(io_err("locking the lock file"))? {
                return Err(FsError::Locked(name.to_path_buf()));
            }
            if names(path, &file).map_err(io_err("checking the lock file's name"))? {
                return Ok(file);
            }
            // its holder removed it as it let go: the lock is on a file nobody else finds
        }
        Err(FsError::Locked(name.to_path_buf()))
    }

    /// The lock of `name`, not a regular file, on the file itself.
    fn take_itself(name: &Path) -> Result<Self, FsError> {
        let key = absolute(name).map_err(io_err("finding the file to lock"))?;
        let entry = Entry::claim(Held::Name(key), name)?;
        let file = File::open(name).map_err(io_err("opening the file to lock"))?;
        match file.try_lock() {
            Ok(()) => Ok(FileLock {
                files: vec![LockFile { file, path: None }],
                keys: vec![entry.keep()],
            }),
            Err(std::fs::TryLockError::WouldBlock) => Err(FsError::Locked(name.to_path_buf())),
            Err(std::fs::TryLockError::Error(error)) => Err(FsError::Io {
                what: "locking the file",
                error,
            }),
        }
    }

    /// Hold `file`, opened as `name` under this lock, by its identity too: [FsError::Locked] when another writer of
    /// this process holds it under another name (a hard link).
    pub(crate) fn bind(&mut self, file: &File, name: &Path) -> Result<(), FsError> {
        if let Some(id) = identity(file).map_err(io_err("reading the file's identity"))? {
            let entry = Entry::claim(id, name)?;
            self.keys.push(entry.keep());
        }
        Ok(())
    }
}

impl Drop for FileLock {
    /// Remove the lock files while they are held, then close them (which lets the locks go) and give back the
    /// entries.
    fn drop(&mut self) {
        for LockFile { file, path } in self.files.drain(..) {
            if let Some(path) = &path
                && matches!(names(path, &file), Ok(true))
            {
                // a lock file left behind holds no lock: the next writer takes it as it is
                _ = std::fs::remove_file(path);
            }
            // closed before the entry goes: a writer of this process that opened the same file and closed it
            // after taking it would let its lock go
            drop(file);
        }
        let mut held = held();
        for key in &self.keys {
            held.remove(key);
        }
    }
}

/// An entry of [HELD] taken for a lock not yet held: given back when dropped unless kept.
struct Entry {
    key: Held,
    kept: bool,
}

impl Entry {
    /// The entry `key` of the lock of `name`; [FsError::Locked] when this process holds it.
    fn claim(key: Held, name: &Path) -> Result<Self, FsError> {
        if !held().insert(key.clone()) {
            return Err(FsError::Locked(name.to_path_buf()));
        }
        Ok(Entry { key, kept: false })
    }

    /// The entry, now owned by a [FileLock].
    fn keep(mut self) -> Held {
        self.kept = true;
        self.key.clone()
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
fn held() -> std::sync::MutexGuard<'static, BTreeSet<Held>> {
    HELD.lock()
        .expect("a writer panicked while it took or gave back a file lock")
}

/// The name the lock of `name` is kept under: its canonical name (links resolved), without `.partial` suffixes, so
/// `NAME`, a link to it and `NAME.partial` are one lock.
fn lock_key(name: &Path) -> io::Result<PathBuf> {
    let mut key = match std::fs::canonicalize(name) {
        Ok(p) => p,
        Err(e) if e.kind() == io::ErrorKind::NotFound => absolute(name)?,
        Err(e) => return Err(e),
    };
    while let Some(stem) = key.file_name().and_then(strip_partial) {
        key = key.with_file_name(stem);
    }
    Ok(key)
}

/// `name` without its `.partial` suffix; None when it has none or is nothing else.
#[cfg(unix)]
fn strip_partial(name: &std::ffi::OsStr) -> Option<std::ffi::OsString> {
    use std::os::unix::ffi::OsStrExt;
    let stem = name.as_bytes().strip_suffix(b".partial")?;
    (!stem.is_empty()).then(|| std::ffi::OsStr::from_bytes(stem).to_owned())
}

#[cfg(not(unix))]
fn strip_partial(name: &std::ffi::OsStr) -> Option<std::ffi::OsString> {
    let stem = name.to_str()?.strip_suffix(".partial")?;
    (!stem.is_empty()).then(|| stem.into())
}

/// The device and inode of `file`; None where std has no file identity (outside unix).
#[cfg(unix)]
fn identity(file: &File) -> io::Result<Option<Held>> {
    use std::os::unix::fs::MetadataExt;
    let m = file.metadata()?;
    Ok(Some(Held::File {
        dev: m.dev(),
        ino: m.ino(),
    }))
}

#[cfg(not(unix))]
fn identity(_file: &File) -> io::Result<Option<Held>> {
    Ok(None)
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
