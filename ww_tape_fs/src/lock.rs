//! [Locked]: one writer per file, by the kernel's lock on the open file itself.
//!
//! A file sink locks the file it writes, under whatever name reaches it (a link, a hard link), and holds the lock
//! until it closes the file. Nothing beside the file is created, so a crash leaves nothing to clean up: the kernel
//! lets the lock go with the process.
//!
//! - On unix it is an `flock` lock, which belongs to the open file. A second open of the file conflicts with it, in
//!   this process too, so two writers of one process are kept apart without a table. A reader opens and closes the
//!   file as it likes: closing another open of the file lets no lock go.
//! - A child forked while a sink holds the lock would share its open file, and with it the lock, until it runs
//!   `exec` (the file is opened close-on-exec) or exits. A fork handler ([fork]) points each locked descriptor of
//!   the child at `/dev/null` instead, opened to read only: the child holds no lock, and a write it makes through a
//!   writer it inherited fails.
//! - On Windows the file is opened sharing reads and deletes but not writes, so a second open to write is refused
//!   by the file system, also on a network share. A lock there ([File::try_lock]) would keep readers out too. A
//!   child gets no handle unless the parent passes it.
//! - A device (such as a raw SD card) is locked the same way, on the sink's own handle.
//!
//! The lock is advisory: other writers of the format take it too. A network or user-space file system may not keep
//! it ([FsWarning]).

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use crate::sink::{FsError, io_err};

/// How [open] opens a file.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Open {
    /// A new file: [FsError::Exists] when the name is there
    New,
    /// The file there, else a new one
    Either,
}

/// An open file this process holds the lock of, see the [module docs](self).
#[derive(Debug)]
pub(crate) struct Locked {
    file: File,
}

/// The file `path` opened to read and append, as `how` says, and locked; the bool is true for a file it created.
/// [FsError::Locked] while another writer holds it, [FsError::NotAFile] for a directory, a FIFO or a socket (opening
/// a FIFO would wait for its other end).
pub(crate) fn open(path: &Path, how: Open) -> Result<(Locked, bool), FsError> {
    check_type(path)?;
    let (file, created) = match open_file(path, Access::Create) {
        Ok(f) => (f, true),
        Err(FsError::Exists(_)) if how == Open::Either => (open_file(path, Access::Append)?, false),
        Err(e) => return Err(e),
    };
    Ok((Locked::take(checked(file, path)?, path)?, created))
}

/// The file there at `path`, opened to read and append and locked as [open]; None when the name isn't there.
pub(crate) fn open_existing(path: &Path) -> Result<Option<Locked>, FsError> {
    open_there(path, Access::Append)
}

/// The file there at `path`, opened to read and to write over its bytes at any offset, and locked as [open]; None
/// when the name isn't there. A write at an offset of a file opened to append would land at its end.
pub(crate) fn open_to_overwrite(path: &Path) -> Result<Option<Locked>, FsError> {
    open_there(path, Access::Overwrite)
}

