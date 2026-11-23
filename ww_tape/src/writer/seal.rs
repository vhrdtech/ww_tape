//! [TapeWriter::seal_on]: raise a closed piece to [Integrity::Full], its bytes those a writer of that level writes
//! from the same calls.
//!
//! The piece is read block by block and written again through a writer of [Integrity::Full], each block where it
//! was:
//!
//! - the piece head names [Integrity::Full]; after a piece sealed before it ([Sealed]) it names that piece's content
//!   hash and the id derived from it ([PieceId::after]);
//! - a chunk marked [FLAG_PAYLOAD_UNCHECKED](crate::FLAG_PAYLOAD_UNCHECKED) gets its segment CRCs and its block CRC,
//!   the mark cleared; every other block of data is written as it was;
//! - each checkpoint and each end of the piece (the final index, the close record, the tail) is the writer's own,
//!   with the unit hashes, track hashes and content hash the writer computed over the blocks before it; an append
//!   head names the close before it again.
//!
//! Every field the seal fills has a fixed size, so each block keeps its length and offset: the seal can write over
//! the piece it reads, a block at a time. It reads each block before it writes over it and never reads behind what
//! it wrote. A regenerated block of another length (a piece no writer of this format wrote) is refused before it is
//! written ([SealError::Moved]).
//!
//! A chain is sealed in order, from its first piece of a level below Full: each piece's id comes from the content
//! hash of the piece before, so every later piece is sealed again too, Full ones included.

use alloc::vec::Vec;

use super::{End, TapeWriter, WriteError};
use crate::index::{IndexBlock, IndexKind, IndexPolicy};
use crate::piece::{
    APPEND_HEAD_LEN, CloseBlock, ContentHash, Integrity, PieceHeadBlock, PieceId, PieceIdent,
    TAIL_LEN, Tail, TrackStart, hash_block, piece_head_len, write_piece_head,
};
use crate::reader::{ByteSource, wide};
use crate::sink::{TapeSink, sink_err};
use crate::{Block, BlockId, BlockKind, HEADER_LEN, MAX_BLOCK_LEN, SyncPolicy};

/// Blocks a seal reads ahead from the start of a piece's end to its close record, at most: a close summary and the
/// final index blocks before it.
const END_BLOCKS_MAX: usize = 1 << 16;

/// A piece a seal raised to [Integrity::Full]: what the piece after it takes ([TapeWriter::seal_on]).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Sealed {
    /// The piece's content hash, as its last close names it.
    pub hash: ContentHash,
    /// Bytes of the piece, its tail included: the same before and after the seal.
    pub len: u64,
    /// Where the piece sits in its chain, None for a file without a piece head.
    pub ident: Option<PieceIdent>,
    /// The next piece's id, when the piece was cut.
    pub next: Option<PieceId>,
    /// Every track of the chain after the piece, with its hash: the next piece's head rows ([super::Closed::tracks]).
    pub tracks: Vec<TrackStart>,
    /// The level the piece had before the seal.
    pub was: Integrity,
}

/// Why a piece was not sealed ([TapeWriter::seal_on]).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SealError {
    /// The piece doesn't end in a whole tail: an unclosed piece is resumed and closed first.
    NotClosed,
    /// The piece doesn't start with a piece head: a file without one is of [Integrity::Full].
    NoHead,
    /// A block at `offset` doesn't read whole, or bytes there start no block: a damaged piece is not sealed.
    Damaged { offset: u64 },
    /// The block or end the writer makes at `offset` has another length than the one there: no writer of this format
    /// wrote the piece.
    Moved { offset: u64 },
    /// A piece sealed before it was given, and the piece is the first of its chain or that piece names no next one.
    Unlinked,
}

impl core::fmt::Display for SealError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotClosed => write!(f, "the piece is not closed"),
            Self::NoHead => write!(f, "the file has no piece head"),
            Self::Damaged { offset } => write!(f, "the piece is damaged at byte {offset}"),
            Self::Moved { offset } => write!(f, "a block at byte {offset} would change its length"),
            Self::Unlinked => write!(f, "the piece doesn't follow the piece sealed before it"),
        }
    }
}

impl core::error::Error for SealError {}

/// Where a piece lies in its bytes.
struct Src<'s, B> {
    bytes: &'s B,
    start: u64,
    len: u64,
}

