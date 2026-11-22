//! [TapeReader]: a tape read by positional reads, without a scan where its index allows.
//!
//! A [ByteSource] is anything with a length that reads bytes at an offset: a byte slice, a file, an HTTP resource read
//! by range requests in a browser. The reader needs no file system and no threads, so it builds for
//! `wasm32-unknown-unknown`.
//!
//! [TapeReader::open] finds the pieces from the end: a good [tail](crate::Tail) gives the piece's start, close and
//! final index, and the previous piece's tail sits just before that start, so concatenated pieces cost one small read
//! each. A piece without a tail (being written, or cut off by a crash) is found from its newest checkpoint
//! [index](crate::index) block, looked for backwards from the end, and the stretch after that checkpoint is scanned;
//! a piece without any checkpoint is scanned from the start of what is left. Scans stop at the first block that
//! isn't good or belongs to a session that may not start there, as [Recovery::Prefix](crate::Recovery) does, and say where
//! ([PieceInfo::cut]); a stretch the session's data goes on after within the layout's gap is read past
//! ([PieceInfo::gaps]). A raw medium ([Medium::Raw]) is walked forward from its start or a hint instead
//! ([OpenOptions::medium]).
//!
//! Index entries of a closed piece are read when asked for: all of them ([TapeReader::entries]) or one track's
//! ([TapeReader::track_entries]: the final index's head and that track's entries, each checked by its own CRC).

use crate::{ItemNum, TrackId};
use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;
use core::ops::Range;

use exact_units::units::Entry;
use exact_units::{Bytes, Count, Ratio64};

use crate::bytes::le_u8;
use crate::chunk::{CHUNK_HEAD_PREFIX, ChunkHead};
use crate::index::{
    ENTRY_LEN, HEAD_LEN_PREFIX, INDEX_LEN_MAX, IndexBlock, IndexEntry, IndexError, IndexKind,
};
use crate::piece::{
    APPEND_HEAD_LEN, AppendHead, ChainReport, Close, CloseBlock, ContentHash, HeadLink, HeadStatus,
    OwnerId, PieceHead, PieceHeadBlock, PieceSummary, RemovedPiece, TAIL_LEN, Tail, TrackHash,
    TrackRange, TrackStart, UnitHash, Walked, check_chain, payload_hash, walk_piece,
};
use crate::series::{SeriesError, SeriesTimeline};
use crate::walk::{Head, SessionState, Verdict, is_free};
use crate::{
    Block, BlockError, BlockKind, GAP_CHECKS_MAX, HEADER_LEN, Layout, MAGIC, MAX_BLOCK_LEN, Medium,
    TrackNum, VERSION, block_len,
};

mod state;
mod verify;
pub use state::{DefinitionsAt, Lookup, Lost, STATE_CACHE_MAX, STATE_READ_MAX, StateItem};
pub use verify::VERIFY_LISTS_MAX;

/// Bytes that read at an offset.
pub trait ByteSource {
    type Error: core::fmt::Debug;

    /// Length of the bytes. (Not `len`, so it never shadows a slice's own.)
    fn size(&self) -> Result<u64, Self::Error>;

    /// Fill `buf` from `offset`; the range is within [Self::start] and [Self::size] (the reader checks).
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), Self::Error>;

    /// Offset of the first byte there is: 0, or where bytes before it were freed (a memory tape whose oldest pieces
    /// were dropped). A reader finds no piece before it.
    fn start(&self) -> Result<u64, Self::Error> {
        Ok(0)
    }
}

/// A read past the end of a byte slice.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct OutOfRange;

impl ByteSource for [u8] {
    type Error = OutOfRange;

    fn size(&self) -> Result<u64, OutOfRange> {
        Ok(wide(self.len()))
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), OutOfRange> {
        #[expect(
            clippy::map_err_ignore,
            reason = "a TryFromIntError says only that the number is out of range, which this error says"
        )]
        let from = usize::try_from(offset).map_err(|_| OutOfRange)?;
        let src = from
            .checked_add(buf.len())
            .and_then(|to| self.get(from..to))
            .ok_or(OutOfRange)?;
        buf.copy_from_slice(src);
        Ok(())
    }
}

impl<S: ByteSource + ?Sized> ByteSource for &S {
    type Error = S::Error;

    fn size(&self) -> Result<u64, S::Error> {
        (**self).size()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), S::Error> {
        (**self).read_at(offset, buf)
    }

    fn start(&self) -> Result<u64, S::Error> {
        (**self).start()
    }
}

impl ByteSource for Vec<u8> {
    type Error = OutOfRange;

    fn size(&self) -> Result<u64, OutOfRange> {
        Ok(wide(self.len()))
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), OutOfRange> {
        self.as_slice().read_at(offset, buf)
    }
}

/// Why a tape can't be read.
#[derive(Debug)]
#[non_exhaustive]
pub enum ReadError<E> {
    /// The byte source failed.
    Source(E),
    /// An index, close, tail or piece head block is bad where a good one must be.
    Index(IndexError),
    /// The block at `offset` isn't good.
    Block { offset: u64, error: BlockError },
    /// The piece has no close record, so no content hash.
    Unclosed,
    /// No piece has this number: the reader has fewer.
    NoPiece(usize),
    /// The piece at `offset` is of another format version, `found` ([PieceInfo::version]).
    Version { offset: u64, found: u8 },
    /// The series chunk at `offset` doesn't fit its own grid or the chunks next to it.
    Series { offset: u64, error: SeriesError },
    /// The chunk at `offset` starts before `end`, the end of the track's chunk before it: the track's chunks overlap.
    Overlap { offset: u64, end: u64 },
    /// The events chunk at `offset` of track `track_id` starts a monotone relation below the track's chunk before it,
    /// and isn't marked a break ([FLAG_RELATION_BREAK](crate::FLAG_RELATION_BREAK)): `relation` by its index in the
    /// track's definition, None for the range relation read from the index entries, which don't name it.
    RelationOrder {
        offset: u64,
        track_id: TrackId,
        relation: Option<u16>,
    },
}

impl<E: core::fmt::Debug> core::fmt::Display for ReadError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Source(e) => write!(f, "reading the bytes: {e:?}"),
            Self::Index(e) => write!(f, "index or piece block: {e}"),
            Self::Block { offset, error } => write!(f, "block at byte {offset}: {error}"),
            Self::Unclosed => write!(f, "the piece has no close record"),
            Self::NoPiece(i) => write!(f, "no piece {i}"),
            Self::Version { offset, found } => write!(
                f,
                "a piece of format version {found} (at byte {offset}); this reader reads version {}",
                crate::VERSION
            ),
            Self::Series { offset, error } => {
                write!(f, "series chunk at byte {offset}: {error}")
            }
            Self::Overlap { offset, end } => {
                write!(
                    f,
                    "chunk at byte {offset} starts before item {end}, the end of the chunk before"
                )
            }
            Self::RelationOrder {
                offset,
                track_id,
                relation,
            } => write!(
                f,
                "chunk at byte {offset} of track {track_id}: monotone relation {relation:?} goes below the chunk \
                 before, with no break marked"
            ),
        }
    }
}

impl<E: core::fmt::Debug> core::error::Error for ReadError<E> {}

/// Most bytes of blocks of definitions a reader reads looking for the layout of a piece's stretch after its newest
/// checkpoint: the layout's newest record behind that stretch.
pub const LAYOUT_READ_MAX: Bytes<u64> = Bytes::new(256 << 20);

/// How [TapeReader::open] looks for pieces.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct OpenOptions {
    /// Most bytes looked through backwards from the end of an unclosed piece for its newest checkpoint before
    /// scanning what is left from its start instead.
    pub scan_back_max: Bytes<u64>,
    /// What the bytes are kept on. A file is read from its end (tails, checkpoints); a [Medium::Raw] is walked
    /// forward from [Self::start], since its end holds stale blocks or nothing and a stale tail there would pass
    /// for the newest piece.
    pub medium: Medium,
    /// Where a [Medium::Raw] walk starts: the medium's start, or a hint the medium layer gives (its own superblock).
    pub start: u64,
    /// The gap rules of a [Medium::Raw] from its start, before the definitions say them; None: none until then.
    pub layout: Option<Layout>,
    /// Most bytes one state lookup spends, coded, decoded and held ([state::STATE_READ_MAX] by default).
    pub state_read_max: Bytes<u64>,
    /// Most state entries the reader keeps sorted over all its pieces ([state::STATE_CACHE_MAX] by default).
    pub state_cache_max: Count<Entry, u64>,
}

/// Default of [OpenOptions::scan_back_max]: 256 MiB, room for a few checkpoint intervals of the largest chunks.
pub const DEFAULT_SCAN_BACK_MAX: Bytes<u64> = Bytes::new(256 << 20);

impl Default for OpenOptions {
    fn default() -> Self {
        Self {
            scan_back_max: DEFAULT_SCAN_BACK_MAX,
            medium: Medium::File,
            start: 0,
            layout: None,
            state_read_max: state::STATE_READ_MAX,
            state_cache_max: state::STATE_CACHE_MAX,
        }
    }
}

/// How a piece was found.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Found {
    /// By its tail.
    Tail,
    /// By its newest checkpoint (or final) index block, `scanned` bytes after it read block by block.
    Checkpoint { scanned: u64 },
    /// Without an index: `scanned` bytes read block by block.
    Scan { scanned: u64 },
}

/// What [TapeReader::verify] checked of a piece, and what it left unchecked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verification {
    /// Whether every check held: each stretch against the hash its close states, the index against the blocks, the
    /// track rows, the append heads.
    pub holds: bool,
    /// The close record in force, where the check ends: its offset in the piece and the hash it states.
    pub close: (u64, ContentHash),
    /// The stretches checked, one per close record up to the one in force.
    pub stretches: usize,
    /// Where the bytes checked end in the byte source: the end of the tail of the close in force (its close record's
    /// start, when no tail ends it).
    pub checked_end: u64,
    /// The bytes after them that no close covers, their place in the source ([TapeReader::unclosed]); None when the
    /// close in force ends the piece's stretch.
    pub unchecked: Option<core::ops::Range<u64>>,
}

