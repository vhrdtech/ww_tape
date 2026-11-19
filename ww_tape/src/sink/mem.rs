//! Pieces in memory: [MemSink], [MemTape] and [MemTapes].

use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use super::{BaseRefused, TapeSink, Tapes, check_base};
use crate::reader::{ByteSource, OutOfRange, wide};

/// Why a [MemSink], [MemTape] or [MemTapes] failed or refused; a refusal of [MemTapes] names the tape by its key
/// `K`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum MemError<K = u64> {
    /// A new tape under a key that has one.
    Exists(K),
    /// Another sink writes the tape: one writer per tape ([crate::sink]).
    Locked(K),
    /// [MemTapes::remove] of a tape still being written.
    Unclosed(K),
    /// A writer panicked while it held the bytes: they may be half written.
    Poisoned,
    /// A read past the end of the bytes.
    OutOfRange,
    /// A piece start refused: past the end of the bytes, or any but 0 on a sink of one piece.
    Base(BaseRefused),
}

impl<K: core::fmt::Debug> core::fmt::Display for MemError<K> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            MemError::Exists(key) => write!(f, "the memory tape {key:?} exists"),
            MemError::Locked(key) => write!(f, "another writer holds the memory tape {key:?}"),
            MemError::Unclosed(key) => {
                write!(f, "the memory tape {key:?} is still being written")
            }
            MemError::Poisoned => f.write_str("a writer of the memory tape panicked"),
            MemError::OutOfRange => f.write_str("a read past the end of the memory tape"),
            MemError::Base(refused) => write!(f, "a piece can't start there: {refused}"),
        }
    }
}

impl<K: core::fmt::Debug> core::error::Error for MemError<K> {}

impl<K> From<OutOfRange> for MemError<K> {
    fn from(_: OutOfRange) -> Self {
        MemError::OutOfRange
    }
}

impl<K> From<BaseRefused> for MemError<K> {
    fn from(refused: BaseRefused) -> Self {
        MemError::Base(refused)
    }
}

/// Bytes in memory that a [MemSink] writes and readers read at the same time: a [ByteSource] whose size grows. `K`
/// is the key type of the [MemTapes] it is kept in, named by its errors.
pub struct MemTape<K = u64>(Arc<MemBytes>, PhantomData<fn() -> K>);

impl<K> Clone for MemTape<K> {
    fn clone(&self) -> Self {
        MemTape(Arc::clone(&self.0), PhantomData)
    }
}

impl<K> Default for MemTape<K> {
    fn default() -> Self {
        MemTape(Arc::default(), PhantomData)
    }
}

impl<K> core::fmt::Debug for MemTape<K> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("MemTape").field(&self.0).finish()
    }
}

#[derive(Debug, Default)]
struct MemBytes {
    bytes: RwLock<Vec<u8>>,
    closed: AtomicBool,
    /// A [MemSink] writes the bytes
    held: AtomicBool,
}

impl<K> MemTape<K> {
    /// A copy of the bytes.
    pub fn to_vec(&self) -> Result<Vec<u8>, MemError<K>> {
        Ok(self.read()?.clone())
    }

    /// Whether the sink writing these bytes closed.
    pub fn is_closed(&self) -> bool {
        self.0.closed.load(Ordering::Acquire)
    }

    /// A sink writing these bytes, the only one while it lives: None while another one does.
    fn hold(&self) -> Option<MemSink<K>> {
        self.0
            .held
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        Some(MemSink {
            tape: self.clone(),
            base: 0,
            whole: false,
        })
    }

    fn read(&self) -> Result<std::sync::RwLockReadGuard<'_, Vec<u8>>, MemError<K>> {
        self.0.bytes.read().map_err(|_poisoned| MemError::Poisoned)
    }

    fn write(&self) -> Result<std::sync::RwLockWriteGuard<'_, Vec<u8>>, MemError<K>> {
        self.0.bytes.write().map_err(|_poisoned| MemError::Poisoned)
    }
}

impl<K: core::fmt::Debug + 'static> ByteSource for MemTape<K> {
    type Error = MemError<K>;

    fn size(&self) -> Result<u64, MemError<K>> {
        Ok(wide(self.read()?.len()))
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), MemError<K>> {
        Ok(self.read()?.as_slice().read_at(offset, buf)?)
    }
}

/// A piece in memory, readers reading its [MemTape] as it grows; `K` as for [MemTape].
#[derive(Debug)]
pub struct MemSink<K = u64> {
    tape: MemTape<K>,
    /// Where the piece being written starts in the bytes
    base: u64,
    /// The tape holds pieces back to back ([Tapes::create_whole], [Tapes::open_whole])
    whole: bool,
}

impl MemSink {
    /// An empty sink, not kept in any [MemTapes].
    pub fn new() -> Self {
        Self::empty()
    }
}

impl<K> MemSink<K> {
    /// An empty sink over new bytes, held.
    fn empty() -> Self {
        let tape = MemTape::default();
        tape.0.held.store(true, Ordering::Release);
        MemSink {
            tape,
            base: 0,
            whole: false,
        }
    }

    /// The bytes, for readers.
    pub fn tape(&self) -> MemTape<K> {
        self.tape.clone()
    }
}

impl<K> Drop for MemSink<K> {
    /// The bytes are free for the next writer.
    fn drop(&mut self) {
        self.tape.0.held.store(false, Ordering::Release);
    }
}