/// The file there at `path`, opened as `how` says and locked; None when the name isn't there.
fn open_there(path: &Path, how: Access) -> Result<Option<Locked>, FsError> {
    check_type(path)?;
    match open_file(path, how) {
        Ok(file) => Ok(Some(Locked::take(checked(file, path)?, path)?)),
        Err(FsError::Io { error, .. }) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// [FsError::NotAFile] when `path` names a directory, a FIFO or a socket, checked before it is opened.
fn check_type(path: &Path) -> Result<(), FsError> {
    match std::fs::metadata(path) {
        Ok(m) if !m.is_file() && !is_device(&m) => Err(FsError::NotAFile(path.to_path_buf())),
        _ => Ok(()),
    }
}

/// `file`, opened as `path`, when it is a regular file or a device: the name may have changed since [check_type].
fn checked(file: File, path: &Path) -> Result<File, FsError> {
    let meta = file
        .metadata()
        .map_err(io_err("reading the file's type", path))?;
    if !meta.is_file() && !is_device(&meta) {
        return Err(FsError::NotAFile(path.to_path_buf()));
    }
    Ok(file)
}

/// How [open_file] opens a file to read and write.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Access {
    /// A new file, appended to: refused when it is there
    Create,
    /// The file there, appended to
    Append,
    /// The file there, written at any offset ([open_to_overwrite])
    Overwrite,
}

/// `path` opened to read and write as `how` says.
fn open_file(path: &Path, how: Access) -> Result<File, FsError> {
    let new = how == Access::Create;
    let mut options = OpenOptions::new();
    options.read(true).create_new(new);
    match how {
        Access::Create | Access::Append => options.append(true),
        Access::Overwrite => options.write(true),
    };
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        /// `FILE_SHARE_READ | FILE_SHARE_DELETE`: readers may open the file, another writer may not
        const SHARE_READ_DELETE: u32 = 0x1 | 0x4;
        options.share_mode(SHARE_READ_DELETE);
    }
    match options.open(path) {
        Ok(f) => Ok(f),
        Err(e) if new && e.kind() == io::ErrorKind::AlreadyExists => {
            Err(FsError::Exists(path.to_path_buf()))
        }
        Err(e) if is_shared_out(&e) => Err(FsError::Locked(path.to_path_buf())),
        Err(error) => Err(io_err(
            if new {
                "creating the file"
            } else {
                "opening the file"
            },
            path,
        )(error)),
    }
}

/// Whether an open failed because another handle opened the file to write (Windows: `ERROR_SHARING_VIOLATION`).
fn is_shared_out(e: &io::Error) -> bool {
    /// `ERROR_SHARING_VIOLATION`
    const SHARING_VIOLATION: i32 = 32;
    cfg!(windows) && e.raw_os_error() == Some(SHARING_VIOLATION)
}

impl Locked {
    /// Lock `file`, opened as `path`: [FsError::Locked] while another open of it holds the lock.
    #[cfg(unix)]
    fn take(file: File, path: &Path) -> Result<Self, FsError> {
        use std::os::fd::AsRawFd;
        // registered before the lock is taken: a fork in between leaves the child nothing to hold
        fork::hold(file.as_raw_fd())
            .map_err(io_err("making the lock stay with this process", path))?;
        // from here the drop gives the registration back
        let locked = Self { file };
        match rustix::fs::flock(
            &locked.file,
            rustix::fs::FlockOperation::NonBlockingLockExclusive,
        ) {
            Ok(()) => Ok(locked),
            Err(e) if e == rustix::io::Errno::WOULDBLOCK => {
                Err(FsError::Locked(path.to_path_buf()))
            }
            Err(e) => Err(io_err("locking the file", path)(e.into())),
        }
    }

    /// On Windows the open shared no writes, which is the lock.
    #[cfg(windows)]
    #[expect(
        clippy::unnecessary_wraps,
        reason = "the same signature as where the lock is taken after the open"
    )]
    fn take(file: File, _path: &Path) -> Result<Self, FsError> {
        Ok(Self { file })
    }

    /// Lock `file` with the file system's lock.
    #[cfg(not(any(unix, windows)))]
    fn take(file: File, path: &Path) -> Result<Self, FsError> {
        match file.try_lock() {
            Ok(()) => Ok(Self { file }),
            Err(std::fs::TryLockError::WouldBlock) => Err(FsError::Locked(path.to_path_buf())),
            Err(std::fs::TryLockError::Error(e)) => Err(io_err("locking the file", path)(e)),
        }
    }

    /// The open file.
    pub(crate) fn file(&self) -> &File {
        &self.file
    }

    /// The open file, to write and truncate.
    pub(crate) fn file_mut(&mut self) -> &mut File {
        &mut self.file
    }
}

#[cfg(unix)]
impl Drop for Locked {
    /// Give the registration back while the descriptor still names this file: once it is closed, its number may
    /// name a file opened by someone else.
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        fork::release(self.file.as_raw_fd());
    }
}

/// Whether `meta` is a device, written and locked as a file: a block or character device on unix, anything but a
/// file or a directory elsewhere.
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

