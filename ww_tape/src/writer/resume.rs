//! [TapeWriter::resume_on]: reopen an unfinished piece after a crash and go on writing it.
//!
//! The piece is found the way [TapeReader] finds it: by its tail, else from its newest checkpoint
//! with the stretch after it scanned up to the first block that isn't good, else scanned from its start. Its bytes are
//! then walked from the start to where the reader stopped, block by block by the headers' lengths, a damaged block
//! the index lists by its entry, and the writer's state is rebuilt as the writer had it:
//!
//! - every block is accounted as a write accounts it: the BLAKE3 content hash, the index entries and
//!   the track ranges, the state records it holds ([super::near]), the counts, the session and the next seq;
//! - a damaged block the index lists stands for what its entries say: a state record in it leaves its slot unknown
//!   to the writer, so the next put writes;
//! - a checkpoint index block moves the indexer on as placing it did; in an aligned piece a checkpoint due after a
//!   block waits for the next chunk, as it did.
//!
//! The piece is kept up to the end of its last good block, counted or not: the puts after the last counted block
//! stay. What ends a piece (the final index, the finish, the tail) is dropped: the piece is open again, and so are
//! puts at the end that belong with a block after them ([FLAG_WITH_NEXT](crate::FLAG_WITH_NEXT)): that call is given
//! again whole. When the last block kept is counted, the writer acts after it again (a checkpoint when due), as a
//! crash may have come before it did; a piece holding only its head gets the layout and the producer put after it.
//!
//! Since what the writer adds depends only on the blocks before it, the calls since and the [WriterOptions] (give the
//! ones it was written with), the same calls after the resume as after the last block kept give the bytes, and the
//! content hash, of a piece written without the crash. A put is idempotent: an application puts again what it uses
//! (a put whose item lies close behind writes nothing), so it needs no record of which puts made it. Blocks of data
//! are given again from [TapeWriter::next_id] on; a call that places several counted blocks (a chunk with deferred
//! blocks in its gap) is given again whole.
//!
//! The writer keeps no definitions: the application passes its own with each call after the resume, as before. The
//! layout and the producer of the [WriterOptions] are checked against those the piece names, before anything is cut
//! off or written: another one is refused ([WriteError::Layout], [WriteError::Producer]), as the writer would place
//! blocks the way the file says it doesn't. A piece whose layout or producer record didn't survive is checked
//! against nothing: the options say what it is.
//!
//! Every length and offset read back goes through [FromFile] and a cap ([crate::MAX_BLOCK_LEN]) or the piece's end;
//! the walk moves on at least one byte a step, so it ends within the piece's length.

use std::collections::BTreeMap;
use std::io;

use super::{TapeWriter, WriteError, WriterOptions};
use crate::index::{IndexBlock, IndexEntry, IndexKind, STATE_TRACK_ID, is_structural};
use crate::piece::{FinishState, PieceHeadBlock};
use crate::reader::{ByteSource, ReadError, TapeReader, wide};
use crate::sink::{TapeSink, io_err};
use crate::{
    Block, BlockKind, CRC_LEN, FLAG_REPEAT, FromFile, HEADER_LEN, MAX_BLOCK_LEN, block_len,
};

/// Bytes the walk reads and hashes at once in a stretch that holds no block.
const JUNK_READ_MAX: usize = 1 << 20;

/// What [TapeWriter::resume_on] kept of a piece.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Resumed {
    /// Bytes of the piece kept: up to the end of its last good block, without the end of a piece.
    pub kept_bytes: u64,
    /// Bytes dropped after them: the end of a piece (final index, finish, tail), and whatever the reader didn't take
    /// (a cut block, damage, stale data).
    pub dropped_bytes: u64,
    /// Where the reader stopped taking blocks before the end of the bytes ([crate::PieceInfo::cut]).
    pub cut: Option<u64>,
    /// The kept blocks the index lists, in file order, offsets from the piece's start: what the final index will
    /// list of them.
    pub entries: Vec<IndexEntry>,
    /// The state of a finish block found (a crash after the finish, before the name): the piece was ended, and is
    /// open again.
    pub ended: Option<FinishState>,
    /// The layout the piece names, which the options were checked against; None when no layout record was kept.
    pub layout: Option<crate::Layout>,
}

