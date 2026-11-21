//! [FileLock]: one writer per file, by a lock file beside it that only writers open.
//!
//! The lock of the file `NAME` is held on `NAME.lock` ([lock_name]: a hashed name where `NAME` leaves no room),
//! never on `NAME` or `NAME.partial` themselves:
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
//!   hard link is another name of the same file that no name tells apart: on unix the table also keys each file a
//!   sink opened by its device and inode, so two writers of one process are refused through hard links too (std
//!   has no file identity elsewhere). Two
//!   processes writing one file through two hard-linked names are not refused: both append to the same bytes, and
//!   their blocks interleave into a file that reads as damaged. A lock on the file itself would catch that, but
//!   every kind either passes to a forked child (`flock`, open file description locks) or goes away when a reader
//!   of the holder's process closes the file (record locks).
//! - The lock file is opened without following a link, and must be a regular file ([FsError::NotAFile]): a link
//!   planted there would lock another file, and a dangling one would create its target.
//! - On unix the holder removes the lock file when it lets go, while it still holds the lock. A writer that opened the file
//!   before it was removed takes the lock on a file no longer named `NAME.lock`, sees so and opens the new one
//!   ([LOCK_TRIES_MAX]). A lock file a crash left holds no lock and is taken as it is.
//!
//! Elsewhere (Windows) the lock file holds the file system's lock ([File::try_lock]): a child process gets no
//! handle there unless the parent passes it. The lock file is never removed there: removing it after it is closed
//! could remove the file of the next holder, and a later writer would lock a new file beside it. It stays beside
//! `NAME` once the writer is gone, holds no lock, and the next writer takes it as it is.
//!
//! A device (such as a raw SD card, whose directory takes no lock file) is locked itself, opened without waiting,
//! by the file system's lock on a handle of its own: a child forked while it is held holds it too, until it runs
//! `exec`. Any other name that isn't a regular file (a directory, a FIFO, a socket) is refused before it is opened
//! ([FsError::NotAFile]): opening a FIFO waits for its other end.

use std::collections::BTreeSet;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::sink::{FsError, io_err, partial_name};

/// How often a file sink opens its lock file again after a holder removed the one it opened: each time is a
/// holder that let go meanwhile.
pub const LOCK_TRIES_MAX: usize = 8;

/// The files this process holds the lock of: a record lock doesn't keep apart two writers of one process. Owned by
/// [FileLock]: taken in [FileLock::take] and [FileLock::bind], given back in its drop.
// spirit-level: allow(static-interior-mutability) record locks are per process, so is this set
static HELD: Mutex<BTreeSet<Held>> = Mutex::new(BTreeSet::new());