impl<B: ByteSource> Src<'_, B> {
    /// The whole block at `at`, read into `buf`, with its kind: one that doesn't read whole is
    /// [SealError::Damaged].
    fn block<E: From<B::Error>>(
        &self,
        at: u64,
        buf: &mut Vec<u8>,
    ) -> Result<BlockKind, WriteError<E>> {
        let damaged = || WriteError::Seal(SealError::Damaged { offset: at });
        let left = self.len.checked_sub(at).ok_or_else(damaged)?;
        let mut header = [0u8; HEADER_LEN];
        if left < wide(HEADER_LEN) {
            return Err(damaged());
        }
        self.read(at, &mut header)?;
        // at most MAX_BLOCK_LEN, and within the piece
        let len = crate::block_len(&header)
            .ok()
            .filter(|n| wide(*n) <= left)
            .ok_or_else(damaged)?;
        buf.resize(len, 0);
        self.read(at, buf)?;
        let kind = Block::read(buf).map_err(|_bad| damaged())?.kind;
        Ok(kind)
    }

    fn read<E: From<B::Error>>(&self, at: u64, buf: &mut [u8]) -> Result<(), WriteError<E>> {
        let at = self
            .start
            .checked_add(at)
            .ok_or(WriteError::Seal(SealError::Damaged { offset: at }))?;
        self.bytes.read_at(at, buf).map_err(|e| WriteError::Sink {
            what: "reading the piece to seal",
            error: E::from(e),
        })
    }
}

impl<S: TapeSink> TapeWriter<S> {
    /// Write the closed piece `src` holds from `start` on, `len` bytes, into `out` at [Integrity::Full], see the
    /// [module docs](self): the bytes a writer of that level writes from the same calls, at the same offsets. After a
    /// piece sealed before it, `after` gives that piece, whose content hash this piece's head names; None keeps the
    /// head's link as it is. Gives the piece sealed and `out`.
    ///
    /// `out` is synced, not closed. It may write over `src` itself: each block is read before its bytes are written.
    /// The piece head goes out first; a sink writing in place holds it back and writes it once the rest is synced, so
    /// the head's level changes last. On an error the piece keeps the level its head names, and blocks written before
    /// it hold their CRCs and hashes.
    pub fn seal_on<B: ByteSource>(
        src: &B,
        (start, len): (u64, u64),
        out: S,
        after: Option<&Sealed>,
    ) -> Result<(Sealed, S), WriteError<S::Error>>
    where
        S::Error: From<B::Error>,
    {
        let src = Src {
            bytes: src,
            start,
            len,
        };
        // a whole tail ends the piece, read before any byte is written
        let mut tail = [0u8; TAIL_LEN];
        let tail_at = len
            .checked_sub(wide(TAIL_LEN))
            .ok_or(WriteError::Seal(SealError::NotClosed))?;
        src.read(tail_at, &mut tail)?;
        Tail::read(&tail).map_err(|_bad| WriteError::Seal(SealError::NotClosed))?;
        let mut w = Self::create_on(out, SyncPolicy::None);
        w.set_index_policy(IndexPolicy::NONE)?;
        // the layout and the producer after the head are blocks of the piece, written as they are
        w.head_put = true;
        let mut buf = Vec::new();
        if src.block(0, &mut buf)? != BlockKind::PieceHead {
            return Err(WriteError::Seal(SealError::NoHead));
        }
        let was = w.seal_head(&buf, after)?;
        let mut at = wide(buf.len());
        let mut last = None;
        while at < len {
            let kind = src.block(at, &mut buf)?;
            let block = Block::read(&buf).map_err(WriteError::Block)?;
            let checkpoint = kind == BlockKind::Index
                && IndexBlock::read(&block).is_ok_and(|ix| ix.kind() == IndexKind::Checkpoint);
            let ends = matches!(kind, BlockKind::CloseSummary | BlockKind::Close)
                || (kind == BlockKind::Index && !checkpoint);
            if checkpoint {
                let b = w
                    .indexer
                    .checkpoint(w.offset(), w.structural_id())
                    .map_err(WriteError::Index)?;
                if b.len() != buf.len() {
                    return Err(WriteError::Seal(SealError::Moved { offset: at }));
                }
                w.write_own(&b)?;
                at = w.offset();
            } else if ends {
                let sealed = w.seal_end(&src, at, &mut buf)?;
                at = sealed.len;
                last = Some(sealed);
            } else {
                if matches!(kind, BlockKind::Events | BlockKind::Series)
                    && block.flags & crate::FLAG_PAYLOAD_UNCHECKED != 0
                {
                    crate::chunk::fill_payload_crcs(&mut buf)
                        .map_err(|_bad| WriteError::Seal(SealError::Damaged { offset: at }))?;
                }
                w.write_own(&buf)?;
                at = w.offset();
            }
        }
        let last = last.ok_or(WriteError::Seal(SealError::NotClosed))?;
        w.sink
            .close()
            .map_err(sink_err("marking the piece sealed"))?;
        Ok((Sealed { was, ..last }, w.sink))
    }

