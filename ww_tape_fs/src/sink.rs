//! Piece files: [FileSink], [Files] and [FileSource].

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use ww_tape::reader::ByteSource;
use ww_tape::sink::{BaseRefused, TapeSink, Tapes, check_base};

use crate::chain::piece_name;
use crate::lock::FileLock;

/// Why a [FileSink], [Files] or [FileSource] failed or refused. Its text names the file, what was done and the kind
/// of failure or refusal once: `x.wwt: locked by another writer`; the system's error of [FsError::Io] is its
/// [source](core::error::Error::source).
#[derive(Debug)]
#[non_exhaustive]
pub enum FsError {
    /// Opening, writing, syncing, renaming or reading `path` failed, with what was being done; its text ends with the
    /// error's [kind](io::Error::kind), the system's error is its source.
    Io {
        what: &'static str,
        path: PathBuf,
        error: io::Error,
    },
    /// A new file that is already there (or a `.partial` a crash left that holds bytes).
    Exists(PathBuf),
    /// Another writer holds the file: one writer per file ([ww_tape::sink]).
    Locked(PathBuf),
    /// The `.partial` file `create` opened is no longer the one its name points to once locked: another writer
    /// finished and renamed it, or it was replaced. Nothing is written.
    Replaced(PathBuf),
    /// A name a file sink doesn't write or lock through: a directory, a FIFO or a socket, or a lock file that is a
    /// link or anything but a regular file.
    NotAFile(PathBuf),
    /// A piece start [check_base] refused for the file at `path`.
    Base { path: PathBuf, refused: BaseRefused },
    /// Nothing to resume at `path`: no file there nor its `.partial`, or a chain's directory that is missing or holds
    /// no first piece.
    NoPiece(PathBuf),
}

impl core::fmt::Display for FsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FsError::Io { what, path, error } => {
                write!(f, "{}: {what}: {}", path.display(), error.kind())
            }
            FsError::Exists(p) => write!(f, "{}: already exists", p.display()),
            FsError::Locked(p) => write!(f, "{}: locked by another writer", p.display()),
            FsError::Replaced(p) => write!(
                f,
                "{}: renamed or replaced while it was opened",
                p.display()
            ),
            FsError::NotAFile(p) => write!(f, "{}: not a regular file", p.display()),
            FsError::NoPiece(p) => write!(f, "{}: no piece to resume", p.display()),
            FsError::Base { path, refused } => {
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
            FsError::Io { error, .. } => Some(error),
            _ => None,
        }
    }
}

/// An [FsError::Io] of `what` on `path`.
pub(crate) fn io_err(what: &'static str, path: &Path) -> impl FnOnce(io::Error) -> FsError {
    move |error| FsError::Io {
        what,
        path: path.to_path_buf(),
        error,
    }
}

/// `offset` in a piece starting at `base`, from the start of the file.
fn file_offset(base: u64, offset: u64) -> io::Result<u64> {
    base.checked_add(offset)
        .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))
}

/// A file written as `NAME.partial` and renamed to `NAME` at its finish, or a file appended to in place.
pub struct FileSink {
    file: File,
    path: PathBuf,
    /// Some while the file is written under its `.partial` name
    partial: Option<PathBuf>,
    /// Where the piece being written starts in the file
    base: u64,
    /// One writer per file; dropped after the file is closed
    _lock: FileLock,
}

impl FileSink {
    /// A new file at `path`, written as `path.partial` until [TapeSink::finish]; the file and its directory are
    /// synced. Fails with [FsError::Exists] if `path` is there or a `path.partial` a crash left holds bytes
    /// ([Self::reopen] resumes it): an empty one is taken, none is ever emptied.
    pub fn create(path: &Path) -> Result<Self, FsError> {
        if path.exists() {
            return Err(FsError::Exists(path.to_path_buf()));
        }
        let lock = FileLock::take(path)?;
        let partial = partial_name(path);
        let file = open_partial(&partial)?;
        Self::claim(lock, file, path, partial)
    }