/// A piece of the tape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PieceInfo {
    /// Where the piece starts in the byte source.
    pub start: u64,
    /// Where its good bytes end.
    pub end: u64,
    pub head: Option<(PieceHead, Vec<TrackStart>)>,
    /// Why a good piece head block at the piece's start didn't read as its head: damage, or an integrity level this
    /// build doesn't handle ([IndexError::UnhandledIntegrity], [IndexError::UnknownIntegrity]), for which every
    /// lookup in the piece returns it ([ReadError::Index]).
    pub head_error: Option<IndexError>,
    pub close: Option<(Close, Vec<TrackRange>)>,
    /// The pieces of the chain the close names as removed on purpose, ascending by sequence number; empty without a
    /// close.
    pub removed: Vec<RemovedPiece>,
    /// Why a good close record of the piece didn't read as its close: damage the block CRC missed, or rows this build
    /// doesn't read. The piece then has no end: its tail, if read, ends no chain ([Self::summary]).
    pub close_error: Option<IndexError>,
    /// The close record's offset in the piece.
    pub close_offset: Option<u64>,
    pub tail: Option<Tail>,
    pub found: Found,
    /// Where the bytes stopped being good blocks, when a scan stopped before the end of the piece's stretch: a cut
    /// last block, damage, something that isn't a block, or a block of another session where no session may start
    /// (stale data behind the end of data).
    pub cut: Option<u64>,
    /// Blank or bad stretches a scan read past, the session's data going on after them within the layout's
    /// [gap](Layout::gap_max): offset and length of each.
    pub gaps: Vec<(u64, u64)>,
    /// Blocks a scan took in place of the block before them: the same session and seq with other bytes (the later
    /// copy wins); their offsets. A copy with the same bytes is dropped silently.
    pub replaced: Vec<u64>,
    /// The format version of a piece of another one: its first block met is good and of that version, and the
    /// stretch from it to the end of the scan is read no further ([ReadError::Version] from every lookup).
    pub version: Option<u8>,
    /// Entries a scan or the checkpoint chain gave, in file order; None for a piece read by its tail
    entries: Option<Vec<IndexEntry>>,
    /// The walk where a scan was cut: the sessions and seqs a block past the cut must go on with
    cut_walk: Option<SessionState>,
    /// The links of a head of an integrity level this build refuses
    refused_link: Option<HeadLink>,
    /// The last close a scan read with its tail, while a later close it met has none yet: in force unless that one's
    /// tail follows
    in_force: Option<InForce>,
    /// Where the close record a scan met last ends, in the piece: its tail starts there
    close_end: Option<u64>,
}

/// A close record of a piece with its rows, offset and tail.
#[derive(Clone, Debug, PartialEq, Eq)]
struct InForce {
    close: (Close, Vec<TrackRange>),
    removed: Vec<RemovedPiece>,
    offset: Option<u64>,
    tail: Tail,
}

impl PieceInfo {
    fn new(start: u64, found: Found) -> Self {
        Self {
            start,
            end: start,
            head: None,
            head_error: None,
            close: None,
            removed: Vec::new(),
            close_error: None,
            close_offset: None,
            tail: None,
            found,
            cut: None,
            gaps: Vec::new(),
            replaced: Vec::new(),
            version: None,
            entries: Some(Vec::new()),
            cut_walk: None,
            refused_link: None,
            in_force: None,
            close_end: None,
        }
    }

    /// Take the good close record `block` at `offset` in the piece, met by a scan: the piece's close from here on.
    /// The close before it, read with its tail, stays in force until this one's tail is read.
    fn meet_close(&mut self, block: &Block<'_>, offset: Option<u64>) {
        self.close_end = offset.and_then(|at| at.checked_add(wide(block.len())));
        if let (Some(close), Some(tail), None) =
            (self.close.take(), self.tail.take(), &self.close_error)
        {
            self.in_force = Some(InForce {
                close,
                removed: core::mem::take(&mut self.removed),
                offset: self.close_offset,
                tail,
            });
        }
        match CloseBlock::read(block) {
            Ok(f) => {
                self.close = Some((f.close(), f.tracks().collect()));
                self.removed = f.removed().collect();
                self.close_error = None;
                self.close_offset = offset;
            }
            Err(e) => self.close_error = Some(e),
        }
    }

    /// Whether `tail`, a tail block at `tail_at` in the piece, ends the piece's close: it follows the close record
    /// right away and names it ([Tail::ends]).
    fn ends_close(&self, tail: &Tail, tail_at: Option<u64>) -> bool {
        let (Some((close, _)), Some(at)) = (&self.close, self.close_offset) else {
            return false;
        };
        self.close_error.is_none()
            && self.close_end.is_some()
            && self.close_end == tail_at
            && tail.ends(close, at)
    }

    /// End a scan of the piece: a close met without its tail after it (a crash in the end of an append) leaves the
    /// close before it in force.
    fn settle_close(&mut self) {
        let Some(f) = self.in_force.take() else {
            return;
        };
        if self.tail.is_none() || self.close_error.is_some() {
            self.close = Some(f.close);
            self.removed = f.removed;
            self.close_offset = f.offset;
            self.tail = Some(f.tail);
            self.close_error = None;
        }
    }

    /// Take the good piece head block `block` as the piece's head, or keep why it doesn't read as one.
    fn take_head(&mut self, block: &Block<'_>) {
        match PieceHeadBlock::read(block) {
            Ok(h) => {
                self.head = Some((h.head(), h.tracks().collect()));
                self.head_error = None;
                self.refused_link = None;
            }
            Err(e) => {
                self.head = None;
                self.head_error = Some(e);
                self.refused_link = PieceHeadBlock::refused(block).map(|(link, _)| link);
            }
        }
    }

    /// Whether the piece ends at its last close record: a good close and its tail, no good block after the tail. A
    /// piece with a close and blocks after its tail was written on after it, the writer still writing or stopped by
    /// a crash: closed up to [Self::closed_end], its unclosed tail read as an unclosed piece is. Bytes after the tail
    /// that read as no block (an append torn in its append head) leave the piece closed, [Self::cut] at them.
    pub fn is_closed(&self) -> bool {
        self.closed_end() == Some(self.end)
    }

    /// Where the piece's last good close ends in the byte source: the end of its tail; None without one.
    pub fn closed_end(&self) -> Option<u64> {
        let (close, _) = self.close.as_ref().filter(|_| self.close_error.is_none())?;
        let tail = self
            .tail
            .filter(|t| self.close_offset.is_some_and(|at| t.ends(close, at)))?;
        self.start.checked_add(tail.piece_len)
    }

    /// The error the piece is refused with: its head is of an integrity level this build doesn't handle.
    fn refused(&self) -> Option<IndexError> {
        self.refused_link.and(self.head_error)
    }

    /// What [check_chain] needs of the piece: from its head, or from its close when the head is lost; None for one
    /// without either. Its end is the close's, or the tail's when no close record was refused
    /// ([Self::close_error]): a refused close leaves the piece open.
    pub fn summary(&self) -> Option<PieceSummary> {
        let tail = self.tail.filter(|_| self.close_error.is_none());
        let end = self
            .close
            .as_ref()
            .map(|(f, _)| (f.reason, f.hash, f.next))
            .or(tail.map(|t| (t.reason, t.hash, t.next)));
        let unclosed = end.is_some() && !self.is_closed();
        if let Some((head, _)) = &self.head {
            return Some(PieceSummary {
                ident: head.ident,
                prev: head.prev,
                end,
                unclosed,
                head: HeadStatus::Read,
                removed: self.removed.clone(),
            });
        }
        if let Some((link, error)) = self.refused_link.zip(self.head_error) {
            return Some(PieceSummary {
                ident: link.ident,
                prev: link.prev,
                end,
                unclosed,
                head: HeadStatus::Refused(error),
                removed: self.removed.clone(),
            });
        }
        // a head lost to damage: the close of a piece that had one carries its ident
        let ident = self.close.as_ref().and_then(|(f, _)| f.ident)?;
        Some(PieceSummary {
            ident,
            prev: None,
            end,
            unclosed,
            head: HeadStatus::Lost,
            removed: self.removed.clone(),
        })
    }
}

/// `n` as a `u64`.
pub(crate) fn wide(n: usize) -> u64 {
    const { assert!(usize::BITS <= u64::BITS, "a usize fits a u64") };
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// `n` as the `u32` of [BlockError::BadLength]; `u32::MAX` for a longer one, over any block's length either way.
pub(crate) fn report_len(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// A block checked once, its bytes owned: lent out as a [Block] again without a second check.
#[derive(Clone, Debug)]
pub(crate) struct OwnedBlock {
    bytes: Vec<u8>,
    /// The header of the block `bytes` holds whole
    head: Block<'static>,
}

impl OwnedBlock {
    /// Check `bytes` as one whole block.
    pub(crate) fn read(bytes: Vec<u8>) -> Result<Self, BlockError> {
        let b = Block::read(&bytes)?;
        if b.len() != bytes.len() {
            return Err(BlockError::BadLength(report_len(bytes.len())));
        }
        let head = b.detach();
        Ok(Self { bytes, head })
    }

    #[expect(
        clippy::expect_used,
        reason = "OwnedBlock::read checked these bytes as one whole block, at least HEADER_LEN + CRC_LEN long"
    )]
    pub(crate) fn block(&self) -> Block<'_> {
        self.head
            .on(&self.bytes)
            .expect("OwnedBlock::read checked these bytes as one whole block")
    }

    #[cfg(feature = "std")]
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    #[cfg(feature = "std")]
    pub(crate) fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    pub(crate) fn len(&self) -> usize {
        self.bytes.len()
    }
}

/// Longest read a [TapeReader] makes at once: a block's length. A longer length the bytes state is damage.
const READ_LEN_MAX: usize = MAX_BLOCK_LEN;

/// Bytes [TapeReader::newest_index] and the blank checks read at once.
const WINDOW: usize = 64 * 1024;

/// A tape read by positional reads, see the [module docs](self).
pub struct TapeReader<S> {
    src: S,
    len: u64,
    /// [ByteSource::start]: no piece is looked for before it
    front: u64,
    pieces: Vec<PieceInfo>,
    /// [OpenOptions::state_read_max]
    state_read_max: u64,
    /// The pieces' state entries sorted, shared by the reader's lookups
    states: state::StateCache,
}

type R<T, S> = Result<T, ReadError<<S as ByteSource>::Error>>;