/// The fork handler: a child forked while this process holds file locks holds none of them.
///
/// `pthread_atfork` runs [prepare] in the forking thread before the fork, then [parent] in the parent and [child]
/// in the child. [prepare] takes the table of locked descriptors and keeps its guard in the forking thread, so no
/// sink changes the table across the fork; with any lock held it also opens a pipe. [child] points each locked
/// descriptor at `/dev/null` (opened to read only, close-on-exec, before any fork) with `dup2`, which drops only the
/// child's share of the open file, then closes the pipe. [parent] waits for the pipe's end, so when `fork` returns
/// in the parent the child holds no lock. The child's descriptor numbers stay taken, so a writer the child inherited
/// and drops closes `/dev/null`, never a file of the child's own.
#[cfg(unix)]
#[expect(
    unsafe_code,
    reason = "pthread_atfork, pipe, read, dup2, fcntl and close around a fork have no safe binding"
)]
mod fork {
    use std::cell::RefCell;
    use std::collections::BTreeSet;
    use std::io;
    use std::os::fd::{AsRawFd, OwnedFd, RawFd};
    use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

    /// The descriptors of files this process holds the lock of. Owned by [super::Locked]: added in its take, given
    /// back in its drop; read by [child].
    // spirit-level: allow(static-interior-mutability) the fork handler is per process, so is this set
    static LOCKED: Mutex<BTreeSet<RawFd>> = Mutex::new(BTreeSet::new());

    /// `/dev/null`, opened to read only: what a locked descriptor names in a forked child.
    static NULL: OnceLock<io::Result<OwnedFd>> = OnceLock::new();

    /// The handlers, installed once: 0, or the error `pthread_atfork` returned.
    static INSTALLED: OnceLock<i32> = OnceLock::new();

    /// What [prepare] hands [parent] and [child] across a fork.
    struct Forking {
        /// The guard of [LOCKED]
        locked: MutexGuard<'static, BTreeSet<RawFd>>,
        /// The pipe the parent waits on, read end first; None with no lock held or when it couldn't be opened
        pipe: Option<[RawFd; 2]>,
    }

    // spirit-level: allow(thread-local-state) handed from one fork handler to the next in the forking thread
    thread_local! {
        /// Set by [prepare], taken by [parent] or [child]; owned by the forking thread.
        static FORKING: RefCell<Option<Forking>> = const { RefCell::new(None) };
    }

    /// The table, also after a sink panicked while it held it: each change is one insert or remove, so the table is
    /// never half changed, and a fork handler must not panic.
    fn locked() -> MutexGuard<'static, BTreeSet<RawFd>> {
        LOCKED.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Keep the lock `fd` will hold out of forked children: until [release].
    pub(super) fn hold(fd: RawFd) -> io::Result<()> {
        let null = NULL.get_or_init(|| {
            use rustix::fs::{Mode, OFlags};
            Ok(rustix::fs::open(
                "/dev/null",
                OFlags::RDONLY | OFlags::CLOEXEC,
                Mode::empty(),
            )?)
        });
        if let Err(e) = null {
            return Err(io::Error::new(e.kind(), "/dev/null can't be opened"));
        }
        let installed = *INSTALLED.get_or_init(|| {
            // SAFETY: the handlers are `extern "C"` functions that never unwind; registering them has no other
            // precondition
            unsafe { libc::pthread_atfork(Some(prepare), Some(parent), Some(child)) }
        });
        if installed != 0 {
            return Err(io::Error::from_raw_os_error(installed));
        }
        locked().insert(fd);
        Ok(())
    }

    /// `fd` holds no lock any more.
    pub(super) fn release(fd: RawFd) {
        locked().remove(&fd);
    }

    /// A pipe whose ends close on `exec`, so no other child keeps it open; None when it can't be opened.
    fn pipe() -> Option<[RawFd; 2]> {
        let mut fds: [RawFd; 2] = [-1, -1];
        #[cfg(not(target_vendor = "apple"))]
        // SAFETY: `fds` has room for the two descriptors pipe2 writes
        let r = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
        #[cfg(target_vendor = "apple")]
        // SAFETY: `fds` has room for the two descriptors pipe writes; both are set close-on-exec at once (macOS
        // has no pipe2), while this process's forks wait for the table this thread holds
        let r = unsafe {
            let r = libc::pipe(fds.as_mut_ptr());
            if r == 0 {
                libc::fcntl(fds[0], libc::F_SETFD, libc::FD_CLOEXEC);
                libc::fcntl(fds[1], libc::F_SETFD, libc::FD_CLOEXEC);
            }
            r
        };
        (r == 0).then_some(fds)
    }