    /// Write the piece head `block` again at [Integrity::Full], after the piece `after` when given; gives the level
    /// it named.
    fn seal_head(
        &mut self,
        block: &[u8],
        after: Option<&Sealed>,
    ) -> Result<Integrity, WriteError<S::Error>> {
        let b = Block::read(block).map_err(WriteError::Block)?;
        let h = PieceHeadBlock::read(&b).map_err(WriteError::Index)?;
        let mut head = h.head();
        let mut tracks: Vec<TrackStart> = h.tracks().collect();
        let was = head.integrity;
        head.integrity = Integrity::Full;
        if let Some(a) = after {
            let (Some(_), Some(ident), Some(next)) = (head.prev, a.ident, a.next) else {
                return Err(WriteError::Seal(SealError::Unlinked));
            };
            head.prev = Some((a.hash, ident.id));
            head.ident.id = next;
            // the tracks go on from the hashes the piece before ends with
            if a.tracks.len() != tracks.len() {
                return Err(WriteError::Seal(SealError::Moved { offset: 0 }));
            }
            tracks.clone_from(&a.tracks);
        }
        self.session = b.session;
        // the head carries the seq of the last counted block before it; a first piece starts at 0
        self.next_seq = match head.prev {
            Some(_) => b.seq.wrapping_add(1),
            None => 0,
        };
        let mut buf = alloc::vec![0u8; piece_head_len(tracks.len())];
        let n = write_piece_head(&mut buf, BlockId::new(b.session, b.seq), &head, &tracks)
            .map_err(WriteError::Index)?;
        buf.truncate(n);
        if n != block.len() {
            return Err(WriteError::Seal(SealError::Moved { offset: 0 }));
        }
        self.set_level(Integrity::Full);
        self.indexer.start(&tracks);
        self.write_own(&buf)?;
        self.head = Some((head, tracks));
        Ok(was)
    }

    /// Write the end of the piece that starts at `at` in `src` (a close summary, the final index, the close record
    /// and its tail) as the writer makes it, and an append head after it when blocks follow; gives the piece as that
    /// close leaves it, its length where the bytes after the end start.
    fn seal_end<B: ByteSource>(
        &mut self,
        src: &Src<'_, B>,
        at: u64,
        buf: &mut Vec<u8>,
    ) -> Result<Sealed, WriteError<S::Error>>
    where
        S::Error: From<B::Error>,
    {
        let damaged = |offset| WriteError::Seal(SealError::Damaged { offset });
        let mut summary = None;
        let mut pos = at;
        let mut close = None;
        for _ in 0..END_BLOCKS_MAX {
            let kind = src.block(pos, buf)?;
            match kind {
                BlockKind::CloseSummary if pos == at => summary = Some(buf.clone()),
                BlockKind::Index => {}
                BlockKind::Close => {
                    let b = Block::read(buf).map_err(WriteError::Block)?;
                    let c = CloseBlock::read(&b).map_err(WriteError::Index)?;
                    close = Some((c.close(), c.removed().collect::<Vec<_>>()));
                }
                _ => return Err(damaged(pos)),
            }
            pos = pos.saturating_add(wide(buf.len()));
            if close.is_some() {
                break;
            }
        }
        let (close, removed) = close.ok_or_else(|| damaged(pos))?;
        let mut tail = [0u8; TAIL_LEN];
        src.read(pos, &mut tail)?;
        Tail::read(&tail).map_err(|_bad| damaged(pos))?;
        let end = pos.saturating_add(wide(TAIL_LEN));
        let how = match close.next {
            Some(_) => End::Cut,
            None => End::Close(close.reason),
        };
        self.removed.clone_from(&removed);
        let fits = (at, end.saturating_sub(at));
        let (closed, close_at, close) = self.write_end(how, summary, Some(fits))?;
        let sealed = Sealed {
            hash: closed.hash,
            len: end,
            ident: closed.ident,
            next: closed.next,
            tracks: closed.tracks,
            was: Integrity::Full,
        };
        if end >= src.len {
            return Ok(sealed);
        }
        // blocks written on after the close: its append head first, which the writer makes again
        if src.block(end, buf)? != BlockKind::AppendHead || buf.len() != APPEND_HEAD_LEN {
            return Err(damaged(end));
        }
        let tail_at = end.saturating_sub(wide(TAIL_LEN));
        // what follows a close is hashed over it, from the close record on, as a resume walks it
        // the close record is a block, at most MAX_BLOCK_LEN long, written right before its tail
        let moved = || WriteError::Seal(SealError::Moved { offset: close_at });
        let close_len = tail_at
            .checked_sub(close_at)
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| *n <= MAX_BLOCK_LEN)
            .ok_or_else(moved)?;
        let mut ended = alloc::vec![0u8; close_len.saturating_add(TAIL_LEN)];
        self.sink
            .read_at(close_at, &mut ended)
            .map_err(sink_err("reading back the close written"))?;
        self.closed_by(close_at, tail_at, &close, removed);
        let (record, tail) = ended.split_at_checked(close_len).ok_or_else(moved)?;
        hash_block(&mut self.hash, record, None);
        hash_block(&mut self.hash, tail, None);
        self.write_on()?;
        Ok(Sealed {
            len: end.saturating_add(wide(APPEND_HEAD_LEN)),
            ..sealed
        })
    }
}