/// A close record and its rows, its offset in the piece and its tail, read whole.
type WholeClose = (((Close, Vec<TrackRange>), Vec<RemovedPiece>), u64, Tail);

/// A series timeline and the chunks left out of it, each with its error.
type Timeline<S> = (SeriesTimeline, Vec<ReadError<<S as ByteSource>::Error>>);

impl<S: ByteSource> TapeReader<S> {
    /// Find the pieces of `src` with the default [OpenOptions].
    pub fn open(src: S) -> R<Self, S> {
        Self::open_with(src, OpenOptions::default())
    }

    /// Find the pieces of `src` as `options` say.
    ///
    /// # Panics
    ///
    /// When [OpenOptions::layout] is not [valid](Layout::is_valid).
    pub fn open_with(src: S, options: OpenOptions) -> R<Self, S> {
        let len = src.size().map_err(ReadError::Source)?;
        let front = src.start().map_err(ReadError::Source)?.min(len);
        let mut r = Self {
            src,
            len,
            front,
            pieces: Vec::new(),
            state_read_max: options.state_read_max.get(),
            states: state::StateCache::new(crate::cap_usize(options.state_cache_max.get())),
        };
        if options.medium == Medium::Raw {
            let mut walk = SessionState::new(Medium::Raw);
            if let Some(l) = &options.layout {
                walk.set_layout(l);
            }
            let start = options.start.clamp(front, len);
            r.pieces = r.scan_with(
                start,
                len,
                PieceInfo::new(start, Found::Scan { scanned: 0 }),
                walk,
            )?;
            return Ok(r);
        }
        let mut hi = len;
        let mut rev: Vec<PieceInfo> = Vec::new();
        while hi > front {
            if let Some(p) = r.by_tail(hi)?
                && p.start < hi
            {
                hi = p.start;
                rev.push(p);
                continue;
            }
            let mut found = r.recover(hi, options)?;
            // the recovery reached back into closed pieces (an unclosed piece without a checkpoint of its own
            // found the one before by its final index): those are found by their tails, the rest is unclosed
            if let Some(k) = found.iter().rposition(|p| p.tail.is_some())
                && let Some(closed) = found.get(k)
                && found
                    .get(k.saturating_add(1))
                    .is_some_and(|n| closed.end <= n.start)
                && closed.end < hi
            {
                hi = closed.end;
                rev.extend(found.drain(k.saturating_add(1)..).rev());
                continue;
            }
            let Some(first) = found.first().filter(|f| f.start < hi) else {
                break;
            };
            hi = first.start;
            rev.extend(found.into_iter().rev());
        }
        rev.reverse();
        r.pieces = rev;
        Ok(r)
    }

    /// Length of the byte source.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The pieces, in file order.
    pub fn pieces(&self) -> &[PieceInfo] {
        &self.pieces
    }

    pub fn source(&self) -> &S {
        &self.src
    }

    /// How the pieces with a head link up ([PieceInfo::summary]); its indices are into [Self::pieces].
    pub fn chain(&self) -> ChainReport {
        let (at, s): (Vec<usize>, Vec<PieceSummary>) = self
            .pieces
            .iter()
            .enumerate()
            .filter_map(|(i, p)| p.summary().map(|s| (i, s)))
            .unzip();
        let mut r = check_chain(&s);
        for k in r
            .order
            .iter_mut()
            .chain(&mut r.strays)
            .chain(&mut r.refused)
        {
            // check_chain's indices are into the summaries, one per piece in `at`
            if let Some(i) = at.get(*k) {
                *k = *i;
            }
        }
        r
    }

    /// Piece `i`, one of this reader's version and an integrity level it handles.
    fn piece(&self, i: usize) -> R<&PieceInfo, S> {
        let p = self.pieces.get(i).ok_or(ReadError::NoPiece(i))?;
        if let Some(found) = p.version {
            return Err(ReadError::Version {
                offset: p.start,
                found,
            });
        }
        match p.refused() {
            Some(e) => Err(ReadError::Index(e)),
            None => Ok(p),
        }
    }

    /// `n` bytes at `offset`: at most [READ_LEN_MAX], within the source.
    fn read(&self, offset: u64, n: usize) -> R<Vec<u8>, S> {
        let within = offset
            .checked_add(wide(n))
            .is_some_and(|end| end <= self.len);
        if n > READ_LEN_MAX || !within {
            return Err(ReadError::Block {
                offset,
                error: BlockError::Short { needed: n },
            });
        }
        let mut buf = vec![0u8; n];
        self.src
            .read_at(offset, &mut buf)
            .map_err(ReadError::Source)?;
        Ok(buf)
    }

    /// The whole block at `offset`, if one is there within `hi` and at most `max` long, and where it ends; not
    /// checked yet.
    fn block_bytes(
        &self,
        offset: u64,
        hi: u64,
        max: usize,
    ) -> R<Result<(Vec<u8>, u64), BlockError>, S> {
        if hi.saturating_sub(offset) < wide(HEADER_LEN) {
            return Ok(Err(BlockError::Short { needed: HEADER_LEN }));
        }
        let header = self.read(offset, HEADER_LEN)?;
        let len = match block_len(&header) {
            Ok(l) if l <= max => l,
            Ok(l) => return Ok(Err(BlockError::BadLength(report_len(l)))),
            Err(e) => return Ok(Err(e)),
        };
        let Some(end) = offset.checked_add(wide(len)).filter(|end| *end <= hi) else {
            return Ok(Err(BlockError::Short { needed: len }));
        };
        Ok(Ok((self.read(offset, len)?, end)))
    }

    /// A good block at `offset` within `hi` and where it ends, or why not.
    fn good_block(&self, offset: u64, hi: u64) -> R<Result<(OwnedBlock, u64), BlockError>, S> {
        Ok(match self.block_bytes(offset, hi, MAX_BLOCK_LEN)? {
            Ok((bytes, end)) => OwnedBlock::read(bytes).map(|b| (b, end)),
            Err(e) => Err(e),
        })
    }

    /// The closed piece whose tail ends at `hi`, when its tail and close are good.
    fn by_tail(&self, hi: u64) -> R<Option<PieceInfo>, S> {
        let Some(tail_at) = hi.checked_sub(wide(TAIL_LEN)) else {
            return Ok(None);
        };
        let tb = self.read(tail_at, TAIL_LEN)?;
        let Ok(tail) = Tail::read(&tb) else {
            return Ok(None);
        };
        let Some(start) = hi.checked_sub(tail.piece_len) else {
            return Ok(None);
        };
        let Some(close_at) = start.checked_add(tail.close_offset) else {
            return Ok(None);
        };
        // the close record the tail names ends where the tail starts: nothing lies between them
        let fb = match self.good_block(close_at, hi)? {
            Ok((b, end)) if wide(b.len()) == u64::from(tail.close_len) && end == tail_at => b,
            _ => return Ok(None),
        };
        let Ok(fin) = CloseBlock::read(&fb.block()) else {
            return Ok(None);
        };
        let close = fin.close();
        if !tail.ends(&close, tail.close_offset) {
            return Ok(None);
        }
        // a piece starts with its head, or with blocks of data when its head is lost: never written on after a close
        if let Ok((b, _)) = self.good_block(start, hi)?
            && b.block().kind == BlockKind::AppendHead
        {
            return Ok(None);
        }
        let mut p = PieceInfo::new(start, Found::Tail);
        p.end = hi;
        self.head_at(start, hi, &mut p)?;
        p.close = Some((close, fin.tracks().collect()));
        p.removed = fin.removed().collect();
        p.close_offset = Some(tail.close_offset);
        p.tail = Some(tail);
        p.entries = None;
        Ok(Some(p))
    }

    /// Take the good block at `offset` within `hi`, if it is one, as `p`'s head.
    fn head_at(&self, offset: u64, hi: u64, p: &mut PieceInfo) -> R<(), S> {
        if let Ok((b, _)) = self.good_block(offset, hi)?
            && b.block().kind == BlockKind::PieceHead
        {
            p.take_head(&b.block());
        }
        Ok(())
    }

    /// The pieces of the unclosed stretch ending at `hi`: from the newest index block in reach, else scanned from
    /// the start.
    fn recover(&self, hi: u64, options: OpenOptions) -> R<Vec<PieceInfo>, S> {
        let lo = hi
            .saturating_sub(options.scan_back_max.get())
            .max(self.front);
        let mut below = hi;
        while let Some((at, ob, end)) = self.newest_index(hi, lo, below)? {
            let block = ob.block();
            let Ok(ix) = IndexBlock::read(&block) else {
                below = at;
                continue;
            };
            return match at.checked_sub(ix.self_offset()) {
                Some(start) if start >= self.front => self.by_checkpoint(hi, start, at, end, &ix),
                _ => self.scan_all(hi),
            };
        }
        self.scan_all(hi)
    }

    /// Every piece from [Self::front] to `hi`, found by a scan.
    fn scan_all(&self, hi: u64) -> R<Vec<PieceInfo>, S> {
        self.scan(
            self.front,
            hi,
            PieceInfo::new(self.front, Found::Scan { scanned: 0 }),
        )
    }