impl<K: core::fmt::Debug + 'static> TapeSink for MemSink<K> {
    type Error = MemError<K>;

    fn write_all(&mut self, bytes: &[u8]) -> Result<(), MemError<K>> {
        self.tape.write()?.extend_from_slice(bytes);
        Ok(())
    }

    fn sync(&mut self) -> Result<(), MemError<K>> {
        Ok(())
    }

    fn close(&mut self) -> Result<(), MemError<K>> {
        self.tape.0.closed.store(true, Ordering::Release);
        Ok(())
    }

    fn size(&self) -> Result<u64, MemError<K>> {
        Ok(ByteSource::size(&self.tape)?.saturating_sub(self.base))
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), MemError<K>> {
        let at = self.base.checked_add(offset).ok_or(MemError::OutOfRange)?;
        ByteSource::read_at(&self.tape, at, buf)
    }

    fn base(&self) -> u64 {
        self.base
    }

    fn holds_pieces(&self) -> bool {
        self.whole
    }

    fn rebase(&mut self, base: u64) -> Result<(), MemError<K>> {
        self.base = check_base(base, ByteSource::size(&self.tape)?, self.whole)?;
        Ok(())
    }

    fn reserve(&mut self, bytes: u64) {
        if let (Ok(more), Ok(mut vec)) = (usize::try_from(bytes), self.tape.write()) {
            // one allocation up front: growing by doubling copies the piece under the write lock, and `read_at` waits
            _ = vec.try_reserve_exact(more);
        }
    }

    fn truncate(&mut self, len: u64) -> Result<(), MemError<K>> {
        let len = self.base.checked_add(len).ok_or(MemError::OutOfRange)?;
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
pub struct MemTapes<K = u64>(Arc<Mutex<BTreeMap<K, MemTape<K>>>>);

impl<K> Default for MemTapes<K> {
    fn default() -> Self {
        MemTapes(Arc::default())
    }
}

impl<K: Ord + Clone> MemTapes<K> {
    pub fn new() -> Self {
        Self::default()
    }

    /// The bytes under `key`, closed or not.
    pub fn get(&self, key: &K) -> Result<Option<MemTape<K>>, MemError<K>> {
        Ok(self.lock()?.get(key).cloned())
    }

    /// Take the closed piece under `key` out of the map: true when it was there, false when it was not.
    ///
    /// A [MemTape] a reader already holds keeps its bytes (they are freed with the last holder), and reads on as
    /// before: nothing a reader holds ever changes or fails. After the call [Self::get] and [Self::closed] find
    /// nothing under `key`. A piece still being written is not removed: [MemError::Unclosed], so a writer never
    /// writes into bytes nobody can find. Removing the oldest pieces of a chain leaves a head cut
    /// ([ChainReport::head_cut](crate::piece::ChainReport::head_cut)); a [PieceChain](crate::PieceChain) resumes only
    /// from its first piece, so a chain with removed pieces is not resumed.
    pub fn remove(&self, key: &K) -> Result<bool, MemError<K>> {
        let mut map = self.lock()?;
        match map.get(key) {
            None => Ok(false),
            Some(t) if !t.is_closed() => Err(MemError::Unclosed(key.clone())),
            Some(_) => Ok(map.remove(key).is_some()),
        }
    }

    /// Every key with bytes, in order.
    pub fn keys(&self) -> Result<Vec<K>, MemError<K>> {
        Ok(self.lock()?.keys().cloned().collect())
    }

    /// A new sink under `key` ([Tapes::create]): [MemError::Exists] when the key has a tape.
    pub fn create(&self, key: &K) -> Result<MemSink<K>, MemError<K>> {
        let mut map = self.lock()?;
        if map.contains_key(key) {
            return Err(MemError::Exists(key.clone()));
        }
        let sink = MemSink::empty();
        map.insert(key.clone(), sink.tape());
        Ok(sink)
    }

    /// The unclosed tape under `key`, to resume ([Tapes::reopen]).
    pub fn reopen(&self, key: &K) -> Result<Option<MemSink<K>>, MemError<K>> {
        self.lock()?
            .get(key)
            .filter(|t| !t.is_closed())
            .map(|t| t.hold().ok_or_else(|| MemError::Locked(key.clone())))
            .transpose()
    }

    /// The closed tape under `key` ([Tapes::closed]).
    pub fn closed(&self, key: &K) -> Result<Option<MemTape<K>>, MemError<K>> {
        Ok(self.lock()?.get(key).filter(|t| t.is_closed()).cloned())
    }

    /// A new tape of several pieces under `key` ([Tapes::create_whole]).
    pub fn create_whole(&self, key: &K) -> Result<MemSink<K>, MemError<K>> {
        let mut sink = self.create(key)?;
        sink.whole = true;
        Ok(sink)
    }

    /// The tape of several pieces under `key`, which is written again: it reads as unclosed until its next close
    /// ([Tapes::open_whole]).
    pub fn open_whole(&self, key: &K) -> Result<Option<MemSink<K>>, MemError<K>> {
        let map = self.lock()?;
        let Some(tape) = map.get(key) else {
            return Ok(None);
        };
        let mut sink = tape.hold().ok_or_else(|| MemError::Locked(key.clone()))?;
        sink.whole = true;
        tape.0.closed.store(false, Ordering::Release);
        Ok(Some(sink))
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, BTreeMap<K, MemTape<K>>>, MemError<K>> {
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

    fn closed(&self, key: &u64) -> Result<Option<MemTape>, MemError> {
        MemTapes::closed(self, key)
    }

    fn create_whole(&mut self, key: &u64) -> Result<MemSink, MemError> {
        MemTapes::create_whole(self, key)
    }

    fn open_whole(&mut self, key: &u64) -> Result<Option<MemSink>, MemError> {
        MemTapes::open_whole(self, key)
    }
}