impl Resumed {
    /// A piece started anew, `bytes` long (its head and the puts after it).
    pub(crate) fn started(bytes: u64) -> Self {
        Resumed {
            kept_bytes: bytes,
            dropped_bytes: 0,
            cut: None,
            entries: Vec::new(),
            ended: None,
            layout: None,
        }
    }
}

/// A [TapeSink] read as a [ByteSource].
pub(crate) struct SinkSource<'a, S>(pub(crate) &'a S);

impl<S: TapeSink> ByteSource for SinkSource<'_, S> {
    type Error = io::Error;

    fn size(&self) -> io::Result<u64> {
        self.0.size()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.0.read_at(offset, buf)
    }
}

/// One step of the walk: a block (good or not) or a stretch that holds none.
#[derive(Copy, Clone, Debug)]
struct Step {
    at: u64,
    len: u64,
    what: What,
}

#[derive(Copy, Clone, Debug)]
enum What {
    /// A block by its header or its index entries: its kind, header flags (0 when unreadable) and first entry.
    Block {
        kind: BlockKind,
        flags: u16,
        entry: Option<IndexEntry>,
    },
    /// Bytes no header or entry explains: a gap, damage.
    Junk,
}

impl Step {
    fn end(&self) -> u64 {
        // a step lies within the piece: at + len <= its end
        self.at.saturating_add(self.len)
    }

    /// A block the writer counts: not one of the file structure, not a block of puts or copies.
    fn counted(&self) -> bool {
        match self.what {
            What::Block {
                kind, flags, entry, ..
            } => {
                let repeat = flags & FLAG_REPEAT != 0 || entry.is_some_and(|e| e.repeat);
                !is_structural(kind) && !repeat
            }
            What::Junk => false,
        }
    }
}

/// The piece's blocks as the reader knows them: index entries and gaps by offset, and where the piece ends.
struct Layout {
    /// The entries of each block by offset: one, or one per state record
    entries: BTreeMap<u64, Vec<IndexEntry>>,
    gaps: BTreeMap<u64, u64>,
    end: u64,
}

impl Layout {
    /// The step at `at` (before [Self::end]), its header read from `src`.
    fn step<S: TapeSink>(&self, src: &S, at: u64) -> Result<Step, WriteError> {
        let left = self.end.saturating_sub(at);
        if let Some(&len) = self.gaps.get(&at) {
            return Ok(Step {
                at,
                len: len.clamp(1, left),
                what: What::Junk,
            });
        }
        let mut header = [0u8; HEADER_LEN];
        let has_header = left >= wide(HEADER_LEN);
        if has_header {
            src.read_at(at, &mut header)
                .map_err(io_err("reading back a block header"))?;
        }
        let flags = u16::from_le_bytes([header[6], header[7]]);
        if let Some(e) = self.entries.get(&at).and_then(|e| e.first()) {
            // an entry read from the file: a block's length at most, within the piece
            let len = FromFile::new(e.len)
                .at_most(MAX_BLOCK_LEN)
                .ok()
                .map(wide)
                .filter(|len| *len >= wide(HEADER_LEN + CRC_LEN) && *len <= left);
            if let Some(len) = len {
                let same = has_header && BlockKind::from_u8(header[5]) == e.kind;
                let flags = if same { flags } else { 0 };
                return Ok(Step {
                    at,
                    len,
                    what: What::Block {
                        kind: e.kind,
                        flags,
                        entry: Some(*e),
                    },
                });
            }
        }
        if has_header
            && let Ok(len) = block_len(&header)
            && wide(len) <= left
        {
            return Ok(Step {
                at,
                len: wide(len),
                what: What::Block {
                    kind: BlockKind::from_u8(header[5]),
                    flags,
                    entry: None,
                },
            });
        }
        // nothing reads here: up to the next block the index lists or the next gap
        let after = at.saturating_add(1);
        let to = [
            self.entries.range(after..).next().map(|(k, _)| *k),
            self.gaps.range(after..).next().map(|(k, _)| *k),
        ]
        .into_iter()
        .flatten()
        .fold(self.end, u64::min);
        Ok(Step {
            at,
            len: to.saturating_sub(at).max(1),
            what: What::Junk,
        })
    }
}