    /// The piece at `start` whose newest checkpoint `ix` is at `at` (ending at `end`), the stretch after it to `hi`
    /// scanned.
    fn by_checkpoint(
        &self,
        hi: u64,
        start: u64,
        at: u64,
        end: u64,
        ix: &IndexBlock<'_>,
    ) -> R<Vec<PieceInfo>, S> {
        let mut piece = PieceInfo::new(start, Found::Checkpoint { scanned: 0 });
        self.head_at(start, hi, &mut piece)?;
        let mut lists = vec![ix.entries().collect::<Vec<_>>()];
        let mut prev = ix.prev();
        // offset in the piece and in the source of the oldest good checkpoint
        let (mut newest, mut newest_at) = (ix.self_offset(), at);
        let mut chain_whole = true;
        // the kind of the chain so far: checkpoints written on after a close go back into that close's final index
        let mut kind = ix.kind();
        let mut sorted = kind == IndexKind::Final;
        // the newest final index block followed by a whole close: its offset in the piece, that close and its tail
        let mut final_met = if ix.kind() == IndexKind::Final {
            self.close_after(start, ix.self_offset(), end, hi)?
                .map(|c| (ix.self_offset(), c))
        } else {
            None
        };
        while let Some(off) = prev {
            let older = start.checked_add(off).filter(|_| off < newest);
            let ok = match older {
                Some(off_at) => match self.good_block(off_at, at)? {
                    Ok((b, b_end)) => match IndexBlock::read(&b.block()) {
                        Ok(p)
                            if p.self_offset() == off
                                && (p.kind() == kind
                                    || (kind == IndexKind::Checkpoint
                                        && p.kind() == IndexKind::Final)) =>
                        {
                            if p.kind() == IndexKind::Final && final_met.is_none() {
                                final_met =
                                    self.close_after(start, off, b_end, at)?.map(|c| (off, c));
                            }
                            if p.kind() != kind {
                                kind = IndexKind::Final;
                                sorted = true;
                            }
                            lists.push(p.entries().collect());
                            prev = p.prev();
                            (newest, newest_at) = (off, off_at);
                            true
                        }
                        _ => false,
                    },
                    Err(_) => false,
                },
                None => false,
            };
            if !ok {
                chain_whole = false;
                break;
            }
        }
        if let Some((_, ((close, removed), offset, tail))) = final_met {
            piece.close = Some(close);
            piece.removed = removed;
            piece.close_error = None;
            piece.close_offset = Some(offset);
            piece.tail = Some(tail);
        }
        let mut entries: Vec<IndexEntry> = lists.into_iter().rev().flatten().collect();
        if !chain_whole {
            // the chain broke: what is before the oldest good checkpoint is scanned
            let scanned = self.scan(
                start,
                newest_at,
                PieceInfo::new(start, Found::Scan { scanned: 0 }),
            )?;
            let mut early: Vec<IndexEntry> = scanned
                .into_iter()
                .flat_map(|p| p.entries.unwrap_or_default())
                .collect();
            early.append(&mut entries);
            entries = early;
        }
        if sorted {
            entries.sort_by_key(|e| e.offset);
        }
        // the stretch after the checkpoint is read with the gap rules the piece's definitions state
        let mut walk = SessionState::new(Medium::File);
        if let Some(layout) = self.layout_of(start, &entries, at)? {
            walk.set_layout(&layout);
        }
        piece.entries = Some(entries);
        self.scan_with(end, hi, piece, walk)
    }

    /// The close record right after the final index block at `final_off` in the piece at `start` (ending at
    /// `final_end` in the source, within `hi`) with its tail, when both read whole and the close names that final
    /// index: the close in force, the one checkpoints after it were written on after.
    fn close_after(
        &self,
        start: u64,
        final_off: u64,
        final_end: u64,
        hi: u64,
    ) -> R<Option<WholeClose>, S> {
        let Ok((cb, close_end)) = self.good_block(final_end, hi)? else {
            return Ok(None);
        };
        let Ok(c) = CloseBlock::read(&cb.block()) else {
            return Ok(None);
        };
        let close = c.close();
        if close.final_index != Some(final_off) {
            return Ok(None);
        }
        let Ok((tb, _)) = self.good_block(close_end, hi)? else {
            return Ok(None);
        };
        let Ok(tail) = Tail::read(tb.block().bytes()) else {
            return Ok(None);
        };
        let close_off = since(final_end, start);
        let whole = tail.ends(&close, close_off)
            && u64::from(tail.close_len) == since(close_end, final_end);
        Ok(whole.then(|| {
            (
                ((close, c.tracks().collect()), c.removed().collect()),
                close_off,
                tail,
            )
        }))
    }

    /// The newest [Layout] the blocks of definitions among `entries` (of the piece at `start`, within `hi`) state.
    /// Each block is read once, newest first, within [LAYOUT_READ_MAX] bytes: a hostile index naming one big
    /// block many times costs one read.
    fn layout_of(&self, start: u64, entries: &[IndexEntry], hi: u64) -> R<Option<Layout>, S> {
        let mut offsets: Vec<(u64, u32)> = entries
            .iter()
            .filter(|e| {
                e.state()
                    .is_some_and(|s| s.slot == crate::state::Slot::Layout)
            })
            .map(|e| (e.offset, e.len))
            .collect();
        offsets.sort_unstable();
        offsets.dedup_by_key(|(o, _)| *o);
        let mut budget = LAYOUT_READ_MAX.get();
        for (offset, len) in offsets.into_iter().rev() {
            let Some(left) = budget.checked_sub(u64::from(len)) else {
                break;
            };
            budget = left;
            let Some(at) = start.checked_add(offset) else {
                continue;
            };
            if let Ok((b, _)) = self.good_block(at, hi)?
                && let Some(layout) = crate::walk::layout_in(&b.block())
            {
                return Ok(Some(layout));
            }
        }
        Ok(None)
    }

    /// The newest good index block that starts in `lo..below` and ends within `hi`, where it starts and ends; looked
    /// for backwards [WINDOW] bytes at a time.
    fn newest_index(&self, hi: u64, lo: u64, below: u64) -> R<Option<(u64, OwnedBlock, u64)>, S> {
        // a magic starting just before a window's end reaches into the next window's bytes
        const MAGIC_REST: usize = MAGIC.len() - 1;
        let mut end = below.min(hi);
        while end > lo {
            let from = end.saturating_sub(wide(WINDOW)).max(lo);
            let to = end.saturating_add(wide(MAGIC_REST)).min(hi);
            let win = self.read(from, read_len(to.saturating_sub(from), WINDOW + MAGIC_REST))?;
            let starts = read_len(end.saturating_sub(from), WINDOW);
            for i in (0..starts).rev() {
                if !win.get(i..).is_some_and(|w| w.starts_with(&MAGIC)) {
                    continue;
                }
                let Some(at) = from.checked_add(wide(i)) else {
                    continue;
                };
                let Ok((bytes, end)) = self.block_bytes(at, hi, INDEX_LEN_MAX)? else {
                    continue;
                };
                let index = le_u8(&bytes, 4) == Some(VERSION)
                    && le_u8(&bytes, 5).map(BlockKind::from_u8) == Some(BlockKind::Index);
                if !index {
                    continue;
                }
                if let Ok(b) = OwnedBlock::read(bytes) {
                    return Ok(Some((at, b, end)));
                }
            }
            end = from;
        }
        Ok(None)
    }

    /// Read blocks one after the other from `from` to `hi` into `piece`, starting a new piece at a piece head and
    /// closing one at a tail; stops at the first block that isn't good.
    fn scan(&self, from: u64, hi: u64, piece: PieceInfo) -> R<Vec<PieceInfo>, S> {
        self.scan_with(from, hi, piece, SessionState::new(Medium::File))
    }

    /// [Self::scan] keeping to the sessions `walk` takes, reading past gaps its layout allows.
    fn scan_with(
        &self,
        from: u64,
        hi: u64,
        mut piece: PieceInfo,
        mut walk: SessionState,
    ) -> R<Vec<PieceInfo>, S> {
        let mut out = Vec::new();
        let mut at = from;
        piece.end = piece.end.max(from);
        // offset in the piece of the last block taken that is in the seq order: the one a replacement replaces
        let mut last_counted: Option<u64> = None;
        while at < hi {
            let mut replaces = false;
            let got = self.good_block(at, hi)?;
            let taken = match got {
                Ok((b, end)) => match walk.verdict(&b.block(), true) {
                    Verdict::Take => Some((b, end)),
                    // the same block again: dropped
                    Verdict::Duplicate { same: true } => {
                        at = end;
                        piece.end = at;
                        continue;
                    }
                    // other bytes under the same id: the later copy wins
                    Verdict::Duplicate { same: false } => {
                        replaces = true;
                        piece.replaced.push(at);
                        Some((b, end))
                    }
                    Verdict::Stale(_) | Verdict::Backwards { .. } => None,
                },
                Err(_) => None,
            };
            // a block of another version that goes on with the data starts a piece of that version: its blocks
            // would read as ones this reader skips; stale data of another version is judged by its session first
            if let Some((b, _)) = &taken
                && b.block().version != crate::VERSION
            {
                if replaces {
                    piece.replaced.pop();
                }
                if piece.end > piece.start {
                    set_scanned(&mut piece, since(at, from));
                    piece.settle_close();
                    out.push(piece);
                }
                let mut other = PieceInfo::new(at, Found::Scan { scanned: 0 });
                other.version = Some(b.block().version);
                other.end = hi;
                set_scanned(&mut other, since(hi, from));
                out.push(other);
                return Ok(out);
            }
            let Some((ob, end)) = taken else {
                if let Some(next) = self.past_gap(at, hi, &walk)?
                    && let Some(len) = next.checked_sub(at)
                {
                    piece.gaps.push((at, len));
                    at = next;
                    continue;
                }
                // on a raw medium blank space longer than a gap is the end, whatever follows it
                let end = match walk.medium {
                    Medium::File => hi,
                    Medium::Raw => at.saturating_add(walk.gap_max()).saturating_add(1).min(hi),
                };
                if !self.blank(at, end)? {
                    piece.cut = Some(at);
                    piece.cut_walk = Some(walk);
                }
                break;
            };
            let block = ob.block();
            walk.take(&block);
            if replaces
                && let Some(gone) = last_counted
                && let Some(list) = piece.entries.as_mut()
            {
                list.retain(|e| e.offset != gone);
            }
            // the piece starts at or before every block of it the scan takes
            let in_piece = at.checked_sub(piece.start);
            if !is_free(&block) {
                last_counted = in_piece;
            }
            match block.kind {
                BlockKind::PieceHead if at != piece.start => {
                    set_scanned(&mut piece, since(at, from));
                    piece.settle_close();
                    out.push(piece);
                    piece = PieceInfo::new(at, Found::Scan { scanned: 0 });
                    last_counted = None;
                    piece.take_head(&block);
                }
                BlockKind::PieceHead => piece.take_head(&block),
                BlockKind::Close => piece.meet_close(&block, in_piece),
                BlockKind::Tail => {
                    // the tail of the close just met, ending at it; another tail is not this close's
                    piece.tail = Tail::read(block.bytes())
                        .ok()
                        .filter(|t| piece.ends_close(t, in_piece));
                    if self.appended_after(&piece, at, end, hi)?
                        || self.torn_after(&piece, end, hi)?
                    {
                        // written on after this close, or bytes a crash tore there: the same piece goes on
                        at = end;
                        piece.end = at;
                        continue;
                    }
                    piece.end = end;
                    set_scanned(&mut piece, since(end, from));
                    piece.settle_close();
                    out.push(piece);
                    piece = PieceInfo::new(end, Found::Scan { scanned: 0 });
                    last_counted = None;
                    at = end;
                    continue;
                }
                _ => {
                    if let Some(off) = in_piece
                        && let Some(list) = piece.entries.as_mut()
                    {
                        list.extend(IndexEntry::all_of_block(off, &block));
                    }
                }
            }
            // a piece of a level this build refuses is read no further, as one of another version
            if block.kind == BlockKind::PieceHead && piece.refused().is_some() {
                piece.end = hi;
                set_scanned(&mut piece, since(hi, from));
                piece.settle_close();
                out.push(piece);
                return Ok(out);
            }
            at = end;
            piece.end = at;
        }
        let scanned = since(piece.end.max(at), from);
        set_scanned(&mut piece, scanned);
        piece.settle_close();
        if piece.end > piece.start || piece.cut.is_some() || out.is_empty() {
            out.push(piece);
        }
        Ok(out)
    }

