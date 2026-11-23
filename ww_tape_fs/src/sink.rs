//! Piece files: [FileSink], [FilePieces] and [FileSource].

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use ww_tape::reader::ByteSource;
use ww_tape::sink::{BaseRefused, PieceStore, TapeSink, check_base};

use crate::chain::piece_name;
use crate::lock::{self, FsWarning, Locked, Open};

/// Why a [FileSink], [FilePieces] or [FileSource] failed or refused.
///
/// Its text names the file, what was done and the kind of failure or refusal once: `x.wwt: locked by another writer`;
/// the system's error of [FsError::Io] is its [source](core::error::Error::source).
#[derive(Debug)]
#[non_exhaustive]
pub enum FsError {
    /// Opening, writing, syncing or reading `path` failed, with what was being done; its text ends with the
    /// error's [kind](io::Error::kind), the system's error is its source.
    Io {
        what: &'static str,
        path: PathBuf,
        error: io::Error,
    },
    /// A new file that is already there, closed, unclosed or empty.
    Exists(PathBuf),
    /// Another writer holds the file: one writer per file ([ww_tape::sink]).
    Locked(PathBuf),
    /// A name a file sink doesn't write or lock through: a directory, a FIFO or a socket.
    NotAFile(PathBuf),
    /// A piece start [check_base] refused for the file at `path`.
    Base { path: PathBuf, refused: BaseRefused },
    /// Nothing to resume at `path`: no file there, or a chain's directory that is missing or holds no first piece.
    NoPiece(PathBuf),
    /// The file system of `path` takes no writes ([io::ErrorKind::ReadOnlyFilesystem]): a read-only mount or medium.
    /// Read the file with [crate::FileSource] instead.
    ReadOnly(PathBuf),
    /// The front of the file of a chain whose pieces are all in one ([ww_tape::PieceTapes::One]) at `path` is not
    /// freed: a file keeps every byte ([ww_tape::PieceStore::drop_front]).
    Whole(PathBuf),
}

impl core::fmt::Display for FsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io { what, path, error } => {
                write!(f, "{}: {what}: {}", path.display(), error.kind())
            }
            Self::Exists(p) => write!(f, "{}: already exists", p.display()),
            Self::Locked(p) => write!(f, "{}: locked by another writer", p.display()),
            Self::NotAFile(p) => write!(f, "{}: not a regular file", p.display()),
            Self::NoPiece(p) => write!(f, "{}: no piece to resume", p.display()),
            Self::ReadOnly(p) => write!(f, "{}: on a read-only file system", p.display()),
            Self::Whole(p) => write!(
                f,
                "{}: a file keeps every byte, its front is not freed",
                p.display()
            ),
            Self::Base { path, refused } => {
                write!(
                    f,
                    "{}: a piece can't start there: {refused}",
                    path.display()
                )
            }
        }
    }
}

impl core::error::Error for FsError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Io { error, .. } => Some(error),
            _ => None,
        }
    }
}

/// An [FsError::Io] of `what` on `path`, or [FsError::ReadOnly] when its file system takes no writes.
pub(crate) fn io_err(what: &'static str, path: &Path) -> impl FnOnce(io::Error) -> FsError {
    move |error| match error.kind() {
        io::ErrorKind::ReadOnlyFilesystem => FsError::ReadOnly(path.to_path_buf()),
        _ => FsError::Io {
            what,
            path: path.to_path_buf(),
            error,
        },
    }
}

/// `offset` in a piece starting at `base`, from the start of the file.
fn file_offset(base: u64, offset: u64) -> io::Result<u64> {
    base.checked_add(offset)
        .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))
}

/// A file of one piece or of pieces back to back, written under its own name from the start and appended to.
///
/// Whether the piece is open, closed or torn is read from its bytes (its close and tail), never from its name.
pub struct FileSink {
    file: Locked,
    path: PathBuf,
    /// Where the piece being written starts in the file
    base: u64,
    /// The file holds pieces back to back ([TapeSink::holds_pieces])
    pieces: bool,
    /// Set when the file is on a network or user-space file system
    warning: Option<FsWarning>,
}

impl FileSink {
    /// A new file of one piece at `path`, created exclusively: [FsError::Exists] when `path` is there, closed,
    /// unclosed or empty ([Self::reopen] takes it). The file and its directory are synced.
    pub fn create(path: &Path) -> Result<Self, FsError> {
        Self::new_file(path, false)
    }

    /// A new file at `path` for pieces back to back ([PieceStore::create_whole]), created exclusively as
    /// [Self::create]; the file and its directory are synced.
    pub fn create_whole(path: &Path) -> Result<Self, FsError> {
        Self::new_file(path, true)
    }