/// Where the kept part of a piece ends, from its steps up to the end of a piece: after the last of them that isn't a
/// block of puts belonging with the block after it ([FLAG_WITH_NEXT](crate::FLAG_WITH_NEXT)): that call is given
/// again.
#[derive(Default)]
struct KeptEnd {
    /// End of the last step kept
    last: u64,
    /// Whether that step is a counted block
    counted: bool,
}

impl KeptEnd {
    fn take(&mut self, s: &Step) {
        if let What::Block { flags, .. } = s.what
            && flags & crate::FLAG_WITH_NEXT != 0
        {
            return;
        }
        self.last = s.end();
        self.counted = s.counted();
    }
}

/// Whether step `s` of the piece in `src` ends the piece: a finish, a tail or a final index block.
fn ends_piece<S: TapeSink>(src: &S, s: &Step) -> Result<bool, WriteError> {
    let What::Block { kind, .. } = s.what else {
        return Ok(false);
    };
    match kind {
        BlockKind::Finish | BlockKind::Tail => Ok(true),
        BlockKind::Index => {
            // a step is at most MAX_BLOCK_LEN long
            let len = usize::try_from(s.len).map_err(|_overflow| too_long(s.len))?;
            let mut buf = vec![0u8; len];
            src.read_at(s.at, &mut buf)
                .map_err(io_err("reading back an index block"))?;
            Ok(Block::read(&buf)
                .ok()
                .and_then(|b| {
                    IndexBlock::read(&b)
                        .ok()
                        .map(|ix| ix.kind() == IndexKind::Final)
                })
                .unwrap_or(false))
        }
        _ => Ok(false),
    }
}

/// The newest layout and producer the kept good blocks name.
#[derive(Default)]
struct Named {
    layout: Option<crate::Layout>,
    producer: Option<String>,
}

impl Named {
    /// Take the layout and producer records of the good record block of `step` in `src`.
    fn take<S: TapeSink>(&mut self, src: &S, step: &Step) -> Result<(), WriteError> {
        let What::Block {
            kind: BlockKind::Records,
            ..
        } = step.what
        else {
            return Ok(());
        };
        // a block step is at most MAX_BLOCK_LEN long
        let len = usize::try_from(step.len).map_err(|_overflow| too_long(step.len))?;
        let mut buf = vec![0u8; len];
        src.read_at(step.at, &mut buf)
            .map_err(io_err("reading back a block"))?;
        let Ok(b) = Block::read(&buf) else {
            return Ok(());
        };
        let mut body = Vec::new();
        let Ok(b) = b.decode(&mut body) else {
            return Ok(());
        };
        for r in b
            .records()
            .flatten()
            .filter(|r| r.track == crate::DESCRIBE_TRACK)
        {
            match <crate::Describe<'_> as shrink_wrap::DeserializeShrinkWrap>::from_ww_bytes(r.item)
            {
                Ok(crate::Describe::Layout(l)) => self.layout = Some(l),
                Ok(crate::Describe::Producer(p)) => self.producer = Some(p.name.to_owned()),
                _ => {}
            }
        }
        Ok(())
    }
}

