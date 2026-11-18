//! Pieces in memory: [MemSink], [MemTape] and [MemTapes].

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use super::{BaseRefused, TapeSink, Tapes, check_base, past_base};
use crate::reader::{ByteSource, OutOfRange, wide};

/// Why a [MemSink], [MemTape] or [MemTapes] failed or refused.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum MemError {
    /// A new tape under a name that has one.
    Exists,
    /// Another sink writes the tape: one writer per tape ([crate::sink]).
    Locked,
    /// [MemTapes::remove] of a tape still being written.
    Unfinished,
    /// A writer panicked while it held the bytes: they may be half written.
    Poisoned,
    /// A read past the end of the bytes.
    OutOfRange,
    /// A piece start refused: past the end of the bytes, or any but 0 on a sink of one piece.
    Base,
}

impl core::fmt::Display for MemError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            MemError::Exists => "the memory tape exists",
            MemError::Locked => "another writer holds the memory tape",
            MemError::Unfinished => "the memory tape is still being written",
            MemError::Poisoned => "a writer of the memory tape panicked",
            MemError::OutOfRange => "a read past the end of the memory tape",
            MemError::Base => {
                "a piece can't start there: past the end, or in a memory tape of one piece"
            }
        })
    }
}

impl core::error::Error for MemError {}

impl From<OutOfRange> for MemError {
    fn from(_: OutOfRange) -> Self {
        MemError::OutOfRange
    }
}

impl From<BaseRefused> for MemError {
    fn from(_: BaseRefused) -> Self {
        MemError::Base
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
    pub fn to_vec(&self) -> Result<Vec<u8>, MemError> {
        Ok(self.read()?.clone())
    }

    /// Whether the sink writing these bytes finished.
    pub fn is_finished(&self) -> bool {
        self.0.finished.load(Ordering::Acquire)
    }

    /// A sink writing these bytes, the only one while it lives: [MemError::Locked] while another one does.
    fn hold(&self) -> Result<MemSink, MemError> {
        self.0
            .held
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_held| MemError::Locked)?;
        Ok(MemSink {
            tape: self.clone(),
            base: 0,
            whole: false,
        })
    }

    fn read(&self) -> Result<std::sync::RwLockReadGuard<'_, Vec<u8>>, MemError> {
        self.0.bytes.read().map_err(|_poisoned| MemError::Poisoned)
    }

    fn write(&self) -> Result<std::sync::RwLockWriteGuard<'_, Vec<u8>>, MemError> {
        self.0.bytes.write().map_err(|_poisoned| MemError::Poisoned)
    }
}

impl ByteSource for MemTape {
    type Error = MemError;

    fn size(&self) -> Result<u64, MemError> {
        Ok(wide(self.read()?.len()))
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), MemError> {
        Ok(self.read()?.as_slice().read_at(offset, buf)?)
    }
}

/// A piece in memory, readers reading its [MemTape] as it grows.
#[derive(Debug)]
pub struct MemSink {
    tape: MemTape,
    /// Where the piece being written starts in the bytes
    base: u64,
    /// The tape holds pieces back to back ([Tapes::create_whole], [Tapes::open_whole])
    whole: bool,
}