    /// Take `file`, opened as `partial` for `path` under `lock`: still the file `partial` names (a writer that held
    /// it may have finished and renamed it to `path` meanwhile, [FsError::Replaced]), empty, and `path` not there.
    fn claim(
        mut lock: FileLock,
        file: File,
        path: &Path,
        partial: PathBuf,
    ) -> Result<Self, FsError> {
        if !names(&partial, &file).map_err(io_err("checking the file's name", &partial))? {
            return Err(FsError::Replaced(partial));
        }
        lock.bind(&file, path)?;
        let len = file
            .metadata()
            .map_err(io_err("reading the file's length", &partial))?
            .len();
        if len != 0 {
            return Err(FsError::Exists(partial));
        }
        if path.exists() {
            return Err(FsError::Exists(path.to_path_buf()));
        }
        file.sync_all()
            .map_err(io_err("syncing the new file", &partial))?;
        sync_dir(path).map_err(io_err("syncing the directory", path))?;
        Ok(FileSink {
            file,
            path: path.to_path_buf(),
            partial: Some(partial),
            base: 0,
            _lock: lock,
        })
    }

    /// The file at `path`, appended to in place; created (and its directory synced) if it isn't there. Its length
    /// is where the writer starts.
    pub fn append(path: &Path) -> Result<Self, FsError> {
        let mut lock = FileLock::take(path)?;
        let existed = path.exists();
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(path)
            .map_err(io_err("opening the file", path))?;
        lock.bind(&file, path)?;
        if !existed {
            file.sync_all()
                .map_err(io_err("syncing the new file", path))?;
            sync_dir(path).map_err(io_err("syncing the directory", path))?;
        }
        Ok(FileSink {
            file,
            path: path.to_path_buf(),
            partial: None,
            base: 0,
            _lock: lock,
        })
    }

    /// A new file at `path` written under its own name, for pieces back to back ([Tapes::create_whole]); the
    /// file and its directory are synced. Fails if `path` is there.
    pub fn create_whole(path: &Path) -> Result<Self, FsError> {
        let mut lock = FileLock::take(path)?;
        let file = match OpenOptions::new()
            .read(true)
            .append(true)
            .create_new(true)
            .open(path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                return Err(FsError::Exists(path.to_path_buf()));
            }
            Err(error) => {
                return Err(FsError::Io {
                    what: "creating the file",
                    path: path.to_path_buf(),
                    error,
                });
            }
        };
        lock.bind(&file, path)?;
        file.sync_all()
            .map_err(io_err("syncing the new file", path))?;
        sync_dir(path).map_err(io_err("syncing the directory", path))?;
        Ok(FileSink {
            file,
            path: path.to_path_buf(),
            partial: None,
            base: 0,
            _lock: lock,
        })
    }

    /// The file at `path`, appended to in place, finished or not ([Tapes::open_whole]); None when it isn't
    /// there.
    pub fn open_whole(path: &Path) -> Result<Option<Self>, FsError> {
        let lock = FileLock::take(path)?;
        match OpenOptions::new().read(true).append(true).open(path) {
            Ok(file) => Self::opened(lock, file, path, None).map(Some),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(FsError::Io {
                what: "opening the file",
                path: path.to_path_buf(),
                error,
            }),
        }
    }

    /// The unfinished file `path.partial`, to resume; None when it isn't there.
    pub fn reopen(path: &Path) -> Result<Option<Self>, FsError> {
        let lock = FileLock::take(path)?;
        let partial = partial_name(path);
        // appended to: writes go to the end, after a truncate too
        match OpenOptions::new().read(true).append(true).open(&partial) {
            Ok(file) => Self::opened(lock, file, path, Some(partial)).map(Some),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(FsError::Io {
                what: "opening the unfinished file",
                path: partial,
                error,
            }),
        }
    }

    /// The sink of `file`, opened under `lock` as `partial` (or `path` itself when None), held by its identity too.
    fn opened(
        mut lock: FileLock,
        file: File,
        path: &Path,
        partial: Option<PathBuf>,
    ) -> Result<Self, FsError> {
        lock.bind(&file, path)?;
        Ok(FileSink {
            file,
            path: path.to_path_buf(),
            partial,
            base: 0,
            _lock: lock,
        })
    }
}

impl TapeSink for FileSink {
    type Error = FsError;