impl<S: TapeSink> TapeWriter<S> {
    /// Reopen the unfinished piece in `sink` (cut off by a crash, or ended and not yet named) to go on writing it,
    /// see the [module docs](self). `options` are the ones it was written with. The piece is truncated to the end of
    /// its last counted block, and the writer acts after that block as it did; the caller then gives it the blocks
    /// from [Self::next_id] on, with the puts between them. Definitions in a damaged block are lost to the writer
    /// unless a later block holds them. The producer seq of [Self::adopt_block] starts again: the first block
    /// adopted shows no gap.
    pub fn resume_on(sink: S, options: WriterOptions) -> Result<(Self, Resumed), WriteError> {
        let size = sink
            .size()
            .map_err(io_err("reading the length to resume"))?;
        let (layout, cut, ended) = read_layout(&sink)?;
        let mut kept = KeptEnd::default();
        let mut at = 0;
        while at < layout.end {
            let step = layout.step(&sink, at)?;
            if ends_piece(&sink, &step)? {
                break;
            }
            kept.take(&step);
            at = step.end();
        }
        let kept_bytes = kept.last;
        let mut w = Self::create_on(sink, options.sync);
        w.apply(&options)?;
        let mut buf = Vec::new();
        let mut named = Named::default();
        let mut at = 0;
        let mut past_head = false;
        while at < kept_bytes {
            let step = layout.step(&w.sink, at)?;
            let entries = layout.entries.get(&at).cloned().unwrap_or_default();
            w.replay(&step, &entries, &mut buf)?;
            named.take(&w.sink, &step)?;
            past_head |= !matches!(
                step.what,
                What::Block {
                    kind: BlockKind::PieceHead,
                    ..
                }
            );
            at = step.end();
        }
        if let Some(described) = named.layout
            && described != options.layout
        {
            return Err(WriteError::Layout {
                described,
                given: options.layout,
            });
        }
        if let Some(described) = named.producer
            && described != options.producer
        {
            return Err(WriteError::Producer {
                described,
                given: options.producer,
            });
        }
        if size > kept_bytes {
            w.sink
                .truncate(kept_bytes)
                .map_err(io_err("truncating the piece to resume"))?;
        }
        // the content hash of the bytes kept, as a reader verifies it: the walk above may have split damaged bytes
        // another way
        let mut hash = blake3::Hasher::new();
        crate::piece::hash_piece(&mut hash, kept_bytes, |at, buf| w.sink.read_at(at, buf))
            .map_err(io_err("reading back the piece to hash"))?;
        w.hash = hash;
        let entries = w.indexer.entries().to_vec();
        // the layout and the producer follow the head first: a block past it means they were written
        w.head_put = past_head;
        if kept.counted {
            w.after_block()?;
        } else if w.head.is_some() {
            w.ensure_head()?;
        }
        let resumed = Resumed {
            kept_bytes,
            dropped_bytes: size.saturating_sub(kept_bytes),
            cut,
            entries,
            ended,
            layout: named.layout,
        };
        Ok((w, resumed))
    }

    /// Account the bytes of `step`, read into `buf`, as writing them did; `entries` are the index's of the block.
    fn replay(
        &mut self,
        step: &Step,
        entries: &[IndexEntry],
        buf: &mut Vec<u8>,
    ) -> Result<(), WriteError> {
        let What::Block { kind, entry, .. } = step.what else {
            return self.replay_junk(step, buf);
        };
        // a block step is at most MAX_BLOCK_LEN long
        let len = usize::try_from(step.len).map_err(|_overflow| too_long(step.len))?;
        buf.resize(len, 0);
        self.sink
            .read_at(step.at, buf)
            .map_err(io_err("reading back a block"))?;
        let good = Block::read(buf).is_ok_and(|b| b.len() == len);
        match (good, entry) {
            (true, _) => self.replay_good(step.at, buf),
            (false, Some(e)) => self.replay_damaged(entries, e, kind, buf),
            (false, None) => self.replay_junk(step, buf),
        }
    }

    fn replay_good(&mut self, at: u64, block: &[u8]) -> Result<(), WriteError> {
        let b = Block::read(block).map_err(WriteError::Block)?;
        let (Some(header), Some(crc)) = (
            block.first_chunk::<HEADER_LEN>(),
            block.last_chunk::<CRC_LEN>(),
        ) else {
            return Err(WriteError::Block(crate::BlockError::Short {
                needed: HEADER_LEN,
            }));
        };
        match b.kind {
            BlockKind::PieceHead if at == 0 => {
                if let Ok(h) = PieceHeadBlock::read(&b) {
                    let head = h.head();
                    self.session = b.session;
                    // the head carries the seq of the last counted block before it; a first piece starts at 0
                    self.next_seq = match head.prev {
                        Some(_) => b.seq.wrapping_add(1),
                        None => 0,
                    };
                    let tracks: Vec<_> = h.tracks().collect();
                    self.indexer.start(&tracks);
                    self.head = Some((head, tracks));
                }
                self.account(block, header, *crc)
            }
            BlockKind::Index => {
                let checkpoint = IndexBlock::read(&b)
                    .is_ok_and(|ix| ix.kind() == IndexKind::Checkpoint && ix.self_offset() == at);
                if checkpoint {
                    // moves the indexer on as placing it did; the bytes are the file's
                    self.indexer
                        .checkpoint(at, crate::BlockId::new(b.session, b.seq))
                        .map_err(WriteError::Index)?;
                    self.index_due = false;
                }
                self.account(block, header, *crc)
            }
            _ => {
                let counted = !is_structural(b.kind) && !b.is_repeat();
                self.account(block, header, *crc)?;
                if counted && self.alignment() != 0 && self.checkpoint_due() {
                    self.index_due = true;
                }
                Ok(())
            }
        }
    }

