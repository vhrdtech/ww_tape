//! Piece files: [FileSink], [Files] and [FileSource].

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::{BaseRefused, TapeSink, Tapes, check_base, past_base};
use crate::piece::piece_name;
use crate::reader::ByteSource;

/// Why a [FileSink], [Files] or [FileSource] failed or refused.
#[derive(Debug)]
#[non_exhaustive]
pub enum FsError {
    /// Opening, writing, syncing, renaming or reading failed, with what was being done.
    Io {
        what: &'static str,
        error: io::Error,
    },
    /// A new file that is already there (or a `.partial` a crash left that holds bytes).
    Exists(PathBuf),
    /// Another writer holds the file: one writer per file ([crate::sink]).
    Locked(PathBuf),
    /// The `.partial` file `create` opened is no longer the one its name points to once locked: another writer
    /// finished and renamed it, or it was replaced. Nothing is written.
    Replaced(PathBuf),
}

impl core::fmt::Display for FsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FsError::Io { what, error } => write!(f, "{what}: {error}"),
            FsError::Exists(p) => write!(f, "{} exists", p.display()),
            FsError::Locked(p) => write!(f, "another writer holds {}", p.display()),
            FsError::Replaced(p) => write!(
                f,
                "{} was renamed or replaced while it was opened",
                p.display()
            ),
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

/// An [FsError::Io] of `what`.
fn io_err(what: &'static str) -> impl FnOnce(io::Error) -> FsError {
    move |error| FsError::Io { what, error }
}

/// A new base [check_base] refused, as the io error a file sink reports.
fn base_err(refused: BaseRefused) -> FsError {
    let error = match refused {
        BaseRefused::OnePiece => io::ErrorKind::Unsupported.into(),
        BaseRefused::PastEnd => io::Error::new(
            io::ErrorKind::InvalidInput,
            "a piece can't start past the end of the bytes",
        ),
    };
    FsError::Io {
        what: "moving the start of the piece",
        error,
    }
}

/// `offset` in a piece starting at `base`, from the start of the file.
fn file_offset(base: u64, offset: u64) -> io::Result<u64> {
    past_base(base, offset).ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))
}

/// A file written as `NAME.partial` and renamed to `NAME` at its finish, or a file appended to in place.
pub struct FileSink {
    file: File,
    path: PathBuf,
    /// Some while the file is written under its `.partial` name
    partial: Option<PathBuf>,
    /// Where the piece being written starts in the file
    base: u64,
}

impl FileSink {
    /// A new file at `path`, written as `path.partial` until [TapeSink::finish]; the file and its directory are
    /// synced. Fails with [FsError::Exists] if `path` is there or a `path.partial` a crash left holds bytes
    /// ([Self::reopen] resumes it): an empty one is taken, none is ever emptied.
    pub fn create(path: &Path) -> Result<Self, FsError> {
        if path.exists() {
            return Err(FsError::Exists(path.to_path_buf()));
        }
        let partial = partial_name(path);
        let file = open_partial(&partial)?;
        Self::claim(file, path, partial)
    }

    /// Take `file`, opened as `partial` for `path`, once locked: still the file `partial` names (a writer that held
    /// it may have finished and renamed it to `path` meanwhile, [FsError::Replaced]), empty, and `path` not there.
    fn claim(file: File, path: &Path, partial: PathBuf) -> Result<Self, FsError> {
        lock(&file, &partial)?;
        if !names(&partial, &file).map_err(io_err("checking the file's name"))? {
            return Err(FsError::Replaced(partial));
        }
        let len = file
            .metadata()
            .map_err(io_err("reading the file's length"))?
            .len();
        if len != 0 {
            return Err(FsError::Exists(partial));
        }
        if path.exists() {
            return Err(FsError::Exists(path.to_path_buf()));
        }
        file.sync_all().map_err(io_err("syncing the new file"))?;
        sync_dir(path).map_err(io_err("syncing the directory"))?;
        Ok(FileSink {
            file,
            path: path.to_path_buf(),
            partial: Some(partial),
            base: 0,
        })
    }

    /// The file at `path`, appended to in place; created (and its directory synced) if it isn't there. Its length
    /// is where the writer starts.
    pub fn append(path: &Path) -> Result<Self, FsError> {
        let existed = path.exists();
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(path)
            .map_err(io_err("opening the file"))?;
        lock(&file, path)?;
        if !existed {
            file.sync_all().map_err(io_err("syncing the new file"))?;
            sync_dir(path).map_err(io_err("syncing the directory"))?;
        }
        Ok(FileSink {
            file,
            path: path.to_path_buf(),
            partial: None,
            base: 0,
        })
    }