    /// Whether the good block at `end` (within `hi`) is an append head going on from the close record and the tail at
    /// `tail_at` of `piece`: a writer wrote on after them, and the piece goes on.
    fn appended_after(&self, piece: &PieceInfo, tail_at: u64, end: u64, hi: u64) -> R<bool, S> {
        let Some((close, close_at)) = closed_unlinked(piece, end) else {
            return Ok(false);
        };
        let Some(tail_off) = tail_at.checked_sub(piece.start) else {
            return Ok(false);
        };
        if hi.saturating_sub(end) < wide(APPEND_HEAD_LEN) {
            return Ok(false);
        }
        Ok(match self.good_block(end, hi)? {
            Ok((b, _)) => AppendHead::read(&b.block())
                .is_ok_and(|h| h.follows(close_at, tail_off, &close.hash)),
            Err(_) => false,
        })
    }

    /// Whether bytes follow the whole tail ending at `end` of `piece`, a close naming no next piece, and don't read as
    /// a block: an append a crash tore (its append head cut, or junk where a writer was to write on). They are the
    /// piece's unclosed tail, never a piece of their own: only a piece head starts one after a close naming none.
    fn torn_after(&self, piece: &PieceInfo, end: u64, hi: u64) -> R<bool, S> {
        if end >= hi || closed_unlinked(piece, end).is_none() {
            return Ok(false);
        }
        Ok(self.good_block(end, hi)?.is_err())
    }

    /// Where the session's data goes on past the blank or bad stretch at `at` within `walk`'s gap rules: the first
    /// non-blank byte or a resume boundary holding a block that [resumes](SessionState::resumes) the walk; None when
    /// nothing does (or the layout allows no gaps), so the stretch is the end of data. Each place costs a header
    /// read; at most [GAP_CHECKS_MAX] whose header [may resume](SessionState::may_resume) the walk are read whole.
    fn past_gap(&self, at: u64, hi: u64, walk: &SessionState) -> R<Option<u64>, S> {
        let reach = walk.gap_max().min(hi.saturating_sub(at));
        if reach == 0 {
            return Ok(None);
        }
        // a block starts with its magic: blank places hold none
        let Some(first) = self.non_blank(at, at.saturating_add(reach))? else {
            return Ok(None);
        };
        let candidates = Some(first)
            .filter(|f| *f > at)
            .into_iter()
            .chain(walk.probes(at, first));
        let mut checks = 0;
        for c in candidates {
            if hi.saturating_sub(c) < wide(HEADER_LEN) {
                break;
            }
            let Some(head) = Head::read(&self.read(c, HEADER_LEN)?) else {
                continue;
            };
            if !walk.may_resume(&head) {
                continue;
            }
            if checks == GAP_CHECKS_MAX {
                break;
            }
            checks = checks.saturating_add(1);
            if let Ok((b, _)) = self.good_block(c, hi)?
                && walk.resumes(&b.block())
            {
                return Ok(Some(c));
            }
        }
        Ok(None)
    }

    /// The first byte from `at` to `end` that isn't blank, read [WINDOW] bytes at a time.
    fn non_blank(&self, mut at: u64, end: u64) -> R<Option<u64>, S> {
        while let Some(left) = end.checked_sub(at).filter(|n| *n > 0) {
            let n = read_len(left, WINDOW);
            let bytes = self.read(at, n)?;
            if let Some(i) = bytes.iter().position(|b| !crate::is_blank(*b)) {
                return Ok(at.checked_add(wide(i)));
            }
            // n is at most left: at stays within end
            at = at.saturating_add(wide(n));
        }
        Ok(None)
    }

    /// Whether everything from `at` to `hi` is blank ([crate::is_blank]: preallocated space, an erased medium): the
    /// end, not damage.
    fn blank(&self, at: u64, hi: u64) -> R<bool, S> {
        Ok(self.non_blank(at, hi)?.is_none())
    }

    /// Every block of piece `i` its index or scan knows, in file order; offsets from the piece's start. Entries
    /// that overlap, but for a block's state records, are refused ([IndexError::BadEntries]): from a final index,
    /// checkpoints or a scan alike.
    pub fn entries(&self, i: usize) -> R<Vec<IndexEntry>, S> {
        let p = self.piece(i)?;
        if let Some(e) = &p.entries {
            return in_order(p, e.clone());
        }
        let mut out = Vec::new();
        self.for_final(p, |ix| {
            out.extend(ix.entries());
        })?;
        in_order(p, out)
    }

    /// The blocks of track `track_id` in piece `i` (0: record blocks of data, [STATE_TRACK_ID](crate::index::STATE_TRACK_ID):
    /// state records, one entry each), in file order. A closed piece
    /// reads only the final index's head and that track's entries.
    ///
    /// The range relation of a track never decreases ([RelationIndex::Range](crate::RelationIndex::Range) is only on a
    /// monotone relation): each chunk's range starts at or above the top of the range before it of its track number,
    /// but at a chunk marked as a break ([IndexEntry::relation_break]); else [ReadError::RelationOrder].
    pub fn track_entries(&self, i: usize, track_id: TrackId) -> R<Vec<IndexEntry>, S> {
        let p = self.piece(i)?;
        let entries = self.track_entries_unordered(p, track_id)?;
        ranges_in_order(p, &entries)?;
        Ok(entries)
    }

    /// The breaks of the monotone relations of track `track_id` owned by `owner` over the pieces in order: each chunk
    /// marked [FLAG_RELATION_BREAK](crate::FLAG_RELATION_BREAK), by its piece and its index entry (where it lies, its
    /// first item, its range). A lookup by a monotone relation's value holds within the stretch between two breaks;
    /// one that spans a break may find the value on both sides of it.
    pub fn relation_breaks(
        &self,
        track_id: TrackId,
        owner: OwnerId,
    ) -> R<Vec<(usize, IndexEntry)>, S> {
        let mut out = Vec::new();
        for i in 0..self.pieces.len() {
            let numbers = self.numbers_of(i, track_id, owner)?;
            out.extend(
                self.track_entries(i, track_id)?
                    .into_iter()
                    .filter(|e| e.kind == BlockKind::Events && e.relation_break)
                    .filter(|e| e.track().is_some_and(|t| numbers.takes(t)))
                    .map(|e| (i, e)),
            );
        }
        Ok(out)
    }

    /// [Self::track_entries] of piece `p`, the ranges unchecked.
    fn track_entries_unordered(&self, p: &PieceInfo, track_id: TrackId) -> R<Vec<IndexEntry>, S> {
        if let Some(e) = &p.entries {
            let track = e.iter().filter(|e| e.track_id == track_id).copied();
            return in_order(p, track.collect());
        }
        let mut out = Vec::new();
        let mut at = p.close.as_ref().and_then(|(f, _)| f.final_index);
        while let Some(off) = at {
            let abs = in_piece(p, off, HEAD_LEN_PREFIX)?;
            let header = self.read(abs, HEAD_LEN_PREFIX)?;
            let head_len = IndexBlock::head_len(&header).map_err(ReadError::Index)?;
            in_piece(p, off, head_len)?;
            let head = self.read(abs, head_len)?;
            let ix = IndexBlock::read_head(&head).map_err(ReadError::Index)?;
            if ix.kind() != IndexKind::Final || ix.self_offset() != off {
                return Err(ReadError::Index(IndexError::BadHead));
            }
            if let Some(list) = ix.track(track_id) {
                let (from, n) = ix.track_range(&list);
                // a track's entries lie in the index block
                if n > MAX_BLOCK_LEN {
                    return Err(ReadError::Index(IndexError::BadEntries));
                }
                let at = in_piece(p, off.saturating_add(wide(from)), n)?;
                let bytes = self.read(at, n)?;
                out.extend(ix.track_entries(&list, &bytes).map_err(ReadError::Index)?);
            }
            at = ix.prev().filter(|p| *p < off);
        }
        in_order(p, out)
    }

