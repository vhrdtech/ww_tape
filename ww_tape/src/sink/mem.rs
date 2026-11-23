//! Pieces in memory: [MemSink], [MemTape] and [MemTapes].

use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use super::segments::{SegmentError, Segments};
use super::{BaseRefused, TapeSink, Tapes, check_base};
use crate::TapeWriter;
use crate::reader::{ByteSource, OutOfRange, TapeReader};
use crate::writer::{Sealed, WriteError};

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
    /// A read or truncate before `front`, the first byte kept: [MemTapes::drop_front] freed the bytes before it.
    Dropped { front: u64 },
    /// A piece start refused: past the end of the bytes, or any but 0 on a sink of one piece.
    Base(BaseRefused),
}

impl<K: core::fmt::Debug> core::fmt::Display for MemError<K> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Exists(key) => write!(f, "the memory tape {key:?} exists"),
            Self::Locked(key) => write!(f, "another writer holds the memory tape {key:?}"),
            Self::Unclosed(key) => {
                write!(f, "the memory tape {key:?} is still being written")
            }
            Self::Poisoned => f.write_str("a writer of the memory tape panicked"),
            Self::OutOfRange => f.write_str("a read past the end of the memory tape"),
            Self::Dropped { front } => {
                write!(f, "the memory tape keeps its bytes from {front} on only")
            }
            Self::Base(refused) => write!(f, "a piece can't start there: {refused}"),
        }
    }
}

impl<K: core::fmt::Debug> core::error::Error for MemError<K> {}

impl<K> From<OutOfRange> for MemError<K> {
    fn from(_: OutOfRange) -> Self {
        Self::OutOfRange
    }
}

impl<K> From<SegmentError> for MemError<K> {
    fn from(e: SegmentError) -> Self {
        match e {
            SegmentError::OutOfRange => Self::OutOfRange,
            SegmentError::Dropped { front } => Self::Dropped { front },
        }
    }
}

impl<K> From<BaseRefused> for MemError<K> {
    fn from(refused: BaseRefused) -> Self {
        Self::Base(refused)
    }
}

/// Bytes in memory that a [MemSink] writes and readers read at the same time: a [ByteSource] whose size grows. `K`
/// is the key type of the [MemTapes] it is kept in, named by its errors.
///
/// The bytes are kept in extents that are filled and never moved, so appending copies nothing written before and a
/// reader waits only for the bytes being appended. [MemTapes::drop_front] frees the oldest pieces of a tape of several; offsets stay as
/// they were, and the tape's [ByteSource::start] moves to the first byte kept.
pub struct MemTape<K = u64>(Arc<MemBytes>, PhantomData<fn() -> K>);

impl<K> Clone for MemTape<K> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0), PhantomData)
    }
}

impl<K> Default for MemTape<K> {
    fn default() -> Self {
        Self(Arc::default(), PhantomData)
    }
}

impl<K> core::fmt::Debug for MemTape<K> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("MemTape").field(&self.0).finish()
    }
}

#[derive(Debug, Default)]
struct MemBytes {
    bytes: RwLock<Segments>,
    closed: AtomicBool,
    /// A [MemSink] writes the bytes
    held: AtomicBool,
}

impl<K> MemTape<K> {
    /// A copy of the bytes kept, from [ByteSource::start] to the end.
    pub fn to_vec(&self) -> Result<Vec<u8>, MemError<K>> {
        let segments = self.read()?;
        #[expect(
            clippy::map_err_ignore,
            reason = "a TryFromIntError says only that the number is out of range, which this error says"
        )]
        let len = usize::try_from(segments.held()).map_err(|_| MemError::OutOfRange)?;
        let mut out = vec![0; len];
        segments.read_at(segments.front(), &mut out)?;
        Ok(out)
    }

    /// Bytes held in memory: from [ByteSource::start] to the end.
    pub fn held(&self) -> Result<u64, MemError<K>> {
        Ok(self.read()?.held())
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

    fn read(&self) -> Result<std::sync::RwLockReadGuard<'_, Segments>, MemError<K>> {
        self.0.bytes.read().map_err(|_poisoned| MemError::Poisoned)
    }

    fn write(&self) -> Result<std::sync::RwLockWriteGuard<'_, Segments>, MemError<K>> {
        self.0.bytes.write().map_err(|_poisoned| MemError::Poisoned)
    }
}