impl MemSink {
    /// An empty sink, not kept in any [MemTapes].
    pub fn new() -> Self {
        let tape = MemTape::default();
        tape.0.held.store(true, Ordering::Release);
        MemSink {
            tape,
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
    type Error = MemError;

    fn write_all(&mut self, bytes: &[u8]) -> Result<(), MemError> {
        self.tape.write()?.extend_from_slice(bytes);
        Ok(())
    }

    fn sync(&mut self) -> Result<(), MemError> {
        Ok(())
    }

    fn finish(&mut self) -> Result<(), MemError> {
        self.tape.0.finished.store(true, Ordering::Release);
        Ok(())
    }

    fn size(&self) -> Result<u64, MemError> {
        Ok(ByteSource::size(&self.tape)?.saturating_sub(self.base))
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), MemError> {
        let at = past_base(self.base, offset).ok_or(MemError::OutOfRange)?;
        ByteSource::read_at(&self.tape, at, buf)
    }

    fn base(&self) -> u64 {
        self.base
    }

    fn holds_pieces(&self) -> bool {
        self.whole
    }

    fn rebase(&mut self, base: u64) -> Result<(), MemError> {
        self.base = check_base(base, ByteSource::size(&self.tape)?, self.whole)?;
        Ok(())
    }

    fn reserve(&mut self, bytes: u64) {
        if let (Ok(more), Ok(mut vec)) = (usize::try_from(bytes), self.tape.write()) {
            // one allocation up front: growing by doubling copies the piece under the write lock, and `read_at` waits
            _ = vec.try_reserve_exact(more);
        }
    }

    fn truncate(&mut self, len: u64) -> Result<(), MemError> {
        let len = past_base(self.base, len).ok_or(MemError::OutOfRange)?;
        let mut bytes = self.tape.write()?;
        if let Ok(len) = usize::try_from(len) {
            bytes.truncate(len);
        }
        Ok(())
    }
}

impl Default for MemSink {
    fn default() -> Self {
        Self::new()
    }
}

/// Pieces in memory by key, shared by every clone: a store of [Tapes] (keyed by a piece's seq with `K = u64`), and
/// where its readers find each piece ([Self::get]). Another key lets an application keep the pieces of many chains
/// in one map, behind a [Tapes] of its own that names each chain's keys.
#[derive(Clone, Debug)]
pub struct MemTapes<K = u64>(Arc<Mutex<BTreeMap<K, MemTape>>>);

impl<K> Default for MemTapes<K> {
    fn default() -> Self {
        MemTapes(Arc::default())
    }
}

impl<K: Ord + Clone> MemTapes<K> {
    pub fn new() -> Self {
        Self::default()
    }

    /// The bytes under `key`, finished or not.
    pub fn get(&self, key: &K) -> Result<Option<MemTape>, MemError> {
        Ok(self.lock()?.get(key).cloned())
    }

    /// Take the finished piece under `key` out of the map: true when it was there, false when it was not.
    ///
    /// A [MemTape] a reader already holds keeps its bytes (they are freed with the last holder), and reads on as
    /// before: nothing a reader holds ever changes or fails. After the call [Self::get] and [Self::finished] find
    /// nothing under `key`. A piece still being written is not removed: [MemError::Unfinished], so a writer never
    /// writes into bytes nobody can find. Removing the oldest pieces of a chain leaves a head cut
    /// ([ChainReport::head_cut](crate::piece::ChainReport::head_cut)); a [PieceChain](crate::PieceChain) resumes only
    /// from its first piece, so a chain with removed pieces is not resumed.
    pub fn remove(&self, key: &K) -> Result<bool, MemError> {
        let mut map = self.lock()?;
        match map.get(key) {
            None => Ok(false),
            Some(t) if !t.is_finished() => Err(MemError::Unfinished),
            Some(_) => Ok(map.remove(key).is_some()),
        }
    }

    /// Every key with bytes, in order.
    pub fn keys(&self) -> Result<Vec<K>, MemError> {
        Ok(self.lock()?.keys().cloned().collect())
    }

    /// A new sink under `key` ([Tapes::create]): [MemError::Exists] when the key has a tape.
    pub fn create(&self, key: &K) -> Result<MemSink, MemError> {
        let mut map = self.lock()?;
        if map.contains_key(key) {
            return Err(MemError::Exists);
        }
        let sink = MemSink::new();
        map.insert(key.clone(), sink.tape());
        Ok(sink)
    }

    /// The unfinished tape under `key`, to resume ([Tapes::reopen]).
    pub fn reopen(&self, key: &K) -> Result<Option<MemSink>, MemError> {
        self.lock()?
            .get(key)
            .filter(|t| !t.is_finished())
            .map(MemTape::hold)
            .transpose()
    }

    /// The finished tape under `key` ([Tapes::finished]).
    pub fn finished(&self, key: &K) -> Result<Option<MemTape>, MemError> {
        Ok(self.lock()?.get(key).filter(|t| t.is_finished()).cloned())
    }

    /// A new tape of several pieces under `key` ([Tapes::create_whole]).
    pub fn create_whole(&self, key: &K) -> Result<MemSink, MemError> {
        let mut sink = self.create(key)?;
        sink.whole = true;
        Ok(sink)
    }

    /// The tape of several pieces under `key`, which is written again: it reads as unfinished until its next finish
    /// ([Tapes::open_whole]).
    pub fn open_whole(&self, key: &K) -> Result<Option<MemSink>, MemError> {
        let map = self.lock()?;
        let Some(tape) = map.get(key) else {
            return Ok(None);
        };
        let mut sink = tape.hold()?;
        sink.whole = true;
        tape.0.finished.store(false, Ordering::Release);
        Ok(Some(sink))
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, BTreeMap<K, MemTape>>, MemError> {
        self.0.lock().map_err(|_poisoned| MemError::Poisoned)
    }
}

impl Tapes for MemTapes<u64> {
    type Key = u64;
    type Error = MemError;
    type Sink = MemSink;
    type Source = MemTape;

    fn key_of(&self, seq: u64) -> u64 {
        seq
    }

    fn create(&mut self, key: &u64) -> Result<MemSink, MemError> {
        MemTapes::create(self, key)
    }

    fn reopen(&mut self, key: &u64) -> Result<Option<MemSink>, MemError> {
        MemTapes::reopen(self, key)
    }

    fn finished(&self, key: &u64) -> Result<Option<MemTape>, MemError> {
        MemTapes::finished(self, key)
    }

    fn create_whole(&mut self, key: &u64) -> Result<MemSink, MemError> {
        MemTapes::create_whole(self, key)
    }

    fn open_whole(&mut self, key: &u64) -> Result<Option<MemSink>, MemError> {
        MemTapes::open_whole(self, key)
    }
}