    /// The file at `path`, appended to in place; created (and its directory synced) if it isn't there. Its length
    /// is where the writer starts.
    pub fn append(path: &Path) -> Result<Self, FsError> {
        let (file, created) = lock::open(path, Open::Either)?;
        let sink = Self::locked(file, path, true);
        if created {
            sink.sync_new()?;
        }
        Ok(sink)
    }

    /// The file at `path`, appended to in place, closed or not ([PieceStore::open_whole]); None when it isn't
    /// there.
    pub fn open_whole(path: &Path) -> Result<Option<Self>, FsError> {
        Ok(lock::open_existing(path)?.map(|file| Self::locked(file, path, true)))
    }

    /// The file of one piece at `path` to write on, closed or not: a resume reads which from its bytes; None when it
    /// isn't there.
    pub fn reopen(path: &Path) -> Result<Option<Self>, FsError> {
        Ok(lock::open_existing(path)?.map(|file| Self::locked(file, path, false)))
    }

    /// The file at `path` to write over in place, holding pieces back to back, locked as a writer's: a seal's
    /// ([crate::writer::seal]). None when it isn't there.
    pub(crate) fn to_overwrite(path: &Path) -> Result<Option<Self>, FsError> {
        Ok(lock::open_to_overwrite(path)?.map(|file| Self::locked(file, path, true)))
    }

    /// A new file at `path`, holding pieces back to back with `pieces`, synced with its directory.
    fn new_file(path: &Path, pieces: bool) -> Result<Self, FsError> {
        let (file, _) = lock::open(path, Open::New)?;
        let sink = Self::locked(file, path, pieces);
        sink.sync_new()?;
        Ok(sink)
    }

    /// The sink of `file`, opened and locked as `path`.
    fn locked(file: Locked, path: &Path, pieces: bool) -> Self {
        let warning = lock::warning(file.file(), path);
        Self {
            file,
            path: path.to_path_buf(),
            base: 0,
            pieces,
            warning,
        }
    }

    /// Sync a file just created and its directory, so its name survives a crash.
    fn sync_new(&self) -> Result<(), FsError> {
        self.file
            .file()
            .sync_all()
            .map_err(io_err("syncing the new file", &self.path))?;
        sync_dir(&self.path).map_err(io_err("syncing the directory", &self.path))
    }
}

impl TapeSink for FileSink {
    type Error = FsError;

    fn write_all(&mut self, bytes: &[u8]) -> Result<(), FsError> {
        self.file
            .file_mut()
            .write_all(bytes)
            .map_err(io_err("writing the file", &self.path))
    }

    fn sync(&mut self) -> Result<(), FsError> {
        self.file
            .file()
            .sync_data()
            .map_err(io_err("syncing the file", &self.path))
    }

    /// Nothing: the file has its name from the start, and the writer synced its bytes.
    fn close(&mut self) -> Result<(), FsError> {
        Ok(())
    }

    fn size(&self) -> Result<u64, FsError> {
        Ok(self.len()?.saturating_sub(self.base))
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
        file_offset(self.base, offset)
            .and_then(|at| read_file_at(self.file.file(), at, buf))
            .map_err(io_err("reading the file", &self.path))
    }

    fn truncate(&mut self, len: u64) -> Result<(), FsError> {
        let truncate = |file: &mut File| {
            let len = file_offset(self.base, len)?;
            file.set_len(len)?;
            // a file opened for writing at its start writes at the new end too (one opened to append always does)
            io::Seek::seek(file, io::SeekFrom::Start(len))?;
            file.sync_all()
        };
        truncate(self.file.file_mut()).map_err(io_err("truncating the file", &self.path))
    }

    fn base(&self) -> u64 {
        self.base
    }

    fn holds_pieces(&self) -> bool {
        self.pieces
    }

    fn rebase(&mut self, base: u64) -> Result<(), FsError> {
        self.base = check_base(base, self.len()?, self.holds_pieces()).map_err(|refused| {
            FsError::Base {
                path: self.path.clone(),
                refused,
            }
        })?;
        Ok(())
    }
}

impl FileSink {
    /// The name of the file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Set when the file is on a network or user-space file system, where the lock may not keep a second writer
    /// out: the application shows it. Writing goes on.
    pub fn warning(&self) -> Option<&FsWarning> {
        self.warning.as_ref()
    }