    fn write_all(&mut self, bytes: &[u8]) -> Result<(), FsError> {
        self.file
            .write_all(bytes)
            .map_err(io_err("writing the file", self.current_path()))
    }

    fn sync(&mut self) -> Result<(), FsError> {
        self.file
            .sync_data()
            .map_err(io_err("syncing the file", self.current_path()))
    }

    fn finish(&mut self) -> Result<(), FsError> {
        if let Some(partial) = self.partial.take() {
            std::fs::rename(&partial, &self.path)
                .map_err(io_err("renaming the finished file", &partial))?;
            sync_dir(&self.path).map_err(io_err("syncing the directory", &self.path))?;
        }
        Ok(())
    }

    fn size(&self) -> Result<u64, FsError> {
        Ok(self.len()?.saturating_sub(self.base))
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
        file_offset(self.base, offset)
            .and_then(|at| read_file_at(&self.file, at, buf))
            .map_err(io_err("reading the file", self.current_path()))
    }

    fn truncate(&mut self, len: u64) -> Result<(), FsError> {
        let truncate = |file: &mut File| {
            let len = file_offset(self.base, len)?;
            file.set_len(len)?;
            // a file opened for writing at its start writes at the new end too (one opened to append always does)
            io::Seek::seek(file, io::SeekFrom::Start(len))?;
            file.sync_all()
        };
        truncate(&mut self.file).map_err(io_err("truncating the file", self.current_path()))
    }

    fn base(&self) -> u64 {
        self.base
    }

    fn holds_pieces(&self) -> bool {
        self.partial.is_none()
    }

    fn rebase(&mut self, base: u64) -> Result<(), FsError> {
        self.base = check_base(base, self.len()?, self.holds_pieces()).map_err(|refused| {
            FsError::Base {
                path: self.current_path().to_path_buf(),
                refused,
            }
        })?;
        Ok(())
    }
}

impl FileSink {
    /// The name of the file once finished.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The name of the file while it is written (`NAME.partial` for a new file).
    pub fn current_path(&self) -> &Path {
        self.partial.as_deref().unwrap_or(&self.path)
    }

    /// The whole length of the file.
    fn len(&self) -> Result<u64, FsError> {
        Ok(self
            .file
            .metadata()
            .map_err(io_err("reading the file's length", self.current_path()))?
            .len())
    }
}

/// `partial` opened to write, created if it isn't there, its bytes kept.
fn open_partial(partial: &Path) -> Result<File, FsError> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(partial)
        .map_err(io_err("creating the file", partial))
}