    extern "C" fn prepare() {
        let locked = locked();
        let pipe = if locked.is_empty() { None } else { pipe() };
        FORKING.with(|f| {
            if let Ok(mut f) = f.try_borrow_mut() {
                *f = Some(Forking { locked, pipe });
            }
        });
    }

    /// Wait until the child dropped its share of the locked files (or ended), then let the table go.
    extern "C" fn parent() {
        let Some(Forking { locked, pipe }) = FORKING.with(|f| f.try_borrow_mut().ok()?.take())
        else {
            return;
        };
        if let Some([read, write]) = pipe {
            let mut byte = 0u8;
            // SAFETY: both ends are open, made by `prepare` for this fork; the parent closes its write end first,
            // so the read ends once the child closed its own, and `byte` has room for the one byte read
            unsafe {
                libc::close(write);
                loop {
                    let n = libc::read(read, (&raw mut byte).cast(), 1);
                    if n >= 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                        break;
                    }
                }
                libc::close(read);
            }
        }
        drop(locked);
    }

    /// In the child, which runs only this thread: async-signal-safe calls only (`dup2`, `fcntl`, `close`).
    extern "C" fn child() {
        let Some(Forking { locked, pipe }) = FORKING.with(|f| f.try_borrow_mut().ok()?.take())
        else {
            return;
        };
        if let Some(Ok(null)) = NULL.get() {
            let null = null.as_raw_fd();
            for &fd in locked.iter() {
                // SAFETY: both descriptors are open: `null` for the life of the process, `fd` as long as its entry
                // is in the table, which no thread changed since `prepare` took it
                unsafe {
                    if libc::dup2(null, fd) == fd {
                        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                    }
                }
            }
        }
        if let Some([read, write]) = pipe {
            // SAFETY: the child's copies of the pipe `prepare` made for this fork, closed once
            unsafe {
                libc::close(read);
                libc::close(write);
            }
        }
        drop(locked);
    }
}

/// A file system where a file sink's lock may not keep a second writer out, or a sync may return before the bytes
/// are stored: a network file system, or one a program serves in user space.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SharedFs {
    /// NFS: a lock is passed to the server, which may not keep it (`nolock`, an old server, a lost lease).
    Nfs,
    /// SMB or CIFS: a lock may not reach other clients, or may be held only in this client's cache.
    Smb,
    /// FUSE, such as sshfs or rclone: the program serving the file system decides whether a lock holds.
    Fuse,
    /// 9P, such as a folder of the host in a virtual machine or WSL2.
    NineP,
    /// CephFS.
    Ceph,
    /// AFS or OpenAFS.
    Afs,
    /// Lustre.
    Lustre,
    /// A VirtualBox shared folder.
    VirtualBox,
    /// AFP, a share of an older Mac.
    Afp,
    /// WebDAV.
    WebDav,
}

impl SharedFs {
    /// The file system's name, as its users know it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Nfs => "NFS",
            Self::Smb => "SMB",
            Self::Fuse => "FUSE",
            Self::NineP => "9P",
            Self::Ceph => "CephFS",
            Self::Afs => "AFS",
            Self::Lustre => "Lustre",
            Self::VirtualBox => "a VirtualBox shared folder",
            Self::Afp => "AFP",
            Self::WebDav => "WebDAV",
        }
    }

    /// The file system of a Linux `statfs` type number, when it is one of these.
    pub fn from_magic(magic: u32) -> Option<Self> {
        Some(match magic {
            0x6969 => Self::Nfs,
            0x517B | 0xFF53_4D42 | 0xFE53_4D42 => Self::Smb,
            0x6573_5546 => Self::Fuse,
            0x0102_1997 => Self::NineP,
            0x00C3_6400 => Self::Ceph,
            0x5346_414F | 0x6B41_4653 => Self::Afs,
            0x0BD0_0BD0 => Self::Lustre,
            0x786F_4256 => Self::VirtualBox,
            _ => return None,
        })
    }

    /// The file system of a BSD or macOS `statfs` type name, when it is one of these.
    pub fn from_type_name(name: &str) -> Option<Self> {
        Some(match name {
            "nfs" => Self::Nfs,
            "smbfs" | "cifs" => Self::Smb,
            "afpfs" => Self::Afp,
            "webdav" => Self::WebDav,
            n if n.contains("fuse") => Self::Fuse,
            _ => return None,
        })
    }
}