impl<K: core::fmt::Debug + 'static> ByteSource for MemTape<K> {
    type Error = MemError<K>;

    fn size(&self) -> Result<u64, MemError<K>> {
        Ok(self.read()?.len())
    }

    fn start(&self) -> Result<u64, MemError<K>> {
        Ok(self.read()?.front())
    }

    /// A memory tape is written by a [MemSink] of this process and never leaves its memory.
    fn trusted(&self) -> bool {
        true
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), MemError<K>> {
        Ok(self.read()?.read_at(offset, buf)?)
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
        Self::empty_at(0)
    }

    /// An empty sink over new bytes whose first byte comes at offset `front`, held.
    fn empty_at(front: u64) -> Self {
        let tape = MemTape(
            Arc::new(MemBytes {
                bytes: RwLock::new(Segments::starting_at(front)),
                closed: AtomicBool::new(false),
                held: AtomicBool::new(true),
            }),
            PhantomData,
        );
        Self {
            tape,
            base: front,
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
        self.tape.write()?.append(bytes);
        Ok(())
    }

    fn sync(&mut self) -> Result<(), MemError<K>> {
        Ok(())
    }

    fn close(&mut self) -> Result<(), MemError<K>> {
        self.tape.write()?.seal();
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
        let mut segments = self.tape.write()?;
        let base = check_base(base, segments.len(), self.whole)?;
        segments.rebase(base)?;
        self.base = base;
        Ok(())
    }

    fn reserve(&mut self, bytes: u64) {
        if let (Ok(more), Ok(mut segments)) = (usize::try_from(bytes), self.tape.write()) {
            // one extent for what is to come, instead of extents that grow as the piece does
            segments.reserve(more);
        }
    }

    fn truncate(&mut self, len: u64) -> Result<(), MemError<K>> {
        let len = self.base.checked_add(len).ok_or(MemError::OutOfRange)?;
        Ok(self.tape.write()?.truncate(len)?)
    }
}

impl Default for MemSink {
    fn default() -> Self {
        Self::new()
    }
}

/// Pieces in memory by key, shared by every clone: a store of [Tapes] (keyed by a piece's seq with `K = u64`), and
/// where its readers find each piece ([Self::get]).
///
/// Another key lets an application keep the pieces of many chains in one map, behind a [Tapes] of its own that names
/// each chain's keys.
#[derive(Clone, Debug)]
pub struct MemTapes<K = u64>(Arc<Mutex<BTreeMap<K, MemTape<K>>>>);