    /// A block that doesn't read whole, which the index lists: its entries stand for it.
    fn replay_damaged(
        &mut self,
        entries: &[IndexEntry],
        e: IndexEntry,
        kind: BlockKind,
        bytes: &[u8],
    ) -> Result<(), WriteError> {
        let end = self.end_after(bytes.len())?;
        self.hash.update(bytes);
        self.indexer.placed_entries(entries, bytes.len());
        self.bytes = end;
        self.blocks = self.blocks.saturating_add(1);
        let data = match kind {
            BlockKind::Records => e.track_id != STATE_TRACK_ID,
            BlockKind::Events | BlockKind::Series => true,
            _ => false,
        };
        if data && !e.repeat {
            self.holds_data = true;
        }
        if matches!(kind, BlockKind::Events | BlockKind::Series) {
            self.chunks = self.chunks.saturating_add(1);
        }
        // a state record in it: its slot's newest record is here, its item unknown
        for s in entries.iter().filter_map(IndexEntry::state) {
            self.near.wrote(s.slot, None, e.offset, &self.near_policy);
        }
        Ok(())
    }

    /// Bytes no block or entry explains: hashed, placed, never listed.
    fn replay_junk(&mut self, step: &Step, buf: &mut Vec<u8>) -> Result<(), WriteError> {
        let mut at = step.at;
        let end = step.end();
        while at < end {
            let n = usize::try_from(end.saturating_sub(at))
                .map_or(JUNK_READ_MAX, |n| n.min(JUNK_READ_MAX));
            buf.resize(n, 0);
            self.sink
                .read_at(at, buf)
                .map_err(io_err("reading back damaged bytes"))?;
            self.hash.update(buf);
            self.indexer.placed_unlisted(n);
            self.bytes = self.end_after(n)?;
            // n is at most end - at
            at = at.saturating_add(wide(n));
        }
        Ok(())
    }
}

/// The piece at the start of `sink` as a reader finds it: its blocks by index entry and gap, where it ends, where a
/// scan was cut, and the state of a finish found.
fn read_layout<S: TapeSink>(
    sink: &S,
) -> Result<(Layout, Option<u64>, Option<FinishState>), WriteError> {
    let reader = TapeReader::open(SinkSource(sink)).map_err(WriteError::Read)?;
    let Some(p) = reader.pieces().first() else {
        return Ok((
            Layout {
                entries: BTreeMap::new(),
                gaps: BTreeMap::new(),
                end: 0,
            },
            None,
            None,
        ));
    };
    if p.start != 0 {
        return Err(WriteError::Read(ReadError::Index(
            crate::IndexError::Invalid("the piece to resume starts the bytes"),
        )));
    }
    let end = p.end;
    let mut entries: BTreeMap<u64, Vec<IndexEntry>> = BTreeMap::new();
    for e in reader.entries(0).map_err(WriteError::Read)? {
        if let Ok(at) = FromFile::new(e.offset).bounded(end) {
            entries.entry(at).or_default().push(e);
        }
    }
    let gaps = p.gaps.iter().copied().collect();
    let ended = p.finish.as_ref().map(|(f, _)| f.state);
    Ok((Layout { entries, gaps, end }, p.cut, ended))
}

fn too_long(len: u64) -> WriteError {
    WriteError::Block(crate::BlockError::BadLength(
        u32::try_from(len).unwrap_or(u32::MAX),
    ))
}