impl core::fmt::Display for SharedFs {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.name())
    }
}

/// A file sink's file is on a [SharedFs], where its lock and syncs may not hold.
///
/// Writing goes on, but the lock may not keep out a writer on another computer or one the file system's program lets
/// in, and a sync may return before the bytes are stored. The application shows it; see
/// [FileSink::warning](crate::FileSink::warning).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsWarning {
    path: PathBuf,
    fs: SharedFs,
}

impl FsWarning {
    /// The warning for the file `path` on `fs`.
    pub fn new(path: &Path, fs: SharedFs) -> Self {
        Self {
            path: path.to_path_buf(),
            fs,
        }
    }

    /// The file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The file system it is on.
    pub fn fs(&self) -> SharedFs {
        self.fs
    }
}

impl core::fmt::Display for FsWarning {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{}: on {}, a network or user-space file system: its lock may not keep out a second writer, and a sync \
             may return before the bytes are stored",
            self.path.display(),
            self.fs
        )
    }
}

/// The warning for `file`, opened as `path`, when its file system is a [SharedFs]; None where it can't be told.
pub(crate) fn warning(file: &File, path: &Path) -> Option<FsWarning> {
    shared_fs(file).map(|fs| FsWarning::new(path, fs))
}

#[cfg(test)]
thread_local! {
    /// A file system type number tests make every file of this thread report.
    pub(crate) static FAKE_MAGIC: core::cell::Cell<Option<u32>> = const { core::cell::Cell::new(None) };
}

/// The [SharedFs] `file` is on, from its `statfs` type number.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn shared_fs(file: &File) -> Option<SharedFs> {
    #[cfg(test)]
    if let Some(magic) = FAKE_MAGIC.with(core::cell::Cell::get) {
        return SharedFs::from_magic(magic);
    }
    let st = rustix::fs::fstatfs(file).ok()?;
    #[expect(
        clippy::as_conversions,
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation,
        reason = "f_type is a 32-bit number in a word whose width and sign vary by target"
    )]
    let magic = st.f_type as u32;
    SharedFs::from_magic(magic)
}

/// The [SharedFs] `file` is on, from its `statfs` type name.
#[cfg(any(target_vendor = "apple", target_os = "freebsd"))]
fn shared_fs(file: &File) -> Option<SharedFs> {
    let st = rustix::fs::fstatfs(file).ok()?;
    let name: Vec<u8> = st
        .f_fstypename
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| c.to_ne_bytes()[0])
        .collect();
    SharedFs::from_type_name(core::str::from_utf8(&name).ok()?)
}

/// Not told elsewhere. On Windows a share keeps the open's sharing mode on the server, so the lock holds there.
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "freebsd"
)))]
fn shared_fs(_file: &File) -> Option<SharedFs> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each network or user-space file system is told by its Linux type number and its BSD type name; local ones
    /// are not.
    #[test]
    fn shared_file_systems_are_told_apart() {
        assert_eq!(SharedFs::from_magic(0x6969), Some(SharedFs::Nfs));
        assert_eq!(SharedFs::from_magic(0xFF53_4D42), Some(SharedFs::Smb));
        assert_eq!(SharedFs::from_magic(0x6573_5546), Some(SharedFs::Fuse));
        // ext4, XFS, Btrfs, tmpfs
        for local in [0xEF53, 0x5846_5342, 0x9123_683E, 0x0102_1994] {
            assert_eq!(SharedFs::from_magic(local), None, "{local:#x}");
        }
        assert_eq!(SharedFs::from_type_name("smbfs"), Some(SharedFs::Smb));
        assert_eq!(SharedFs::from_type_name("macfuse"), Some(SharedFs::Fuse));
        assert_eq!(SharedFs::from_type_name("apfs"), None);
    }
}