impl<K> Default for MemTapes<K> {
    fn default() -> Self {
        Self(Arc::default())
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

    /// Free the oldest pieces of the tape under `key` that end at or before `to`, an offset in the tape: the first
    /// byte it keeps ([ByteSource::start]), None when there is no tape under `key`.
    ///
    /// Meant for a tape of several pieces ([Tapes::create_whole]) whose oldest pieces are no longer needed, while a
    /// writer goes on in it or after it closed. Pieces are dropped whole, and the piece being written never: a `to`
    /// inside a piece keeps that piece. The memory is freed in extents of up to 64 MiB: the one a cut falls inside
    /// goes with a later cut past it. Offsets don't move: a [PieceInfo](crate::PieceInfo) a reader holds stays
    /// valid for the pieces kept, a read of a dropped one fails with [MemError::Dropped], and a
    /// [TapeReader](crate::TapeReader) opened afterwards finds the pieces from the new start on. As for
    /// [Self::remove], the chain then has a head cut and is not resumed.
    pub fn drop_front(&self, key: &K, to: u64) -> Result<Option<u64>, MemError<K>> {
        let Some(tape) = self.get(key)? else {
            return Ok(None);
        };
        Ok(Some(tape.write()?.drop_front(to)))
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

    /// The tape under `key` to write on ([Tapes::reopen]): unclosed, to resume, or closed, to write on after its
    /// close; it keeps its mark of closed.
    pub fn reopen(&self, key: &K) -> Result<Option<MemSink<K>>, MemError<K>> {
        self.lock()?
            .get(key)
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

    /// Raise the closed tape under `key` to [Integrity::Full](crate::Integrity::Full) ([TapeWriter::seal_on]): its
    /// pieces sealed in order, the first after the piece `after` when it follows a piece sealed before; None when
    /// there is no tape under `key`.
    ///
    /// The sealed copy takes the tape's place, at the same offsets: a [MemTape] a reader already holds keeps the old
    /// bytes and reads on as before, and readers opened afterwards read the sealed ones. A tape still being written
    /// is not sealed ([MemError::Unclosed]), nor one a writer took while it was sealed ([MemError::Locked]). Gives
    /// each piece sealed: the last is what the tape after it takes as `after`. A tape whose pieces are all of Full,
    /// given no `after`, stays as it is and gives none.
    pub fn seal(
        &self,
        key: &K,
        after: Option<&Sealed>,
    ) -> Result<Option<Vec<Sealed>>, WriteError<MemError<K>>>
    where
        K: core::fmt::Debug + 'static,
    {
        let refused = |error| WriteError::Sink {
            what: "sealing a memory tape",
            error,
        };
        let Some(tape) = self.get(key).map_err(refused)? else {
            return Ok(None);
        };
        if !tape.is_closed() {
            return Err(refused(MemError::Unclosed(key.clone())));
        }
        let reader = TapeReader::open(tape.clone()).map_err(WriteError::Read)?;
        if after.is_none()
            && reader
                .pieces()
                .iter()
                .all(|p| p.integrity() == crate::Integrity::Full)
        {
            return Ok(Some(Vec::new()));
        }
        let pieces: Vec<(u64, u64)> = reader
            .pieces()
            .iter()
            .map(|p| (p.start, p.end.saturating_sub(p.start)))
            .collect();
        let front = tape.start().map_err(refused)?;
        let mut out = MemSink::empty_at(front);
        out.whole = pieces.len() > 1 || front > 0;
        // one extent for the copy, as long as the tape
        out.reserve(tape.held().map_err(refused)?);
        let mut sealed: Vec<Sealed> = Vec::with_capacity(pieces.len());
        for piece in pieces {
            out.rebase(piece.0).map_err(refused)?;
            let (s, sink) = TapeWriter::seal_on(&tape, piece, out, sealed.last().or(after))?;
            out = sink;
            sealed.push(s);
        }
        out.close().map_err(refused)?;
        let mut map = self.lock().map_err(refused)?;
        match map.get_mut(key) {
            Some(t) if Arc::ptr_eq(&t.0, &tape.0) && !t.0.held.load(Ordering::Acquire) => {
                *t = out.tape();
                Ok(Some(sealed))
            }
            _ => Err(refused(MemError::Locked(key.clone()))),
        }
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
        Self::create(self, key)
    }

    fn reopen(&mut self, key: &u64) -> Result<Option<MemSink>, MemError> {
        Self::reopen(self, key)
    }

    fn closed(&self, key: &u64) -> Result<Option<MemTape>, MemError> {
        Self::closed(self, key)
    }

    fn create_whole(&mut self, key: &u64) -> Result<MemSink, MemError> {
        Self::create_whole(self, key)
    }

    fn open_whole(&mut self, key: &u64) -> Result<Option<MemSink>, MemError> {
        Self::open_whole(self, key)
    }
}