    /// Write `bytes` over the file's bytes at `offset` in the piece, not past its end.
    pub(crate) fn write_at(&self, offset: u64, bytes: &[u8]) -> Result<(), FsError> {
        let what = "writing over the file";
        let end = file_offset(self.base, offset)
            .and_then(|at| file_offset(at, crate::seal::wide(bytes.len())))
            .map_err(io_err(what, &self.path))?;
        if end > self.len()? {
            return Err(io_err(what, &self.path)(
                io::ErrorKind::UnexpectedEof.into(),
            ));
        }
        file_offset(self.base, offset)
            .and_then(|at| write_file_at(self.file.file(), at, bytes))
            .map_err(io_err(what, &self.path))
    }

    /// The whole length of the file.
    fn len(&self) -> Result<u64, FsError> {
        Ok(self
            .file
            .file()
            .metadata()
            .map_err(io_err("reading the file's length", &self.path))?
            .len())
    }
}

#[cfg(unix)]
fn read_file_at(file: &File, offset: u64, buf: &mut [u8]) -> io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

#[cfg(unix)]
fn write_file_at(file: &File, offset: u64, bytes: &[u8]) -> io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, bytes, offset)
}

#[cfg(windows)]
fn write_file_at(file: &File, mut offset: u64, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        match std::os::windows::fs::FileExt::seek_write(file, bytes, offset)? {
            0 => return Err(io::ErrorKind::WriteZero.into()),
            n => {
                let past_end = || io::Error::from(io::ErrorKind::InvalidData);
                bytes = bytes.get(n..).ok_or_else(past_end)?;
                let n =
                    u64::try_from(n).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                offset = offset.checked_add(n).ok_or_else(past_end)?;
            }
        }
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn write_file_at(mut file: &File, offset: u64, bytes: &[u8]) -> io::Result<()> {
    use std::io::Seek;
    let back = file.stream_position()?;
    file.seek(io::SeekFrom::Start(offset))?;
    let w = file.write_all(bytes);
    file.seek(io::SeekFrom::Start(back))?;
    w
}

#[cfg(windows)]
fn read_file_at(file: &File, mut offset: u64, mut buf: &mut [u8]) -> io::Result<()> {
    while !buf.is_empty() {
        match std::os::windows::fs::FileExt::seek_read(file, buf, offset)? {
            0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            n => {
                let past_end = || io::Error::from(io::ErrorKind::InvalidData);
                buf = core::mem::take(&mut buf)
                    .get_mut(n..)
                    .ok_or_else(past_end)?;
                let n =
                    u64::try_from(n).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                offset = offset.checked_add(n).ok_or_else(past_end)?;
            }
        }
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn read_file_at(mut file: &File, offset: u64, buf: &mut [u8]) -> io::Result<()> {
    use std::io::{Read, Seek};
    let back = file.stream_position()?;
    file.seek(io::SeekFrom::Start(offset))?;
    let r = file.read_exact(buf);
    file.seek(io::SeekFrom::Start(back))?;
    r
}

/// Piece files: a [PieceStore] of [FileSink]s keyed by a piece's seq, each piece a file of a directory, or every
/// key the one file that holds all pieces of a chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilePieces(Names);

#[derive(Clone, Debug, PartialEq, Eq)]
enum Names {
    /// Key `seq` is `dir/`[piece_name] (`seq`)
    Dir(PathBuf),
    /// Every key is this file
    File(PathBuf),
}

impl FilePieces {
    /// The pieces in `dir` (which must exist), key `seq` the file `dir/`[piece_name] (`seq`).
    pub fn new(dir: &Path) -> Self {
        Self(Names::Dir(dir.to_path_buf()))
    }

    /// The one file `path` under every key: the file of a chain whose pieces are all in one
    /// ([PieceTapes::One](ww_tape::PieceTapes::One)).
    pub fn file(path: &Path) -> Self {
        Self(Names::File(path.to_path_buf()))
    }

    /// The file of `key`.
    pub fn path_of(&self, key: u64) -> PathBuf {
        match &self.0 {
            Names::Dir(dir) => dir.join(piece_name(key)),
            Names::File(path) => path.clone(),
        }
    }
}

impl PieceStore for FilePieces {
    type Key = u64;
    type Error = FsError;
    type Sink = FileSink;
    type Source = FileSource;

    fn key_of(&self, seq: u64) -> u64 {
        seq
    }

    fn create(&mut self, key: &u64) -> Result<FileSink, FsError> {
        FileSink::create(&self.path_of(*key))
    }

    fn reopen(&mut self, key: &u64) -> Result<Option<FileSink>, FsError> {
        FileSink::reopen(&self.path_of(*key))
    }

    /// The file of `key`, closed or not: a chain reads from its bytes whether it is closed.
    fn closed(&self, key: &u64) -> Result<Option<FileSource>, FsError> {
        match FileSource::open(&self.path_of(*key)) {
            Ok(src) => Ok(Some(src)),
            Err(FsError::Io { error, .. }) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn create_whole(&mut self, key: &u64) -> Result<FileSink, FsError> {
        FileSink::create_whole(&self.path_of(*key))
    }

    fn open_whole(&mut self, key: &u64) -> Result<Option<FileSink>, FsError> {
        FileSink::open_whole(&self.path_of(*key))
    }

    /// The lowest piece file of the directory from `from` on, by its name ([piece_name]); names of another shape
    /// are passed over. The file of a chain in one file is piece 0.
    fn first_from(&self, from: u64) -> Result<Option<u64>, FsError> {
        let dir = match &self.0 {
            Names::Dir(dir) => dir,
            Names::File(path) => return Ok((from == 0 && !none_at(path)).then_some(0)),
        };
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_err("listing the chain's directory", dir)(e)),
        };
        let mut lowest: Option<u64> = None;
        for entry in entries {
            let entry = entry.map_err(io_err("listing the chain's directory", dir))?;
            let Some(seq) = entry.file_name().to_str().and_then(seq_of_name) else {
                continue;
            };
            if seq >= from && lowest.is_none_or(|l| seq < l) {
                lowest = Some(seq);
            }
        }
        Ok(lowest)
    }

    /// Delete the piece's file and sync its directory, so the removal survives a crash.
    fn remove(&mut self, key: &u64) -> Result<bool, FsError> {
        let path = self.path_of(*key);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(io_err("removing the piece's file", &path)(e)),
        }
        sync_dir(&path).map_err(io_err("syncing the directory", &path))?;
        Ok(true)
    }

    /// Refused ([FsError::Whole]): a file keeps every byte.
    fn drop_front(&mut self, key: &u64, _to: u64) -> Result<Option<u64>, FsError> {
        Err(FsError::Whole(self.path_of(*key)))
    }
}

/// The seq a piece file's name gives: [piece_name] of it, nothing else.
fn seq_of_name(name: &str) -> Option<u64> {
    let seq: u64 = name.strip_suffix(".wwt")?.parse().ok()?;
    (piece_name(seq) == name).then_some(seq)
}

/// A file read by positional reads, a closed piece or any tape: the [ByteSource] of a file for a
/// [TapeReader](ww_tape::TapeReader).
#[derive(Debug)]
pub struct FileSource {
    file: File,
    path: PathBuf,
}

impl FileSource {
    /// The file at `path`, opened to read.
    pub fn open(path: &Path) -> Result<Self, FsError> {
        let file = File::open(path).map_err(io_err("opening the file", path))?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }

    /// The metadata of the open file (`fstat`), not of whatever its path names now: a file replaced on disk by a
    /// rename after the open keeps the metadata it had. On unix its device and inode
    /// ([MetadataExt](std::os::unix::fs::MetadataExt)) tell it apart from the file the path names now.
    pub fn metadata(&self) -> Result<std::fs::Metadata, FsError> {
        self.file
            .metadata()
            .map_err(io_err("reading the file's metadata", &self.path))
    }
}

impl ByteSource for FileSource {
    type Error = FsError;

    fn size(&self) -> Result<u64, FsError> {
        Ok(self
            .file
            .metadata()
            .map_err(io_err("reading the file's length", &self.path))?
            .len())
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
        read_file_at(&self.file, offset, buf).map_err(io_err("reading the file", &self.path))
    }
}

/// Whether nothing is at `path`: nothing to resume. A name that can't be looked at counts as there, so opening it
/// reports why.
pub(crate) fn none_at(path: &Path) -> bool {
    matches!(std::fs::symlink_metadata(path), Err(e) if e.kind() == io::ErrorKind::NotFound)
}

/// Sync the directory a file is in, so a new name survives a crash. Windows has no such call (NTFS
/// journals names itself).
fn sync_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        let dir = match path.parent() {
            Some(d) if !d.as_os_str().is_empty() => d,
            _ => Path::new("."),
        };
        File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file system that takes no writes is its own refusal, whichever step met it, and says so in its text.
    #[test]
    fn a_read_only_file_system_is_its_own_refusal() {
        let path = Path::new("/media/card/x.wwt");
        let e =
            io_err("creating the file", path)(io::Error::from(io::ErrorKind::ReadOnlyFilesystem));
        assert!(matches!(&e, FsError::ReadOnly(p) if p == path), "{e:?}");
        assert_eq!(
            e.to_string(),
            "/media/card/x.wwt: on a read-only file system"
        );
        let other =
            io_err("creating the file", path)(io::Error::from(io::ErrorKind::PermissionDenied));
        assert!(matches!(other, FsError::Io { .. }), "{other:?}");
    }

    /// A child forked while a sink holds its lock, which runs no `exec`, holds no lock: the writer's process takes
    /// the file again as soon as the writer is dropped. Without the fork handler the child would share the open
    /// file, and with it the `flock`, until it exits.
    #[cfg(unix)]
    #[test]
    #[expect(
        unsafe_code,
        reason = "fork, sleep, _exit, kill and waitpid have no safe binding"
    )]
    fn a_forked_child_holds_no_lock() {
        let dir = std::env::temp_dir().join(format!("ww_tape_sink_fork_{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.wwt");
        let mut sink = FileSink::create(&path).unwrap();
        sink.write_all(b"abc").unwrap();
        // SAFETY: the child calls only async-signal-safe functions (sleep, _exit) and returns never
        let child = unsafe { libc::fork() };
        assert!(child >= 0, "fork failed");
        if child == 0 {
            // SAFETY: async-signal-safe; the child ends without running the parent's destructors
            unsafe {
                libc::sleep(60);
                libc::_exit(0);
            }
        }
        drop(sink);
        let again = FileSink::reopen(&path);
        // SAFETY: `child` is this process's child, killed and reaped once
        unsafe {
            libc::kill(child, libc::SIGKILL);
            libc::waitpid(child, core::ptr::null_mut(), 0);
        }
        assert!(matches!(again, Ok(Some(_))), "{:?}", again.err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A file on a network or user-space file system (a fake NFS type here) gets a warning naming it; writing goes
    /// on. A local one gets none.
    #[test]
    fn a_shared_file_system_is_a_warning() {
        let dir = std::env::temp_dir().join(format!("ww_tape_sink_warn_{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.wwt");
        lock::FAKE_MAGIC.with(|m| m.set(Some(0x6969)));
        let mut sink = FileSink::create(&path).unwrap();
        lock::FAKE_MAGIC.with(|m| m.set(None));
        let w = sink.warning().expect("NFS is a shared file system").clone();
        assert_eq!(w.fs(), lock::SharedFs::Nfs);
        assert_eq!(w.path(), path);
        assert!(
            w.to_string()
                .starts_with(&format!("{}: on NFS,", path.display())),
            "{w}"
        );
        sink.write_all(b"abc").unwrap();
        sink.sync().unwrap();
        drop(sink);
        lock::FAKE_MAGIC.with(|m| m.set(Some(0xEF53)));
        let sink = FileSink::reopen(&path).unwrap().unwrap();
        lock::FAKE_MAGIC.with(|m| m.set(None));
        assert_eq!(sink.warning(), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Each refusal's text names the file and the kind once; an io error's text names the file, what was done and
    /// the error's kind, and the system's error is its source, said once.
    #[test]
    fn an_error_text_names_the_file_and_the_kind() {
        use core::error::Error;

        let dir = std::env::temp_dir().join(format!("ww_tape_sink_texts_{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("x.wwt");
        let shown = path.display();

        let mut sink = FileSink::create(&path).unwrap();
        let e = FileSink::reopen(&path).err().unwrap();
        assert_eq!(e.to_string(), format!("{shown}: locked by another writer"));
        let e = FileSink::create(&path).err().unwrap();
        assert_eq!(e.to_string(), format!("{shown}: already exists"));
        assert!(e.source().is_none());

        sink.write_all(b"abc").unwrap();
        let e = sink.rebase(1).unwrap_err();
        assert_eq!(
            e.to_string(),
            format!("{shown}: a piece can't start there: a sink of one piece takes no base but 0")
        );
        assert!(e.source().is_none());
        sink.close().unwrap();
        drop(sink);

        let e = FileSink::create(&path).err().unwrap();
        assert_eq!(e.to_string(), format!("{shown}: already exists"));

        let e = FileSink::append(&dir).err().unwrap();
        assert_eq!(
            e.to_string(),
            format!("{}: not a regular file", dir.display())
        );

        let missing = dir.join("gone").join("y.wwt");
        let e = FileSink::append(&missing).err().unwrap();
        assert_eq!(
            e.to_string(),
            format!("{}: creating the file: entity not found", missing.display())
        );
        let os = e.source().unwrap().to_string();
        assert!(!e.to_string().contains(&os), "{e} repeats {os}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
