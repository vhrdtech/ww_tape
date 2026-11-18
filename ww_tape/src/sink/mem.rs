//! Pieces in memory: [MemSink], [MemTape] and [MemTapes].

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use super::{SinkFactory, TapeSink, check_base, past_base};
use crate::reader::{ByteSource, wide};
use crate::writer::WriteError;

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