/// An entry of [HELD].
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Held {
    /// A lock file, by its absolute path
    Name(PathBuf),
    /// A file a sink opened, by its device and inode
    #[cfg(unix)]
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

/// The longest name of a file in a directory, in bytes (Linux, macOS; Windows counts 255 UTF-16 units, never more
/// than the bytes).
const NAME_BYTES_MAX: usize = 255;

/// The lock file of the file `path`: `NAME.lock` next to `NAME`, or, where that name would pass the 255-byte limit
/// of a name, `HASH.lock` next to it, `HASH` the BLAKE3 hash of `NAME` in hex.
pub fn lock_name(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or(path.as_os_str());
    if name.len().saturating_add(".lock".len()) <= NAME_BYTES_MAX {
        let mut name = path.as_os_str().to_owned();
        name.push(".lock");
        return PathBuf::from(name);
    }
    let mut hex = blake3::hash(name.as_encoded_bytes()).to_hex().to_string();
    hex.push_str(".lock");
    path.with_file_name(hex)
}

/// An [FsError::Io] of `what` on the lock file `path` of the piece `name`, or [FsError::ReadOnly] naming the piece
/// when the file system takes no writes: the caller asked for the piece and never named its lock file.
fn lock_err(
    what: &'static str,
    path: &Path,
    name: &Path,
) -> impl FnOnce(std::io::Error) -> FsError {
    let (path, name) = (path.to_path_buf(), name.to_path_buf());
    move |error| match error.kind() {
        std::io::ErrorKind::ReadOnlyFilesystem => FsError::ReadOnly(name),
        _ => FsError::Io { what, path, error },
    }
}

impl FileLock {
    /// The lock of the file `name` (its final name, also while it is written as `NAME.partial`):
    /// [FsError::Locked] while another writer, of this process or another, holds it.
    pub(crate) fn take(name: &Path) -> Result<Self, FsError> {
        match std::fs::metadata(name) {
            Ok(m) if is_device(&m) => return Self::take_itself(name),
            Ok(m) if !m.is_file() => return Err(FsError::NotAFile(name.to_path_buf())),
            _ => {}
        }
        let mut lock = Self {
            files: Vec::new(),
            keys: Vec::new(),
        };
        // the names `name` and its `.partial` stand for; one unless a link points elsewhere
        let mut keys = BTreeSet::new();
        keys.insert(lock_key(name).map_err(io_err("finding the file to lock", name))?);
        let partial = partial_name(name);
        if std::fs::symlink_metadata(&partial).is_ok() {
            keys.insert(lock_key(&partial).map_err(io_err("finding the file to lock", &partial))?);
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
        let io_err = |what| lock_err(what, path, name);
        for _ in 0..LOCK_TRIES_MAX {
            let file = match open_lock_file(path) {
                Ok(f) => f,
                Err(e) if is_link_refused(&e) => return Err(FsError::NotAFile(path.to_path_buf())),
                Err(error) => {
                    return Err(io_err("opening the lock file")(error));
                }
            };
            let meta = file
                .metadata()
                .map_err(io_err("reading the lock file's type"))?;
            if !meta.is_file() {
                return Err(FsError::NotAFile(path.to_path_buf()));
            }
            if !try_lock(&file).map_err(io_err("locking the lock file"))? {
                return Err(FsError::Locked(name.to_path_buf()));
            }
            if names_itself(path, &file).map_err(io_err("checking the lock file's name"))? {
                return Ok(file);
            }
            // its holder removed it as it let go: the lock is on a file nobody else finds
        }
        Err(FsError::Locked(name.to_path_buf()))
    }

    /// The lock of `name`, not a regular file, on the file itself.
    fn take_itself(name: &Path) -> Result<Self, FsError> {
        let key = absolute(name).map_err(io_err("finding the file to lock", name))?;
        let entry = Entry::claim(Held::Name(key), name)?;
        let file = open_device(name).map_err(io_err("opening the file to lock", name))?;
        match file.try_lock() {
            Ok(()) => Ok(Self {
                files: vec![LockFile { file, path: None }],
                keys: vec![entry.keep()],
            }),
            Err(std::fs::TryLockError::WouldBlock) => Err(FsError::Locked(name.to_path_buf())),
            Err(std::fs::TryLockError::Error(error)) => {
                Err(io_err("locking the file", name)(error))
            }
        }
    }

    /// Hold `file`, opened as `name` under this lock, by its identity too: [FsError::Locked] when another writer of
    /// this process holds it under another name (a hard link). [FsError::NotAFile] unless it is a regular file or a
    /// device.
    pub(crate) fn bind(&mut self, file: &File, name: &Path) -> Result<(), FsError> {
        let meta = file
            .metadata()
            .map_err(io_err("reading the file's type", name))?;
        if !meta.is_file() && !is_device(&meta) {
            return Err(FsError::NotAFile(name.to_path_buf()));
        }
        if let Some(id) = identity(file).map_err(io_err("reading the file's identity", name))? {
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
            if cfg!(unix)
                && let Some(path) = &path
                && matches!(names_itself(path, &file), Ok(true))
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
        Ok(Self { key, kept: false })
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

/// Whether `meta` is a device, locked itself: a block or character device on unix, anything but a file or a
/// directory elsewhere.
fn is_device(meta: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    return {
        use std::os::unix::fs::FileTypeExt;
        let t = meta.file_type();
        t.is_block_device() || t.is_char_device()
    };
    #[cfg(not(unix))]
    return !meta.is_file() && !meta.is_dir();
}

/// The device `name`, opened to hold its lock: without waiting (a serial port waits for its carrier otherwise).
#[cfg(unix)]
fn open_device(name: &Path) -> io::Result<File> {
    use rustix::fs::{Mode, OFlags};
    let flags = OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC;
    Ok(File::from(rustix::fs::open(name, flags, Mode::empty())?))
}

#[cfg(not(unix))]
fn open_device(name: &Path) -> io::Result<File> {
    File::open(name)
}

/// The lock file `path`, opened to read and write and created if it isn't there; a link is never followed, and a
/// FIFO doesn't block the open.
#[cfg(unix)]
fn open_lock_file(path: &Path) -> io::Result<File> {
    use rustix::fs::{Mode, OFlags};
    let flags =
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let mode = Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::ROTH;
    Ok(File::from(rustix::fs::open(path, flags, mode)?))
}

/// The lock file `path`, opened to read and write and created if it isn't there; a link is opened itself, not
/// followed.
#[cfg(windows)]
fn open_lock_file(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    /// `FILE_FLAG_OPEN_REPARSE_POINT`: open a link itself
    const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(OPEN_REPARSE_POINT)
        .open(path)
}

#[cfg(not(any(unix, windows)))]
fn open_lock_file(path: &Path) -> io::Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

/// Whether opening the lock file failed because it is a link (`O_NOFOLLOW` refuses one with `ELOOP`).
fn is_link_refused(e: &io::Error) -> bool {
    #[cfg(unix)]
    return e.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error());
    #[cfg(not(unix))]
    return {
        let _ = e;
        false
    };
}

/// Whether `path` itself, not what a link there points to, names `file`. False when `path` is gone.
#[cfg(unix)]
fn names_itself(path: &Path, file: &File) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let opened = file.metadata()?;
    match std::fs::symlink_metadata(path) {
        Ok(named) => Ok(named.dev() == opened.dev() && named.ino() == opened.ino()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Whether `path` is still there: std has no file identity outside unix.
#[cfg(not(unix))]
fn names_itself(path: &Path, _file: &File) -> io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A read-only file system met at the lock file names the piece the caller asked for, any other error the lock
    /// file.
    #[test]
    fn a_read_only_lock_file_names_the_piece() {
        let (lock, piece) = (
            Path::new("/media/card/x.wwt.lock"),
            Path::new("/media/card/x.wwt"),
        );
        let e = lock_err("opening the lock file", lock, piece)(std::io::Error::from(
            std::io::ErrorKind::ReadOnlyFilesystem,
        ));
        assert!(matches!(&e, FsError::ReadOnly(p) if p == piece), "{e:?}");
        let e = lock_err("opening the lock file", lock, piece)(std::io::Error::from(
            std::io::ErrorKind::PermissionDenied,
        ));
        assert!(
            matches!(&e, FsError::Io { path, .. } if path == lock),
            "{e:?}"
        );
    }
}