    /// A new file at `path` written under its own name, for pieces back to back ([Tapes::create_whole]); the
    /// file and its directory are synced. Fails if `path` is there.
    pub fn create_whole(path: &Path) -> Result<Self, FsError> {
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
                    error,
                });
            }
        };
        lock(&file, path)?;
        file.sync_all().map_err(io_err("syncing the new file"))?;
        sync_dir(path).map_err(io_err("syncing the directory"))?;
        Ok(FileSink {
            file,
            path: path.to_path_buf(),
            partial: None,
            base: 0,
        })
    }

    /// The file at `path`, appended to in place, finished or not ([Tapes::open_whole]); None when it isn't
    /// there.
    pub fn open_whole(path: &Path) -> Result<Option<Self>, FsError> {
        match OpenOptions::new().read(true).append(true).open(path) {
            Ok(file) => {
                lock(&file, path)?;
                Ok(Some(FileSink {
                    file,
                    path: path.to_path_buf(),
                    partial: None,
                    base: 0,
                }))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(FsError::Io {
                what: "opening the file",
                error,
            }),
        }
    }

    /// The unfinished file `path.partial`, to resume; None when it isn't there.
    pub fn reopen(path: &Path) -> Result<Option<Self>, FsError> {
        let partial = partial_name(path);
        // appended to: writes go to the end, after a truncate too
        match OpenOptions::new().read(true).append(true).open(&partial) {
            Ok(file) => {
                lock(&file, &partial)?;
                Ok(Some(FileSink {
                    file,
                    path: path.to_path_buf(),
                    partial: Some(partial),
                    base: 0,
                }))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(FsError::Io {
                what: "opening the unfinished file",
                error,
            }),
        }
    }
}

impl TapeSink for FileSink {
    type Error = FsError;

    fn write_all(&mut self, bytes: &[u8]) -> Result<(), FsError> {
        self.file
            .write_all(bytes)
            .map_err(io_err("writing the file"))
    }

    fn sync(&mut self) -> Result<(), FsError> {
        self.file.sync_data().map_err(io_err("syncing the file"))
    }

    fn finish(&mut self) -> Result<(), FsError> {
        if let Some(partial) = self.partial.take() {
            std::fs::rename(&partial, &self.path).map_err(io_err("renaming the finished file"))?;
            sync_dir(&self.path).map_err(io_err("syncing the directory"))?;
        }
        Ok(())
    }

    fn size(&self) -> Result<u64, FsError> {
        Ok(self.len()?.saturating_sub(self.base))
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
        file_offset(self.base, offset)
            .and_then(|at| read_file_at(&self.file, at, buf))
            .map_err(io_err("reading the file"))
    }

    fn truncate(&mut self, len: u64) -> Result<(), FsError> {
        let truncate = |file: &mut File| {
            let len = file_offset(self.base, len)?;
            file.set_len(len)?;
            // a file opened for writing at its start writes at the new end too (one opened to append always does)
            io::Seek::seek(file, io::SeekFrom::Start(len))?;
            file.sync_all()
        };
        truncate(&mut self.file).map_err(io_err("truncating the file"))
    }

    fn base(&self) -> u64 {
        self.base
    }

    fn holds_pieces(&self) -> bool {
        self.partial.is_none()
    }

    fn rebase(&mut self, base: u64) -> Result<(), FsError> {
        self.base = check_base(base, self.len()?, self.holds_pieces()).map_err(base_err)?;
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
            .map_err(io_err("reading the file's length"))?
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
        .map_err(io_err("creating the file"))
}

/// Whether `path` names `file`: the same device and inode. False when `path` is gone.
#[cfg(unix)]
fn names(path: &Path, file: &File) -> io::Result<bool> {
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
fn names(path: &Path, _file: &File) -> io::Result<bool> {
    match std::fs::metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Take the exclusive lock on `file`, named `path`, held until it is closed: [FsError::Locked] when another
/// writer holds it.
fn lock(file: &File, path: &Path) -> Result<(), FsError> {
    match file.try_lock() {
        Ok(()) => Ok(()),
        Err(std::fs::TryLockError::WouldBlock) => Err(FsError::Locked(path.to_path_buf())),
        Err(std::fs::TryLockError::Error(error)) => Err(FsError::Io {
            what: "locking the file",
            error,
        }),
    }
}

#[cfg(unix)]
fn read_file_at(file: &File, offset: u64, buf: &mut [u8]) -> io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

#[cfg(not(unix))]
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
    /// ([PieceTapes::One](crate::PieceTapes::One)).
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
        match File::open(self.path_of(*key)) {
            Ok(file) => Ok(Some(FileSource(file))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(FsError::Io {
                what: "opening a finished piece",
                error,
            }),
        }
    }

    fn create_whole(&mut self, key: &u64) -> Result<FileSink, FsError> {
        FileSink::create_whole(&self.path_of(*key))
    }

    fn open_whole(&mut self, key: &u64) -> Result<Option<FileSink>, FsError> {
        FileSink::open_whole(&self.path_of(*key))
    }
}

/// A finished piece file, read by positional reads.
#[derive(Debug)]
pub struct FileSource(File);

impl ByteSource for FileSource {
    type Error = FsError;

    fn size(&self) -> Result<u64, FsError> {
        Ok(self
            .0
            .metadata()
            .map_err(io_err("reading the file's length"))?
            .len())
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
        read_file_at(&self.0, offset, buf).map_err(io_err("reading the file"))
    }
}

/// `NAME.partial` next to `NAME`.
pub fn partial_name(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".partial");
    PathBuf::from(name)
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
        let r = FileSink::claim(file, &path, partial.clone());
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
        let r = FileSink::claim(file, &path, partial.clone());
        assert!(matches!(&r, Err(FsError::Replaced(_))), "{:?}", r.err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