    /// Call `f` with each final index block of closed piece `p`, newest first.
    fn for_final(&self, p: &PieceInfo, mut f: impl FnMut(&IndexBlock<'_>)) -> R<(), S> {
        let mut at = p.close.as_ref().and_then(|(fin, _)| fin.final_index);
        while let Some(off) = at {
            let abs = in_piece(p, off, HEADER_LEN)?;
            let b = match self.good_block(abs, p.end)? {
                Ok((b, _)) => b,
                Err(error) => return Err(ReadError::Block { offset: abs, error }),
            };
            let ix = IndexBlock::read(&b.block()).map_err(ReadError::Index)?;
            if ix.kind() != IndexKind::Final || ix.self_offset() != off {
                return Err(ReadError::Index(IndexError::BadHead));
            }
            f(&ix);
            at = ix.prev().filter(|p| *p < off);
        }
        Ok(())
    }

    /// Read the block of `entry` in piece `i` into `buf` and check it. Only the entry's offset and length are used:
    /// what the block holds is the caller's to compare with the entry ([Self::read_chunk_head] compares a chunk's).
    pub fn read_block<'b>(
        &self,
        i: usize,
        entry: &IndexEntry,
        buf: &'b mut Vec<u8>,
    ) -> R<Block<'b>, S> {
        let (offset, len) = self.place(i, entry)?;
        buf.resize(len, 0);
        self.src.read_at(offset, buf).map_err(ReadError::Source)?;
        Block::read(buf).map_err(|error| ReadError::Block { offset, error })
    }

    /// Read the head of the chunk block of `entry` in piece `i`, using `buf`: its header, then its bytes up to the
    /// payload, head CRC and ranges checked ([ChunkHead::read_head]). The payload is neither read nor checked, so a
    /// series' grids cost a few hundred bytes per chunk, not its samples. A head other than the one the entry names
    /// (kind, track id and number, first item, count) is refused ([IndexError::BadEntries]).
    pub fn read_chunk_head(
        &self,
        i: usize,
        entry: &IndexEntry,
        buf: &mut Vec<u8>,
    ) -> R<ChunkHead, S> {
        let (offset, len) = self.place(i, entry)?;
        let block = |error| ReadError::Block { offset, error };
        if len < CHUNK_HEAD_PREFIX {
            return Err(block(BlockError::Short {
                needed: CHUNK_HEAD_PREFIX,
            }));
        }
        buf.resize(CHUNK_HEAD_PREFIX, 0);
        self.src.read_at(offset, buf).map_err(ReadError::Source)?;
        // the entry's length bounds the read, as in read_block: the block lies within it
        let n = block_len(buf).map_err(block)?;
        if n > len {
            return Err(block(BlockError::Short { needed: n }));
        }
        let head_len = ChunkHead::head_len(buf).map_err(block)?;
        // head_len is within the block, whose length is within the piece
        buf.resize(head_len, 0);
        self.src.read_at(offset, buf).map_err(ReadError::Source)?;
        let head = ChunkHead::read_head(buf).map_err(block)?;
        if !entry.names_chunk(&head) {
            return Err(ReadError::Index(IndexError::BadEntries));
        }
        Ok(head)
    }

    /// The timeline of the series track `track_id` in piece `i`, of `period` ticks per sample, from the heads of its
    /// chunks alone ([Self::read_chunk_head]). A chunk whose head is damaged, or out of line with its neighbours, is
    /// left out with its error beside the timeline, as a lost chunk: the samples around it keep their grid. An
    /// index that can't be read, or a failing source, is an error.
    pub fn series_timeline(
        &self,
        i: usize,
        track_id: TrackId,
        period: Ratio64,
    ) -> R<Timeline<S>, S> {
        let start = self.piece(i)?.start;
        let mut buf = Vec::new();
        let mut heads = Vec::new();
        let mut errors = Vec::new();
        for e in self.track_entries(i, track_id)? {
            if e.kind != BlockKind::Series {
                continue;
            }
            let offset = start.saturating_add(e.offset);
            match self.read_chunk_head(i, &e, &mut buf) {
                Ok(h) => heads.push((h, offset)),
                // the index names another chunk than the one there: it is damaged
                Err(ReadError::Index(IndexError::BadEntries)) => errors.push(ReadError::Block {
                    offset,
                    error: BlockError::NotChunk,
                }),
                Err(ReadError::Block { offset, error }) => {
                    errors.push(ReadError::Block { offset, error });
                }
                Err(other) => return Err(other),
            }
        }
        heads.sort_by_key(|(h, _)| h.first_item);
        let mut timeline = SeriesTimeline::new(period);
        for (h, offset) in heads {
            if let Err(error) = timeline.push_chunk(&h) {
                errors.push(ReadError::Series { offset, error });
            }
        }
        Ok((timeline, errors))
    }

    /// Where the block of `entry` in piece `i` starts in the source, and its length: at most [MAX_BLOCK_LEN] and
    /// within the piece.
    fn place(&self, i: usize, entry: &IndexEntry) -> R<(u64, usize), S> {
        let p = self.piece(i)?;
        let at = p.start.saturating_add(entry.offset);
        // a block is never longer: the entry is damaged
        let Some(len) = usize::try_from(entry.len)
            .ok()
            .filter(|len| *len <= MAX_BLOCK_LEN)
        else {
            return Err(ReadError::Block {
                offset: at,
                error: BlockError::BadLength(entry.len),
            });
        };
        let Ok(offset) = in_piece::<S::Error>(p, entry.offset, len) else {
            return Err(ReadError::Block {
                offset: at,
                error: BlockError::Short { needed: len },
            });
        };
        Ok((offset, len))
    }

    /// Whether closed piece `i` holds up as a whole: its bytes before its close record hash to the close's content
    /// hash (each block that reads whole as it is, a chunk's payload by its hash, other bytes as they are,
    /// [piece](crate::piece) docs); its final index and the blocks walked are the same set: each block walked that
    /// its writer lists has exactly the entries its writer made of it, field for field (offset, length, kind, track,
    /// items, ticks, a chunk's unit hash of its bytes and its range relation's range from its column), but a
    /// byte-identical copy of the listed block before it; each entry names a block walked, or a block that doesn't
    /// read whole in bytes no block explains, ending before the close and before the next block walked; and each
    /// close row states the hash its track's blocks fold to from the head row's, or says it incomplete exactly where
    /// a damaged block kept or the head leaves units out. The strict check of what [Self::range_hash] takes from the
    /// index unread. Each monotone relation of a track starts each chunk at or above the chunk before it in the piece,
    /// but at a chunk marked [FLAG_RELATION_BREAK](crate::FLAG_RELATION_BREAK): else [ReadError::RelationOrder].
    ///
    /// The final index is read as the piece is walked, its track lists merged in file order: verify holds one entry
    /// per track list and the entries of one block, whatever the piece's size. A final index of more than
    /// [VERIFY_LISTS_MAX] track lists names more tracks than a writer gives a piece: it doesn't hold.
    ///
    /// The check ends at the close in force. The result says where: what it checked, and the bytes after it that no
    /// close covers ([Verification::unchecked]).
    pub fn verify(&self, i: usize) -> R<Verification, S> {
        let held = self.verify_counted(i)?;
        let p = self.piece(i)?;
        let (Some((close, _)), Some(at)) = (&p.close, p.close_offset) else {
            return Err(ReadError::Unclosed);
        };
        let checked_end = p.closed_end().unwrap_or_else(|| p.start.saturating_add(at));
        let hi = self.stretch_end(i);
        Ok(Verification {
            holds: held.holds,
            close: (at, close.hash),
            stretches: self.closes(i)?.len(),
            checked_end,
            unchecked: (hi > checked_end).then_some(checked_end..hi),
        })
    }

    /// Where the stretch of the byte source that piece `i` may hold ends: the next piece's start, or the source's
    /// length.
    fn stretch_end(&self, i: usize) -> u64 {
        self.pieces
            .get(i.saturating_add(1))
            .map_or(self.len, |n| n.start)
    }

    /// The bytes of piece `i`'s stretch of the byte source that its close in force doesn't cover, their place in the
    /// source: an unclosed tail (a writer writing on after the close, or one a crash stopped), bytes after a tail that
    /// read as no block, or the whole stretch of a piece without a close. None when the close in force ends the
    /// stretch. A reader that opened the source sees what it held then: a source that grew since has a
    /// [ByteSource::size] past [Self::len], and a new open reads the growth.
    pub fn unclosed(&self, i: usize) -> R<Option<core::ops::Range<u64>>, S> {
        let p = self.piece(i)?;
        let from = p.closed_end().unwrap_or(p.start);
        let hi = self.stretch_end(i);
        Ok((hi > from).then_some(from..hi))
    }

    /// The body of the summary block the close in force of piece `i` names ([Close::summary]), read into `buf`; None
    /// when it names none. Two reads past the tail: the close record is in hand, the block is read whole. A block
    /// that doesn't lie before the close record, isn't a summary block, fails its CRC or holds more than
    /// [CLOSE_SUMMARY_MAX](crate::piece::CLOSE_SUMMARY_MAX) bytes is refused.
    pub fn close_summary<'b>(&self, i: usize, buf: &'b mut Vec<u8>) -> R<Option<&'b [u8]>, S> {
        let p = self.piece(i)?;
        let (Some((close, _)), Some(close_at)) = (&p.close, p.close_offset) else {
            return Ok(None);
        };
        let Some((at, len)) = close.summary else {
            return Ok(None);
        };
        let bad = || ReadError::Index(IndexError::Invalid("a summary block the close names"));
        // a length read from the file: a summary block's at most, and the block before the close record
        let len = crate::FromFile::new(len)
            .at_most(
                crate::piece::CLOSE_SUMMARY_MAX
                    .saturating_add(HEADER_LEN)
                    .saturating_add(crate::CRC_LEN),
            )
            .ok()
            .ok_or_else(bad)?;
        if at.checked_add(wide(len)).is_none_or(|end| end > close_at) {
            return Err(bad());
        }
        let offset = in_piece(p, at, len)?;
        buf.resize(len, 0);
        self.src.read_at(offset, buf).map_err(ReadError::Source)?;
        let b = Block::read(buf).map_err(|error| ReadError::Block { offset, error })?;
        let body = crate::piece::close_summary_body(&b).ok_or_else(bad)?;
        Ok(Some(body))
    }

    /// The entries of piece `i` that its close in force covers: every block its index or scan knows before that
    /// close record, in file order. The blocks of an unclosed tail after it are left out; a piece without a close in
    /// force has none.
    pub fn closed_entries(&self, i: usize) -> R<Vec<IndexEntry>, S> {
        let p = self.piece(i)?;
        let Some(at) = p.close_offset.filter(|_| p.closed_end().is_some()) else {
            return Ok(Vec::new());
        };
        let mut out = self.entries(i)?;
        out.retain(|e| e.offset < at);
        Ok(out)
    }

    /// The close records of piece `i`, oldest first, each with its offset in the piece: the last good one
    /// ([PieceInfo::close]) and those before it, each named by the next ([Close::prev]). The walk back stops at a
    /// close record that doesn't read or doesn't lie before the one naming it: those after it are given.
    pub fn closes(&self, i: usize) -> R<Vec<(u64, Close)>, S> {
        let p = self.piece(i)?;
        let (Some((last, _)), Some(at)) = (&p.close, p.close_offset) else {
            return Ok(Vec::new());
        };
        let mut out = vec![(at, *last)];
        let mut prev = last.prev;
        let mut below = at;
        // each close record lies before the one naming it: the walk ends within the piece's blocks
        while let Some(off) = prev.filter(|off| *off < below) {
            let Some(abs) = p.start.checked_add(off) else {
                break;
            };
            let Ok((b, _)) = self.good_block(abs, p.end)? else {
                break;
            };
            let Ok(c) = CloseBlock::read(&b.block()) else {
                break;
            };
            let close = c.close();
            out.push((off, close));
            prev = close.prev;
            below = off;
        }
        out.reverse();
        Ok(out)
    }

    /// [Self::verify] with what its replay cost.
    pub(crate) fn verify_counted(&self, i: usize) -> R<verify::Verified, S> {
        let p = self.piece(i)?;
        let (Some((fin, rows)), Some(end)) = (&p.close, p.close_offset) else {
            return Err(ReadError::Unclosed);
        };
        // the closes before the last, by offset: the hash of each stretch chains over the one before's stated hash
        let mut earlier: BTreeMap<u64, ContentHash> = self
            .closes(i)?
            .into_iter()
            .filter(|(at, _)| *at < end)
            .map(|(at, c)| (at, c.hash))
            .collect();
        let mut stretches_hold = true;
        let mut pos = 0u64;
        // the close record before the stretch walked and the hash it states: an append head names them
        let mut last_close: Option<(u64, ContentHash)> = None;
        // where each track list of the final index is: its blocks read whole and checked once, its entries read later
        let mut lists: Vec<(u64, u32, u64)> = Vec::new();
        let read = self.for_final(p, |ix| {
            let block = p.start.saturating_add(ix.self_offset());
            for list in ix.tracks() {
                if lists.len() <= verify::VERIFY_LISTS_MAX {
                    let (from, _) = ix.track_range(&list);
                    lists.push((
                        block.saturating_add(wide(from)),
                        list.count,
                        wide(ix.entry_len()),
                    ));
                }
            }
        });
        let not = verify::Verified::default();
        match read {
            Ok(()) if lists.len() <= verify::VERIFY_LISTS_MAX => {}
            Ok(()) | Err(ReadError::Index(_) | ReadError::Block { .. }) => return Ok(not),
            Err(e) => return Err(e),
        }
        let fits = |e: &IndexEntry| {
            usize::try_from(e.len).is_ok_and(|len| in_piece::<S::Error>(p, e.offset, len).is_ok())
        };
        let mut listed =
            verify::Lists::new(|at, b: &mut [u8; ENTRY_LEN]| self.src.read_at(at, b), fits);
        for (at, count, step) in lists {
            listed.add(at, count, step);
        }
        let starts = p.head.as_ref().map_or(&[][..], |(_, t)| &t[..]);
        let mut replay = verify::Replay::new(starts, listed, end);
        let mut h = blake3::Hasher::new();
        // a walk that spent its budget before the close stops short: what it hashed is not the piece
        let walked = walk_piece(
            0,
            end,
            |at, buf| {
                let off = in_piece(p, at, buf.len())?;
                self.src.read_at(off, buf).map_err(ReadError::Source)
            },
            |step| {
                // a close record before the last ends a stretch: its hash checks the stretch, the next chains over it
                if let Some((&at, _)) = earlier.first_key_value()
                    && at <= pos
                    && let Some((_, stated)) = earlier.pop_first()
                {
                    stretches_hold &= at == pos && *h.finalize().as_bytes() == stated.0;
                    h = crate::piece::chained(&stated);
                    last_close = Some((at, stated));
                }
                match step {
                    Walked::Block(block) => {
                        // an append head goes on from the close before it, right after that close's tail
                        // the kind byte first: only an append head is read again
                        let head = block.get(5) == Some(&BlockKind::AppendHead.to_u8());
                        if let Some(b) = head.then(|| Block::read(block).ok()).flatten() {
                            let tail_at = pos.checked_sub(wide(TAIL_LEN));
                            stretches_hold &= AppendHead::read(&b).is_ok_and(|a| {
                                last_close.is_some_and(|(at, hash)| {
                                    tail_at.is_some_and(|t| a.follows(at, t, &hash))
                                })
                            });
                        }
                        pos = pos.saturating_add(wide(block.len()));
                        let chunk = crate::piece::hash_block(&mut h, block, None);
                        replay.block(block, chunk);
                    }
                    Walked::Raw(raw) => {
                        pos = pos.saturating_add(wide(raw.len()));
                        h.update(raw);
                        replay.raw(raw.len());
                    }
                }
            },
        )?;
        if let Some(e) = replay.listed().take_error() {
            return Err(ReadError::Source(e));
        }
        if let Some((offset, track_id, relation)) = replay.order() {
            return Err(ReadError::RelationOrder {
                offset: p.start.saturating_add(offset),
                track_id,
                relation,
            });
        }
        Ok(verify::Verified {
            holds: stretches_hold
                && walked
                && *h.finalize().as_bytes() == fin.hash.0
                && replay.check(rows),
            steps: replay.steps(),
            held: replay.listed().held().saturating_add(replay.group_max()),
        })
    }

    /// The hash of the whole chunks of track `track_id` owned by `owner` that hold an item in `items`, folded from
    /// `from` ([TrackHash::then]) over the pieces in order. Folded from [TrackHash::START], it doesn't depend on where
    /// the chunks lie: the same chunks in other pieces or files give the same hash. Each chunk's unit hash comes from
    /// its index entry, so no payload is read; a chunk whose entry holds none, and every chunk of a piece whose index
    /// doesn't read, is read and hashed instead; a chunk read by its entry must be the one the entry names
    /// ([IndexError::BadEntries] else). A piece's head and close rows tell which track number is that
    /// track; with none for the track there, any number another owner's row doesn't name. Records are not chunks: a
    /// track's records are left out.
    ///
    /// Each chunk folded starts at or after the end of the one before ([ReadError::Overlap] else); a copy of the chunk
    /// before (the same items and unit hash) takes its place, so a block written again folds once. A piece read
    /// without its index is walked block by block: a damaged chunk of the track whose head still reads is an error,
    /// as on the index path, and bytes no block explains are counted in [RangeHash::skipped].
    pub fn range_hash(
        &self,
        track_id: TrackId,
        owner: OwnerId,
        items: Range<ItemNum>,
        from: TrackHash,
    ) -> R<RangeHash, S> {
        let mut fold = Fold::new(from);
        let end_of = |head: &ChunkHead| head.first_item.saturating_add(u64::from(head.count));
        let wanted = |head: &ChunkHead| head.first_item < items.end && end_of(head) > items.start;
        for i in 0..self.pieces.len() {
            let numbers = self.numbers_of(i, track_id, owner)?;
            let ours = |e: &IndexEntry| {
                matches!(e.kind, BlockKind::Events | BlockKind::Series)
                    && e.track().is_some_and(|t| numbers.takes(t))
                    && e.first_item < items.end
                    && e.end_item() > items.start
            };
            let p = self.piece(i)?;
            if p.head.is_none() {
                fold.out.headless = fold.out.headless.saturating_add(1);
            }
            match self.track_entries(i, track_id) {
                Ok(entries) => {
                    let mut buf = Vec::new();
                    // an entry of the track of a kind this reader doesn't fold: its bytes are counted, never dropped
                    for e in entries.iter().filter(|e| unfolded(e.kind)) {
                        fold.skip_bytes(u64::from(e.len));
                    }
                    for e in entries.iter().filter(|e| ours(e)) {
                        let offset = p.start.saturating_add(e.offset);
                        let (unit, read) = if e.unit.is_none() {
                            let b = self.read_block(i, e, &mut buf)?;
                            // the chunk the entry names with the range it states, or the index is damaged
                            let c = b
                                .chunk()
                                .filter(|c| {
                                    e.names_chunk(&c.head()) && range_of_columns(c, e.range)
                                })
                                .ok_or(ReadError::Index(IndexError::BadEntries))?;
                            (TrackHash::chunk_unit(&c, &payload_hash(c.payload())), true)
                        } else {
                            (e.unit, false)
                        };
                        fold.push(unit, e.first_item, e.end_item(), offset, read)?;
                    }
                }
                Err(ReadError::Source(e)) => return Err(ReadError::Source(e)),
                Err(_) => {
                    let end = p.close_offset.unwrap_or(p.end.saturating_sub(p.start));
                    let mut at = p.start;
                    let mut failed = Ok(());
                    walk_piece(
                        0,
                        end,
                        |off, buf| {
                            let off = in_piece(p, off, buf.len())?;
                            self.src.read_at(off, buf).map_err(ReadError::Source)
                        },
                        |step| {
                            let offset = at;
                            let (bytes, whole) = match step {
                                Walked::Block(b) => (b, true),
                                Walked::Raw(b) => (b, false),
                            };
                            at = at.saturating_add(wide(bytes.len()));
                            if failed.is_err() {
                                return;
                            }
                            if !whole {
                                // a damaged chunk of the track, its head good: lost, as reading it by an index is
                                match ChunkHead::read_head(bytes) {
                                    Ok(head)
                                        if head.track_id == track_id
                                            && numbers.takes(head.track_number)
                                            && wanted(&head) =>
                                    {
                                        failed = Err(ReadError::Block {
                                            offset,
                                            error: BlockError::BadCrc,
                                        });
                                    }
                                    _ => fold.skip(bytes.len()),
                                }
                                return;
                            }
                            let Some(c) = Block::read(bytes).ok().and_then(|b| b.chunk()) else {
                                return;
                            };
                            let head = c.head();
                            if head.track_id == track_id
                                && numbers.takes(head.track_number)
                                && wanted(&head)
                            {
                                let unit = TrackHash::chunk_unit(&c, &payload_hash(c.payload()));
                                failed =
                                    fold.push(unit, head.first_item, end_of(&head), offset, true);
                            }
                        },
                    )?;
                    failed?;
                }
            }
        }
        Ok(fold.finish())
    }

    /// The track numbers piece `i`'s head and close rows give track `track_id` of `owner`, and those they give it
    /// for other owners.
    fn numbers_of(&self, i: usize, track_id: TrackId, owner: OwnerId) -> R<Numbers, S> {
        let p = self.piece(i)?;
        let rows = p
            .head
            .iter()
            .flat_map(|(_, t)| t.iter().map(|t| (t.track_id, t.owner, t.track_number)))
            .chain(
                p.close
                    .iter()
                    .flat_map(|(_, t)| t.iter().map(|t| (t.track_id, t.owner, t.track_number))),
            )
            .filter(|(id, ..)| *id == track_id);
        let mut n = Numbers::default();
        for (_, o, number) in rows {
            if o == owner {
                n.own.push(number);
            } else {
                n.others.push(number);
            }
        }
        Ok(n)
    }
}

