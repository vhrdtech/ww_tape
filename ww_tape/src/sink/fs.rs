//! Piece files: [FileSink], [Files] and [FileSource].

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::{SinkFactory, TapeSink, check_base, io_err, past_base};
use crate::reader::ByteSource;
use crate::writer::WriteError;

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
    /// synced. Fails with [WriteError::Exists] if `path` is there or a `path.partial` a crash left holds bytes
    /// ([Self::reopen] resumes it): an empty one is taken, none is ever emptied.
    pub fn create(path: &Path) -> Result<Self, WriteError> {
        if path.exists() {
            return Err(WriteError::Exists(path.to_path_buf()));
        }
        let partial = partial_name(path);
        let file = open_partial(&partial)?;
        Self::claim(file, path, partial)
    }

    /// Take `file`, opened as `partial` for `path`, once locked: still the file `partial` names (a writer that held
    /// it may have finished and renamed it to `path` meanwhile, [WriteError::Replaced]), empty, and `path` not there.
    fn claim(file: File, path: &Path, partial: PathBuf) -> Result<Self, WriteError> {
        lock(&file, &partial)?;
        if !names(&partial, &file).map_err(io_err("checking the file's name"))? {
            return Err(WriteError::Replaced(partial));
        }
        let len = file
            .metadata()
            .map_err(io_err("reading the file's length"))?
            .len();
        if len != 0 {
            return Err(WriteError::Exists(partial));
        }
        if path.exists() {
            return Err(WriteError::Exists(path.to_path_buf()));
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
    pub fn append(path: &Path) -> Result<Self, WriteError> {
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

    /// A new file at `path` written under its own name, for pieces back to back ([SinkFactory::create_whole]); the
    /// file and its directory are synced. Fails if `path` is there.
    pub fn create_whole(path: &Path) -> Result<Self, WriteError> {
        let file = match OpenOptions::new()
            .read(true)
            .append(true)
            .create_new(true)
            .open(path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                return Err(WriteError::Exists(path.to_path_buf()));
            }
            Err(error) => {
                return Err(WriteError::Io {
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

    /// The file at `path`, appended to in place, finished or not ([SinkFactory::open_whole]); None when it isn't
    /// there.
    pub fn open_whole(path: &Path) -> Result<Option<Self>, WriteError> {
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
            Err(error) => Err(WriteError::Io {
                what: "opening the file",
                error,
            }),
        }
    }

    /// The unfinished file `path.partial`, to resume; None when it isn't there.
    pub fn reopen(path: &Path) -> Result<Option<Self>, WriteError> {
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
            Err(error) => Err(WriteError::Io {
                what: "opening the unfinished file",
                error,
            }),
        }
    }
}

impl TapeSink for FileSink {
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.file.write_all(bytes)
    }

    fn sync(&mut self) -> io::Result<()> {
        self.file.sync_data()
    }

    fn finish(&mut self) -> io::Result<()> {
        if let Some(partial) = self.partial.take() {
            std::fs::rename(&partial, &self.path)?;
            sync_dir(&self.path)?;
        }
        Ok(())
    }

    fn size(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len().saturating_sub(self.base))
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        read_file_at(&self.file, past_base(self.base, offset)?, buf)
    }

    fn truncate(&mut self, len: u64) -> io::Result<()> {
        let len = past_base(self.base, len)?;
        self.file.set_len(len)?;
        // a file opened for writing at its start writes at the new end too (one opened to append always does)
        io::Seek::seek(&mut self.file, io::SeekFrom::Start(len))?;
        self.file.sync_all()
    }

    fn base(&self) -> u64 {
        self.base
    }

    fn holds_pieces(&self) -> bool {
        self.partial.is_none()
    }

    fn rebase(&mut self, base: u64) -> io::Result<()> {
        self.base = check_base(base, self.file.metadata()?.len(), self.holds_pieces())?;
        Ok(())
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn current_path(&self) -> &Path {
        self.partial.as_deref().unwrap_or(&self.path)
    }
}

/// `partial` opened to write, created if it isn't there, its bytes kept.
fn open_partial(partial: &Path) -> Result<File, WriteError> {
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

/// Take the exclusive lock on `file`, named `path`, held until it is closed: [WriteError::Locked] when another
/// writer holds it.
fn lock(file: &File, path: &Path) -> Result<(), WriteError> {
    match file.try_lock() {
        Ok(()) => Ok(()),
        Err(std::fs::TryLockError::WouldBlock) => Err(WriteError::Locked(path.to_path_buf())),
        Err(std::fs::TryLockError::Error(error)) => Err(WriteError::Io {
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

/// Piece files in directories: [FileSink]s.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Files;

impl SinkFactory for Files {
    type Sink = FileSink;
    type Source = FileSource;

    fn create(&mut self, path: &Path) -> Result<FileSink, WriteError> {
        FileSink::create(path)
    }

    fn reopen(&mut self, path: &Path) -> Result<Option<FileSink>, WriteError> {
        FileSink::reopen(path)
    }

    fn finished(&self, path: &Path) -> Result<Option<FileSource>, WriteError> {
        match File::open(path) {
            Ok(file) => Ok(Some(FileSource(file))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(WriteError::Io {
                what: "opening a finished piece",
                error,
            }),
        }
    }

    fn create_whole(&mut self, path: &Path) -> Result<FileSink, WriteError> {
        FileSink::create_whole(path)
    }

    fn open_whole(&mut self, path: &Path) -> Result<Option<FileSink>, WriteError> {
        FileSink::open_whole(path)
    }
}

/// A finished piece file, read by positional reads.
#[derive(Debug)]
pub struct FileSource(File);

impl ByteSource for FileSource {
    type Error = io::Error;

    fn size(&self) -> io::Result<u64> {
        Ok(self.0.metadata()?.len())
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        read_file_at(&self.0, offset, buf)
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
            matches!(&r, Err(WriteError::Replaced(p)) if *p == partial),
            "{:?}",
            r.err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"finished bytes");
        // another file under the name now: refused as well
        let file = open_partial(&partial).unwrap();
        std::fs::remove_file(&partial).unwrap();
        std::fs::write(&partial, b"").unwrap();
        let r = FileSink::claim(file, &path, partial.clone());
        assert!(matches!(&r, Err(WriteError::Replaced(_))), "{:?}", r.err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
