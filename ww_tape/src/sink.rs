//! [TapeSink]: where a [TapeWriter](crate::TapeWriter) puts its bytes, a file ([FileSink]) or memory ([MemSink]).
//!
//! A sink appends, syncs, finishes and, for [TapeWriter::resume_on](crate::TapeWriter::resume_on), reads back and
//! truncates. A [SinkFactory] makes the sink of each piece of a [PieceChain](crate::PieceChain) by its path: [Files]
//! creates `NAME.partial` and renames it to `NAME` at the finish, [MemTapes] keeps each piece in memory under its
//! path, where a [TapeReader](crate::TapeReader) reads it while it is written ([MemTape] is a [ByteSource]).
//!
//! A chain may also keep all its pieces in one file or memory tape ([SinkFactory::create_whole],
//! [SinkFactory::open_whole]): the writer of the next piece goes on in the same sink, whose window
//! ([TapeSink::base], [TapeSink::rebase]) starts where the piece before ended, so offsets, reads and truncates count
//! from the piece's start as in a file of its own.
//!
//! A sink is the only writer of its bytes while it lives: a file sink holds an exclusive lock on its file
//! ([File::try_lock], advisory: other writers of the format take it too), a memory sink marks its tape held. A second
//! sink of the same bytes is refused with [WriteError::Locked], so two writers never fork one file.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::reader::{ByteSource, wide};
use crate::writer::WriteError;

/// Where a [TapeWriter](crate::TapeWriter) appends its bytes.
pub trait TapeSink {
    /// Append `bytes` at the end.
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()>;

    /// Make the bytes written so far durable.
    fn sync(&mut self) -> io::Result<()>;

    /// The bytes are whole: a file takes its final name and its directory is synced; memory does nothing. Called
    /// once, after the last sync.
    fn finish(&mut self) -> io::Result<()>;

    /// Bytes in the sink.
    fn size(&self) -> io::Result<u64>;

    /// Fill `buf` from `offset`, within [Self::size].
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()>;

    /// Drop every byte from `len` on: what resume keeps.
    fn truncate(&mut self, len: u64) -> io::Result<()>;

    /// Make room for `bytes` more, so that appending them never moves what is written: a hint, nothing changes when
    /// it can't be met. A file does nothing.
    fn reserve(&mut self, _bytes: u64) {}

    /// The name of the bytes once finished.
    fn path(&self) -> &Path;

    /// The name of the bytes while they are written (`NAME.partial` for a new file).
    fn current_path(&self) -> &Path {
        self.path()
    }

    /// Where the piece being written starts in the bytes: [Self::size], [Self::read_at] and [Self::truncate] count
    /// from here. 0 unless [Self::rebase] moved it.
    fn base(&self) -> u64 {
        0
    }

    /// Whether the sink holds pieces back to back ([SinkFactory::create_whole], [SinkFactory::open_whole], a file
    /// appended to in place), so a writer may cut in place ([TapeWriter::cut_in_place](crate::TapeWriter::cut_in_place)).
    /// A sink of one piece (`NAME.partial`, a piece of a chain of files) doesn't.
    fn holds_pieces(&self) -> bool {
        false
    }

    /// Let the piece being written start at `base` in the bytes, at most their whole length: the next piece of a
    /// file of several, or the last piece of one to resume. A sink that holds one piece only refuses any base but 0
    /// ([io::ErrorKind::Unsupported]).
    fn rebase(&mut self, base: u64) -> io::Result<()> {
        if base == 0 {
            Ok(())
        } else {
            Err(io::ErrorKind::Unsupported.into())
        }
    }
}

/// The whole length of the bytes of a sink whose window starts at `base` and holds `size` bytes, checked against a
/// new base; any base but 0 refused for a sink of one piece (`pieces` false).
fn check_base(base: u64, whole: u64, pieces: bool) -> io::Result<u64> {
    if base != 0 && !pieces {
        return Err(io::ErrorKind::Unsupported.into());
    }
    if base > whole {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a piece can't start past the end of the bytes",
        ));
    }
    Ok(base)
}

/// Makes the sink of each piece of a chain by its path, reopens an unfinished one and reads a finished one.
pub trait SinkFactory {
    type Sink: TapeSink;
    /// A finished piece, read back.
    type Source: ByteSource<Error = io::Error>;

    /// A new sink for `path`; [WriteError::Exists] when `path` is there, finished or not.
    fn create(&mut self, path: &Path) -> Result<Self::Sink, WriteError>;

    /// The unfinished sink of `path`, to resume; None when there is none.
    fn reopen(&mut self, path: &Path) -> Result<Option<Self::Sink>, WriteError>;