/// The track numbers of one track in a piece, from its rows: its own, and those of the same id with other owners.
#[derive(Default)]
struct Numbers {
    own: Vec<TrackNum>,
    others: Vec<TrackNum>,
}

impl Numbers {
    /// Whether a chunk numbered `number` is the track's.
    fn takes(&self, number: TrackNum) -> bool {
        if self.own.is_empty() {
            !self.others.contains(&number)
        } else {
            self.own.contains(&number)
        }
    }
}

impl RangeHash {
    /// Take in a chunk of unit hash `unit` holding items `first..end`, read from the chunk if `read`.
    fn fold(&mut self, unit: &UnitHash, first: ItemNum, end: ItemNum, read: bool) {
        self.hash = self.hash.then(unit);
        let (start, stop) = (self.items.start, self.items.end);
        self.items = if self.chunks == 0 {
            first..end
        } else {
            start.min(first)..stop.max(end)
        };
        self.chunks = self.chunks.saturating_add(1);
        self.read = self.read.saturating_add(u64::from(read));
    }
}

/// A chunk [TapeReader::range_hash] holds back: a copy of it may follow.
#[derive(Copy, Clone)]
struct Held {
    unit: UnitHash,
    first: ItemNum,
    end: ItemNum,
    read: bool,
}

/// [TapeReader::range_hash]'s fold, one chunk behind: a copy of the chunk before (the same items and unit hash) takes
/// its place.
struct Fold {
    out: RangeHash,
    held: Option<Held>,
}