/// Whether `path` names `file`: the same device and inode. False when `path` is gone.
#[cfg(unix)]
pub(super) fn names(path: &Path, file: &File) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let opened = file.metadata()?;
    match std::fs::metadata(path) {
        Ok(named) => Ok(named.dev() == opened.dev() && named.ino() == opened.ino()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Whether `path` names `file`: only whether it is there. Stable std has no file identity outside unix (Windows'
/// file index is unstable); there [FileSink::create] relies on refusing a file with bytes.
#[cfg(not(unix))]
pub(super) fn names(path: &Path, _file: &File) -> io::Result<bool> {
    match std::fs::metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

#[cfg(unix)]
fn read_file_at(file: &File, offset: u64, buf: &mut [u8]) -> io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
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
                let n = u64::try_from(n).map_err(|_| past_end())?;
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

/// Piece files: a [Tapes] store of [FileSink]s keyed by a piece's seq, each piece a file of a directory, or every
/// key the one file that holds all pieces of a chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Files(Names);

#[derive(Clone, Debug, PartialEq, Eq)]
enum Names {
    /// Key `seq` is `dir/`[piece_name] (`seq`)
    Dir(PathBuf),
    /// Every key is this file
    File(PathBuf),
}

impl Files {
    /// The pieces in `dir` (which must exist), key `seq` the file `dir/`[piece_name] (`seq`).
    pub fn new(dir: &Path) -> Self {
        Files(Names::Dir(dir.to_path_buf()))
    }

    /// The one file `path` under every key: the file of a chain whose pieces are all in one
    /// ([PieceTapes::One](ww_tape::PieceTapes::One)).
    pub fn file(path: &Path) -> Self {
        Files(Names::File(path.to_path_buf()))
    }

    /// The file of `key`.
    pub fn path_of(&self, key: u64) -> PathBuf {
        match &self.0 {
            Names::Dir(dir) => dir.join(piece_name(key)),
            Names::File(path) => path.clone(),
        }
    }
}

impl Tapes for Files {
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

    fn finished(&self, key: &u64) -> Result<Option<FileSource>, FsError> {
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
}

/// A file read by positional reads, a finished piece or any tape: the [ByteSource] of a file for a
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
        Ok(FileSource {
            file,
            path: path.to_path_buf(),
        })
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

/// `NAME.partial` next to `NAME`.
pub fn partial_name(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".partial");
    PathBuf::from(name)
}

/// Whether neither `path` nor its `.partial` is there: nothing to resume. A name that can't be looked at counts as
/// there, so opening it reports why.
pub(crate) fn none_at(path: &Path) -> bool {
    let gone = |p: &Path| matches!(std::fs::symlink_metadata(p), Err(e) if e.kind() == io::ErrorKind::NotFound);
    gone(path) && gone(&partial_name(path))
}

/// Sync the directory a file is in, so a new or renamed name survives a crash. Windows has no such call (NTFS
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

    /// A create that opened `NAME.partial` while another writer held it, and locks it after that writer finished
    /// and renamed it to `NAME`, refuses: the finished file keeps its bytes.
    #[test]
    fn create_refuses_a_partial_renamed_before_its_lock() {
        let dir = std::env::temp_dir().join(format!("ww_tape_sink_claim_{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.wwt");
        let partial = partial_name(&path);
        std::fs::write(&partial, b"finished bytes").unwrap();
        let file = open_partial(&partial).unwrap();
        std::fs::rename(&partial, &path).unwrap();
        let r = FileSink::claim(FileLock::take(&path).unwrap(), file, &path, partial.clone());
        assert!(
            matches!(&r, Err(FsError::Replaced(p)) if *p == partial),
            "{:?}",
            r.err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"finished bytes");
        // another file under the name now: refused as well
        let file = open_partial(&partial).unwrap();
        std::fs::remove_file(&partial).unwrap();
        std::fs::write(&partial, b"").unwrap();
        let r = FileSink::claim(FileLock::take(&path).unwrap(), file, &path, partial.clone());
        assert!(matches!(&r, Err(FsError::Replaced(_))), "{:?}", r.err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A child process that shares the writer's open file (as a child forked by the writer's process does until it
    /// runs `exec`) holds no lock: the writer's process takes the file again as soon as the writer is dropped. An
    /// `flock` on the file would stay with the child.
    #[cfg(unix)]
    #[test]
    fn a_child_sharing_the_open_file_holds_no_lock() {
        let dir = std::env::temp_dir().join(format!("ww_tape_sink_child_{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.wwt");
        let sink = FileSink::create(&path).unwrap();
        let shared = sink.file.try_clone().unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .env_clear()
            .stdin(std::process::Stdio::null())
            .stdout(shared)
            .spawn()
            .expect("sleep, a POSIX utility, runs the child");
        drop(sink);
        let again = FileSink::reopen(&path);
        // SIGKILL: no core dump, nothing left behind
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(matches!(again, Ok(Some(_))), "{:?}", again.err());
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
        let partial = partial_name(&path);

        let mut sink = FileSink::create(&path).unwrap();
        let e = FileSink::create(&path).err().unwrap();
        assert_eq!(e.to_string(), format!("{shown}: locked by another writer"));
        assert!(e.source().is_none());

        sink.write_all(b"abc").unwrap();
        let e = sink.rebase(1).unwrap_err();
        assert_eq!(
            e.to_string(),
            format!(
                "{}: a piece can't start there: a sink of one piece takes no base but 0",
                partial.display()
            )
        );
        assert!(e.source().is_none());
        sink.finish().unwrap();
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
            format!(
                "{}: finding the file to lock: entity not found",
                missing.display()
            )
        );
        let os = e.source().unwrap().to_string();
        assert!(!e.to_string().contains(&os), "{e} repeats {os}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