    /// The finished bytes of `path`; None when `path` isn't finished.
    fn finished(&self, path: &Path) -> Result<Option<Self::Source>, WriteError>;

    /// A new sink for a file of several pieces at `path`, written under its own name from the start (a crash
    /// leaves its last piece unfinished, which a resume finds); [WriteError::Exists] when `path` is there. A
    /// factory without files of several pieces refuses ([io::ErrorKind::Unsupported]).
    fn create_whole(&mut self, path: &Path) -> Result<Self::Sink, WriteError> {
        Err(unsupported(path))
    }

    /// The file of several pieces at `path`, appended to in place, finished or not; None when there is none.
    fn open_whole(&mut self, path: &Path) -> Result<Option<Self::Sink>, WriteError> {
        Err(unsupported(path))
    }
}

/// A factory that holds no files of several pieces.
fn unsupported(_path: &Path) -> WriteError {
    WriteError::Io {
        what: "opening a file of several pieces",
        error: io::ErrorKind::Unsupported.into(),
    }
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

/// `offset` in a piece starting at `base`, from the start of the bytes.
fn past_base(base: u64, offset: u64) -> io::Result<u64> {
    base.checked_add(offset)
        .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))
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

/// Bytes in memory that a [MemSink] writes and readers read at the same time: a [ByteSource] whose size grows.
#[derive(Clone, Debug, Default)]
pub struct MemTape(Arc<MemBytes>);

#[derive(Debug, Default)]
struct MemBytes {
    bytes: RwLock<Vec<u8>>,
    finished: AtomicBool,
    /// A [MemSink] writes the bytes
    held: AtomicBool,
}

impl MemTape {
    /// A copy of the bytes.
    pub fn to_vec(&self) -> io::Result<Vec<u8>> {
        Ok(self.read()?.clone())
    }

    /// Whether the sink writing these bytes finished.
    pub fn is_finished(&self) -> bool {
        self.0.finished.load(Ordering::Acquire)
    }

    /// A sink named `path` writing these bytes, the only one while it lives: [WriteError::Locked] while another
    /// one does.
    fn hold(&self, path: &Path) -> Result<MemSink, WriteError> {
        self.0
            .held
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_held| WriteError::Locked(path.to_path_buf()))?;
        Ok(MemSink {
            tape: self.clone(),
            path: path.to_path_buf(),
            base: 0,
            whole: false,
        })
    }

    fn read(&self) -> io::Result<std::sync::RwLockReadGuard<'_, Vec<u8>>> {
        self.0.bytes.read().map_err(|_poisoned| poisoned())
    }

    fn write(&self) -> io::Result<std::sync::RwLockWriteGuard<'_, Vec<u8>>> {
        self.0.bytes.write().map_err(|_poisoned| poisoned())
    }
}

/// A writer panicked while it held the bytes: they may be half written.
fn poisoned() -> io::Error {
    io::Error::other("a writer of the memory tape panicked")
}

impl ByteSource for MemTape {
    type Error = io::Error;

    fn size(&self) -> io::Result<u64> {
        Ok(wide(self.read()?.len()))
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let bytes = self.read()?;
        let src = usize::try_from(offset)
            .ok()
            .and_then(|from| Some(from..from.checked_add(buf.len())?))
            .and_then(|r| bytes.get(r))
            .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
        buf.copy_from_slice(src);
        Ok(())
    }
}

/// A piece in memory under a path, readers reading its [MemTape] as it grows.
#[derive(Debug)]
pub struct MemSink {
    tape: MemTape,
    path: PathBuf,
    /// Where the piece being written starts in the bytes
    base: u64,
    /// The tape holds pieces back to back ([SinkFactory::create_whole], [SinkFactory::open_whole])
    whole: bool,
}

impl MemSink {
    /// An empty sink named `path`, not kept in any [MemTapes].
    pub fn new(path: &Path) -> Self {
        let tape = MemTape::default();
        tape.0.held.store(true, Ordering::Release);
        MemSink {
            tape,
            path: path.to_path_buf(),
            base: 0,
            whole: false,
        }
    }

    /// The bytes, for readers.
    pub fn tape(&self) -> MemTape {
        self.tape.clone()
    }
}

impl Drop for MemSink {
    /// The bytes are free for the next writer.
    fn drop(&mut self) {
        self.tape.0.held.store(false, Ordering::Release);
    }
}

impl TapeSink for MemSink {
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.tape.write()?.extend_from_slice(bytes);
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn finish(&mut self) -> io::Result<()> {
        self.tape.0.finished.store(true, Ordering::Release);
        Ok(())
    }

    fn size(&self) -> io::Result<u64> {
        Ok(ByteSource::size(&self.tape)?.saturating_sub(self.base))
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        ByteSource::read_at(&self.tape, past_base(self.base, offset)?, buf)
    }