impl Fold {
    fn new(from: TrackHash) -> Self {
        Self {
            out: RangeHash {
                hash: from,
                items: ItemNum::ZERO..ItemNum::ZERO,
                chunks: 0,
                read: 0,
                skipped: 0,
                headless: 0,
            },
            held: None,
        }
    }

    /// Take in the chunk at `offset` of unit hash `unit` holding items `first..end`, read from the chunk if `read`.
    fn push<E>(
        &mut self,
        unit: UnitHash,
        first: ItemNum,
        end: ItemNum,
        offset: u64,
        read: bool,
    ) -> Result<(), ReadError<E>> {
        let next = Held {
            unit,
            first,
            end,
            read,
        };
        match self.held {
            // a copy: the same items and the same unit hash; other bytes stating the same items overlap
            Some(h) if h.first == first && h.end == end && h.unit == unit => {}
            Some(h) if first < h.end => {
                return Err(ReadError::Overlap {
                    offset,
                    end: h.end.get(),
                });
            }
            Some(h) => self.out.fold(&h.unit, h.first, h.end, h.read),
            None => {}
        }
        self.held = Some(next);
        Ok(())
    }

    /// Count `n` bytes a walk found no block in.
    fn skip(&mut self, n: usize) {
        self.skip_bytes(wide(n));
    }

    /// Count `n` bytes left out: of no block, or of a block of a kind not folded.
    fn skip_bytes(&mut self, n: u64) {
        self.out.skipped = self.out.skipped.saturating_add(n);
    }

    fn finish(mut self) -> RangeHash {
        if let Some(h) = self.held.take() {
            self.out.fold(&h.unit, h.first, h.end, h.read);
        }
        self.out
    }
}

/// What [TapeReader::range_hash] folded.
///
/// The index path trusts the writer's statement: a chunk folded by its index entry ([Self::stated]) folds by the unit
/// hash its writer stated there, held by the index CRCs and never compared with the chunk's bytes. [Self::read]
/// chunks were hashed from their bytes. [TapeReader::verify] is what proves the stated hashes: it replays the piece
/// and compares each entry with its block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RangeHash {
    /// The track's hash after the chunks folded, from the hash given.
    pub hash: TrackHash,
    /// The items of the chunks folded: whole chunks, so from the first one's first item to the last one's end; empty
    /// when no chunk holds an item of the range.
    pub items: Range<ItemNum>,
    /// Chunks folded.
    pub chunks: u64,
    /// Chunks of those whose unit hash was read from the chunk, not the index: an entry made by a scan, or a piece
    /// whose index doesn't read.
    pub read: u64,
    /// Bytes of pieces walked without their index that no block explains: damage that may have held chunks of the
    /// range whose heads are lost too; and bytes of blocks the track's index entries list of a kind this reader
    /// doesn't fold (a reserved kind). 0 when every byte walked is a block and every entry one folded.
    pub skipped: u64,
    /// Pieces whose head doesn't read (damaged, lost, of another layout): no head row states the track's number and
    /// owner, so their chunks are taken by the close rows, or with no close by the id alone: what the hash covers
    /// there no head vouched for. 0 when every piece's head read.
    pub headless: u64,
}

impl RangeHash {
    /// Chunks folded by the unit hash their index entry states, not read: the writer's statement, which
    /// [TapeReader::verify] proves.
    pub fn stated(&self) -> u64 {
        self.chunks.saturating_sub(self.read)
    }
}

/// Whether a track's entry of kind `kind` is one [TapeReader::range_hash] doesn't fold: not a chunk, records or a
/// summary (a reserved kind, or another kind's byte).
fn unfolded(kind: BlockKind) -> bool {
    !matches!(
        kind,
        BlockKind::Events | BlockKind::Series | BlockKind::Records | BlockKind::Summary
    )
}

/// Whether `range`, an index entry's, holds for chunk `c`: none, or the range of one of its relation columns.
fn range_of_columns(c: &crate::Chunk<'_>, range: Option<(u64, u64)>) -> bool {
    let Some(range) = range else {
        return true;
    };
    c.columns()
        .filter(|col| col.kind == crate::ColumnKind::Relation)
        .any(|col| {
            crate::times::RelationColumn::open(c, col.param, |_| false)
                .ok()
                .flatten()
                .is_some_and(|values| values.range() == range)
        })
}

/// Where `n` bytes at `off` from the start of piece `p` begin in the source, when they lie within the piece.
fn in_piece<E>(p: &PieceInfo, off: u64, n: usize) -> Result<u64, ReadError<E>> {
    p.start
        .checked_add(off)
        .filter(|abs| abs.checked_add(wide(n)).is_some_and(|end| end <= p.end))
        .ok_or(ReadError::Index(IndexError::BadHead))
}

/// `entries` of piece `p` (from its index or a scan's checkpoints), when every block they name lies in the piece; an
/// index naming a block past it is damaged.
/// `entries` of piece `p` in file order, each block listed once (a block's state records excepted) and within the
/// piece: a block named twice would be read and folded twice.
/// Refuse entries `entries` of one track id in piece `p`, in file order, where a chunk's range starts below the top of
/// the range before it of its track number without a break marked.
fn ranges_in_order<E>(p: &PieceInfo, entries: &[IndexEntry]) -> Result<(), ReadError<E>> {
    let mut top: alloc::collections::BTreeMap<u32, u64> = alloc::collections::BTreeMap::new();
    for e in entries
        .iter()
        .filter(|e| e.kind == BlockKind::Events && !e.repeat)
    {
        if e.relation_break {
            top.remove(&e.number);
        }
        let Some((lo, hi)) = e.range else {
            continue;
        };
        if top.get(&e.number).is_some_and(|t| lo < *t) {
            return Err(ReadError::RelationOrder {
                offset: p.start.saturating_add(e.offset),
                track_id: e.track_id,
                relation: None,
            });
        }
        top.insert(e.number, hi);
    }
    Ok(())
}

/// The close record of `piece` and its offset when its close and tail, the tail ending at `end`, read whole and name
/// no next piece: a writer may write on after them.
fn closed_unlinked(piece: &PieceInfo, end: u64) -> Option<(&Close, u64)> {
    let (Some((close, _)), Some(close_at), Some(tail)) =
        (&piece.close, piece.close_offset, piece.tail)
    else {
        return None;
    };
    let whole = piece.close_error.is_none()
        && tail.ends(close, close_at)
        && close.next.is_none()
        && tail.piece_len == since(end, piece.start);
    whole.then_some((close, close_at))
}

fn in_order<E>(
    p: &PieceInfo,
    mut entries: Vec<IndexEntry>,
) -> Result<Vec<IndexEntry>, ReadError<E>> {
    entries.sort_by_key(|e| e.offset);
    if !crate::index::in_file_order(&entries) {
        return Err(ReadError::Index(IndexError::BadEntries));
    }
    within_piece(p, entries)
}

fn within_piece<E>(
    p: &PieceInfo,
    entries: Vec<IndexEntry>,
) -> Result<Vec<IndexEntry>, ReadError<E>> {
    let fits = |e: &IndexEntry| {
        usize::try_from(e.len).is_ok_and(|len| in_piece::<E>(p, e.offset, len).is_ok())
    };
    if entries.iter().all(fits) {
        Ok(entries)
    } else {
        Err(ReadError::Index(IndexError::BadEntries))
    }
}

/// How much of `left` bytes fits one read into a `buf`-byte buffer; `left` may pass `usize::MAX` on 32-bit targets.
fn read_len(left: u64, buf: usize) -> usize {
    usize::try_from(left).map_or(buf, |left| left.min(buf))
}

/// Bytes a scan went from `from` to `to`, a place it reached going forward.
fn since(to: u64, from: u64) -> u64 {
    debug_assert!(to >= from, "a scan only goes forward");
    to.saturating_sub(from)
}

fn set_scanned(p: &mut PieceInfo, n: u64) {
    match &mut p.found {
        Found::Checkpoint { scanned } | Found::Scan { scanned } => *scanned = n,
        Found::Tail => {}
    }
}

#[cfg(test)]
mod tests {
    use super::read_len;

    #[test]
    fn read_len_takes_the_buffer_past_4_gib() {
        assert_eq!(read_len((1 << 32) + 5, 4096), 4096);
        assert_eq!(read_len(1 << 32, usize::MAX), (1u64 << 32) as usize);
        assert_eq!(read_len(5, 4096), 5);
        assert_eq!(read_len(u64::MAX, 1 << 20), 1 << 20);
    }
}