    fn base(&self) -> u64 {
        self.base
    }

    fn holds_pieces(&self) -> bool {
        self.whole
    }

    fn rebase(&mut self, base: u64) -> io::Result<()> {
        self.base = check_base(base, ByteSource::size(&self.tape)?, self.whole)?;
        Ok(())
    }

    fn reserve(&mut self, bytes: u64) {
        if let (Ok(more), Ok(mut vec)) = (usize::try_from(bytes), self.tape.write()) {
            // one allocation up front: growing by doubling copies the piece under the write lock, and `read_at` waits
            _ = vec.try_reserve_exact(more);
        }
    }

    fn truncate(&mut self, len: u64) -> io::Result<()> {
        let len = past_base(self.base, len)?;
        let mut bytes = self.tape.write()?;
        if let Ok(len) = usize::try_from(len) {
            bytes.truncate(len);
        }
        Ok(())
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

/// Pieces in memory by path, shared by every clone: the factory of an in-memory store, and where its readers find
/// each piece ([Self::get]).
#[derive(Clone, Debug, Default)]
pub struct MemTapes(Arc<Mutex<BTreeMap<PathBuf, MemTape>>>);

impl MemTapes {
    pub fn new() -> Self {
        Self::default()
    }

    /// The bytes under `path`, finished or not.
    pub fn get(&self, path: &Path) -> Result<Option<MemTape>, WriteError> {
        Ok(self.lock()?.get(path).cloned())
    }

    /// Take the finished piece under `path` out of the map: true when it was there, false when it was not.
    ///
    /// A [MemTape] a reader already holds keeps its bytes (they are freed with the last holder), and reads on as
    /// before: nothing a reader holds ever changes or fails. After the call [Self::get] and [SinkFactory::finished]
    /// find nothing under `path`. A piece still being written is not removed: [WriteError::Unfinished], so a writer
    /// never writes into bytes nobody can find. Removing the oldest pieces of a chain leaves a head cut
    /// ([ChainReport::head_cut](crate::piece::ChainReport::head_cut)); a [PieceChain](crate::PieceChain) resumes only
    /// from its first piece, so a chain with removed pieces is not resumed.
    pub fn remove(&self, path: &Path) -> Result<bool, WriteError> {
        let mut map = self.lock()?;
        match map.get(path) {
            None => Ok(false),
            Some(t) if !t.is_finished() => Err(WriteError::Unfinished(path.to_path_buf())),
            Some(_) => Ok(map.remove(path).is_some()),
        }
    }

    /// Every path with bytes, in order.
    pub fn paths(&self) -> Result<Vec<PathBuf>, WriteError> {
        Ok(self.lock()?.keys().cloned().collect())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, BTreeMap<PathBuf, MemTape>>, WriteError> {
        self.0.lock().map_err(|_poisoned| WriteError::Io {
            what: "listing the memory tapes",
            error: poisoned(),
        })
    }
}

impl SinkFactory for MemTapes {
    type Sink = MemSink;
    type Source = MemTape;

    fn create(&mut self, path: &Path) -> Result<MemSink, WriteError> {
        let mut map = self.lock()?;
        if map.contains_key(path) {
            return Err(WriteError::Exists(path.to_path_buf()));
        }
        let sink = MemSink::new(path);
        map.insert(path.to_path_buf(), sink.tape());
        Ok(sink)
    }

    fn reopen(&mut self, path: &Path) -> Result<Option<MemSink>, WriteError> {
        self.lock()?
            .get(path)
            .filter(|t| !t.is_finished())
            .map(|tape| tape.hold(path))
            .transpose()
    }

    fn finished(&self, path: &Path) -> Result<Option<MemTape>, WriteError> {
        Ok(self.lock()?.get(path).filter(|t| t.is_finished()).cloned())
    }

    fn create_whole(&mut self, path: &Path) -> Result<MemSink, WriteError> {
        let mut sink = self.create(path)?;
        sink.whole = true;
        Ok(sink)
    }

    /// The tape under `path`, which is written again: it reads as unfinished until its next finish.
    fn open_whole(&mut self, path: &Path) -> Result<Option<MemSink>, WriteError> {
        let map = self.lock()?;
        let Some(tape) = map.get(path) else {
            return Ok(None);
        };
        let mut sink = tape.hold(path)?;
        sink.whole = true;
        tape.0.finished.store(false, Ordering::Release);
        Ok(Some(sink))
    }
}

/// `NAME.partial` next to `NAME`.
pub fn partial_name(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".partial");
    PathBuf::from(name)
}

pub(crate) fn io_err(what: &'static str) -> impl FnOnce(io::Error) -> WriteError {
    move |error| WriteError::Io { what, error }
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
