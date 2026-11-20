//! [TapeWriter::resume_on]: reopen an unclosed piece after a crash and go on writing it.
//!
//! The piece is found the way [TapeReader] finds it: by its tail, else from its newest checkpoint
//! with the stretch after it scanned up to the first block that isn't good, else scanned from its start. The bytes
//! kept are then walked from the start as a reader's [verify](crate::TapeReader::verify) walks them (the content-hash
//! walk: good blocks, and bytes no block explains), and the writer's state is rebuilt as the writer had it. A good
//! block is taken wherever the walk meets one; in bytes no block explains, a block the index lists is a damaged block
//! kept by its entry when it ends before the next good block, else those bytes are junk, placed and never listed:
//!
//! - every block is accounted as a write accounts it: the BLAKE3 content hash, the index entries and
//!   the track ranges, the state records it holds ([super::near]), the counts, the session and the next seq;
//! - a damaged block the index lists stands for what its entries say: a state record in it leaves its slot unknown
//!   to the writer, so the next put writes; a chunk goes into its track's hash by the unit hash of its entry, the
//!   records of a damaged block of data don't (its entry names no track): each track that may hold records (the
//!   piece defines or its head carries it, no series or state track) leaves them out of its hash and its close row
//!   is marked incomplete ([TrackRange::incomplete](crate::piece::TrackRange::incomplete)), as is the track of a
//!   damaged chunk whose entry holds no unit hash;
//! - a checkpoint index block moves the indexer on as placing it did; in an aligned piece a checkpoint due after a
//!   block waits for the next chunk, as it did.
//!
//! A piece whose last close and tail read whole is closed, not ended: the resume cuts off a torn append after the
//! tail, then writes on after the tail, an append head ([AppendHead]) first, every byte before it as it was. A good
//! block after the tail refuses ([ResumeError::GoodBlockAfterCut]). A close naming a next piece (a cut) ends its piece
//! for good: the resume gives it its name ([TapeSink::close]) and returns [ResumeError::Continued]. Blocks after an
//! append head that names a whole close and tail, naming no next piece, are the piece going on: the resume keeps that
//! close and goes on after them.
//!
//! Else the piece is kept up to the end of its last good block, counted or not: the puts after the last counted block
//! stay. What ends a piece (the final index, a close without its tail) is dropped: the piece is open again, and so are
//! puts at the end that belong with a block after them ([FLAG_WITH_NEXT](crate::FLAG_WITH_NEXT)): that call is given
//! again whole.
//!
//! Only a torn end is ever cut: whole blocks, then a record the bytes stop inside of, or blank (zero) bytes to the
//! end. A whole record in the bytes to cut that fails its check, bytes that start no record, or a whole tail that
//! didn't end its close is damage: the resume refuses ([ResumeError::Damaged]) and every byte stays. When the last block kept is counted, the writer acts after it again (a checkpoint when due), as a
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
//! off or written: another one is refused ([InputError::Layout], [InputError::Producer]), as the writer would place
//! blocks the way the file says it doesn't. A piece whose layout or producer record didn't survive is checked
//! against nothing: the options say what it is.
//!
//! Every length and offset read back goes through [FromFile] and a cap ([crate::MAX_BLOCK_LEN]) or the piece's end;
//! the walk moves on at least one byte a step, so it ends within the piece's length.

use alloc::borrow::ToOwned;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use super::{InputError, ResumeError, TapeWriter, WriteError, WriterOptions};
use crate::index::{IndexBlock, IndexEntry, IndexKind, STATE_TRACK_ID, is_structural};
use crate::piece::{
    AppendHead, Close, CloseBlock, CloseReason, PieceHeadBlock, TAIL_LEN, Tail, Walked, hash_block,
    walk_piece,
};
use crate::reader::{ByteSource, ReadError, TapeReader, wide};
use crate::sink::{TapeSink, sink_err};
use crate::{
    Block, BlockKind, CRC_LEN, FLAG_REPEAT, FromFile, HEADER_LEN, MAX_BLOCK_LEN, block_len,
};

/// Bytes the walk reads and hashes at once in a stretch that holds no block.
const JUNK_READ_MAX: usize = 1 << 20;

/// Bytes past where the reader stopped that a resume looks through for a good block before it cuts them off: a torn
/// block is at most [MAX_BLOCK_LEN] long, and a block flushed before it lies right after it: twice that.
pub const RESUME_LOOK_MAX: u64 = 128 << 20;
/// Places past where the reader stopped that a resume reads whole as a block of the piece's sessions, at most.
pub const RESUME_PROBES_MAX: usize = 4;

/// What [TapeWriter::resume_on] kept of a piece.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Resumed {
    /// Bytes of the piece kept: up to the end of its last good block, without the end of a piece.
    pub kept_bytes: u64,
    /// Bytes dropped after them: the end of a piece (final index, close, tail), and whatever the reader didn't take
    /// (a cut block, damage, stale data).
    pub dropped_bytes: u64,
    /// Where the reader stopped taking blocks before the end of the bytes ([crate::PieceInfo::cut]).
    pub cut: Option<u64>,
    /// The kept blocks the index lists, in file order, offsets from the piece's start: what the final index will
    /// list of them.
    pub entries: Vec<IndexEntry>,
    /// The state of a close record found without a whole tail after it (a crash in the end of the piece): the piece
    /// was being ended, and is open again.
    pub ended: Option<CloseReason>,
    /// The layout the piece names, which the options were checked against; None when no layout record was kept.
    pub layout: Option<crate::Layout>,
    /// The last close record kept: the writer writes on after it. A closed piece is written on after its close and
    /// tail, with an append head first; a piece with blocks after its last close (a writer stopped while writing on)
    /// goes on after them.
    pub after_close: Option<Close>,
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
            after_close: None,
        }
    }
}

/// A [TapeSink] read as a [ByteSource].
pub(crate) struct SinkSource<'a, S>(pub(crate) &'a S);

impl<S: TapeSink> ByteSource for SinkSource<'_, S> {
    type Error = S::Error;

    fn size(&self) -> Result<u64, S::Error> {
        self.0.size()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), S::Error> {
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

/// A step of the walk of the bytes kept: a good block, or bytes no block explains (those next to each other as one).
struct Walk {
    at: u64,
    len: u64,
    good: bool,
}

impl Walk {
    fn end(&self) -> u64 {
        self.at.saturating_add(self.len)
    }
}

impl Step {
    /// The junk from `at` to `end` (empty when `end` is not past `at`).
    fn junk(at: u64, end: u64) -> Step {
        Step {
            at,
            len: end.saturating_sub(at),
            what: What::Junk,
        }
    }

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
    fn step<S: TapeSink>(&self, src: &S, at: u64) -> Result<Step, WriteError<S::Error>> {
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
                .map_err(sink_err("reading back a block header"))?;
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

/// The offset of a good block of one of `sessions` in the bytes of `src` from `from` to `size`, which a resume would
/// cut off: more than a torn end. It looks [RESUME_LOOK_MAX] bytes far and reads at most [RESUME_PROBES_MAX] places
/// whole, so crafted bytes cost a bounded read; where either runs out before the end of the bytes it can't tell they
/// hold no good block: [ResumeError::LookSpent]. With no session known (no block kept, a head the reader didn't
/// take), a good block of any session is the piece's.
fn good_block_after<S: TapeSink>(
    src: &S,
    from: u64,
    size: u64,
    sessions: &alloc::collections::BTreeSet<u32>,
) -> Result<Option<u64>, WriteError<S::Error>> {
    // the places looked at: a header starting before `end` is read whole, up to `size`
    let end = size.min(from.saturating_add(RESUME_LOOK_MAX));
    let mut probes = 0;
    let mut window = vec![0u8; JUNK_READ_MAX];
    let mut at = from;
    while at < end {
        // at most JUNK_READ_MAX, as min says
        let n = usize::try_from(
            size.min(end.saturating_add(wide(HEADER_LEN - 1)))
                .saturating_sub(at)
                .min(wide(JUNK_READ_MAX)),
        )
        .unwrap_or(JUNK_READ_MAX);
        let w = window.get_mut(..n).unwrap_or_default();
        src.read_at(at, w)
            .map_err(sink_err("reading back the bytes a resume cuts off"))?;
        // the places a whole header fits in this window, before `end`; the next window starts at the first that
        // doesn't
        let fits = n
            .saturating_sub(HEADER_LEN - 1)
            .min(usize::try_from(end.saturating_sub(at)).unwrap_or(usize::MAX));
        for i in 0..fits {
            let Some(header) = w.get(i..i.saturating_add(HEADER_LEN)) else {
                break;
            };
            if header.get(..4) != Some(&crate::MAGIC[..]) {
                continue;
            }
            let Some(session) = crate::bytes::le_u32(header, 24) else {
                continue;
            };
            let Ok(len) = block_len(header) else { continue };
            let place = at.saturating_add(wide(i));
            let ours = sessions.is_empty() || sessions.contains(&session);
            if !ours || place.saturating_add(wide(len)) > size {
                continue;
            }
            if probes >= RESUME_PROBES_MAX {
                return Err(WriteError::Resume(ResumeError::LookSpent { offset: place }));
            }
            probes = probes.saturating_add(1);
            let mut block = vec![0u8; len];
            src.read_at(place, &mut block)
                .map_err(sink_err("reading back a block a resume cuts off"))?;
            if Block::read(&block).is_ok() {
                return Ok(Some(place));
            }
        }
        at = at.saturating_add(wide(fits.max(1)));
    }
    if end < size {
        return Err(WriteError::Resume(ResumeError::LookSpent { offset: end }));
    }
    Ok(None)
}

/// Where the bytes of `src` from `from` to `size`, which a resume would cut, hold damage rather than a torn end; None
/// when they are a torn end. A torn end is what a crash leaves of an append: whole blocks, then a record the bytes
/// stop inside of (fewer bytes than a header, a header naming more bytes than are left), or blank bytes (zeros) up to
/// the end, which a file holds where it grew but its data wasn't written. Damage is a whole record that fails its
/// check, bytes that don't start a record, or a whole tail: one that didn't end its close is not what a writer left.
/// Each step moves on at least one byte, and reads at most [MAX_BLOCK_LEN] or [JUNK_READ_MAX] bytes at once.
fn torn_end<S: TapeSink>(
    src: &S,
    from: u64,
    size: u64,
) -> Result<Option<u64>, WriteError<S::Error>> {
    let blank = blank_from(src, from, size)?;
    let mut at = from;
    while at < blank && size.saturating_sub(at) >= wide(HEADER_LEN) {
        let mut header = [0u8; HEADER_LEN];
        src.read_at(at, &mut header)
            .map_err(sink_err("reading back a header a resume cuts off"))?;
        let Ok(len) = block_len(&header) else {
            // a header that runs into the blank end was being written
            return Ok((at.saturating_add(wide(HEADER_LEN)) <= blank).then_some(at));
        };
        let end = at.saturating_add(wide(len));
        if end > size {
            return Ok(rotted_length(src, at, size, &header, len)?.then_some(at));
        }
        let mut block = vec![0u8; len];
        src.read_at(at, &mut block)
            .map_err(sink_err("reading back a block a resume cuts off"))?;
        match Block::read(&block) {
            Ok(b) if b.kind == BlockKind::Tail => return Ok(Some(at)),
            Ok(_) => at = end,
            // a record that runs into the blank end was being written
            Err(_) => return Ok((end <= blank).then_some(at)),
        }
    }
    Ok(None)
}

/// Whether the record at `at` in `src`, whose `header` names `len` bytes, more than are left before `size`, is a whole
/// record whose length rotted rather than a torn one: its kind has a fixed length other than `len`, a block start lies
/// in its bytes, or its bytes up to `size` read as a good block once its header names their length. A torn record is
/// a prefix of one block and holds none of these.
fn rotted_length<S: TapeSink>(
    src: &S,
    at: u64,
    size: u64,
    header: &[u8; HEADER_LEN],
    len: usize,
) -> Result<bool, WriteError<S::Error>> {
    let fixed = match BlockKind::from_u8(header[5]) {
        BlockKind::Tail => Some(TAIL_LEN),
        BlockKind::AppendHead => Some(crate::piece::APPEND_HEAD_LEN),
        _ => None,
    };
    if fixed.is_some_and(|f| f != len) {
        return Ok(true);
    }
    // fewer bytes are left than `len`, at most MAX_BLOCK_LEN
    let left = usize::try_from(size.saturating_sub(at)).map_err(|_overflow| too_long(size))?;
    let mut bytes = vec![0u8; left];
    src.read_at(at, &mut bytes)
        .map_err(sink_err("reading back a torn record"))?;
    if bytes
        .get(1..)
        .is_some_and(|rest| rest.windows(crate::MAGIC.len()).any(|w| w == crate::MAGIC))
    {
        return Ok(true);
    }
    let Ok(stated) = u32::try_from(left) else {
        return Ok(false);
    };
    match bytes.get_mut(8..12) {
        Some(field) => field.copy_from_slice(&stated.to_le_bytes()),
        None => return Ok(false),
    }
    Ok(Block::read(&bytes).is_ok())
}

/// Where the blank (zero) bytes that run to `size` start in `src`, not before `from`.
fn blank_from<S: TapeSink>(src: &S, from: u64, size: u64) -> Result<u64, WriteError<S::Error>> {
    let mut window = vec![0u8; JUNK_READ_MAX];
    let mut end = size;
    while end > from {
        // at most JUNK_READ_MAX, as min says
        let n = usize::try_from(end.saturating_sub(from).min(wide(JUNK_READ_MAX)))
            .unwrap_or(JUNK_READ_MAX);
        let start = end.saturating_sub(wide(n));
        let w = window.get_mut(..n).unwrap_or_default();
        src.read_at(start, w)
            .map_err(sink_err("reading back the end a resume cuts off"))?;
        if let Some(last) = w.iter().rposition(|b| *b != 0) {
            return Ok(start.saturating_add(wide(last)).saturating_add(1));
        }
        end = start;
    }
    Ok(from)
}

/// The offset of a block the reader took from `from` up to its end, after a block that ends the piece, which a resume
/// would cut off: a block of one of `sessions` (any with none known) that neither ends a piece itself nor is a put
/// belonging with a block after it ([crate::FLAG_WITH_NEXT]).
fn taken_after_end<S: TapeSink>(
    src: &S,
    layout: &Layout,
    from: u64,
    sessions: &alloc::collections::BTreeSet<u32>,
) -> Result<Option<u64>, WriteError<S::Error>> {
    let mut at = from;
    while at < layout.end {
        let step = layout.step(src, at)?;
        if let What::Block { flags, .. } = step.what
            && flags & crate::FLAG_WITH_NEXT == 0
            && !ends_piece(src, &step)?
        {
            let mut tag = [0u8; 4];
            src.read_at(step.at.saturating_add(24), &mut tag)
                .map_err(sink_err("reading back a block's session"))?;
            if sessions.is_empty() || sessions.contains(&u32::from_le_bytes(tag)) {
                return Ok(Some(step.at));
            }
        }
        at = step.end();
    }
    Ok(None)
}

/// Whether step `s` of the piece in `src` ends the piece: a close, a tail or a final index block.
fn ends_piece<S: TapeSink>(src: &S, s: &Step) -> Result<bool, WriteError<S::Error>> {
    let What::Block { kind, .. } = s.what else {
        return Ok(false);
    };
    match kind {
        BlockKind::Close | BlockKind::Tail => Ok(true),
        BlockKind::Index => {
            // a step is at most MAX_BLOCK_LEN long
            let len = usize::try_from(s.len).map_err(|_overflow| too_long(s.len))?;
            let mut buf = vec![0u8; len];
            src.read_at(s.at, &mut buf)
                .map_err(sink_err("reading back an index block"))?;
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
    fn take<S: TapeSink>(&mut self, src: &S, step: &Step) -> Result<(), WriteError<S::Error>> {
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
            .map_err(sink_err("reading back a block"))?;
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
    /// Reopen the unclosed piece in `sink` (cut off by a crash, or cut in its end) to go on writing it, see the
    /// [module docs](self); a closed piece is written on after its last close. `options` are the ones it was written
    /// with. The piece is truncated to the end of its last counted block, and the writer acts after that block as it
    /// did; the caller then gives it the blocks from [Self::next_id] on, with the puts between them. Definitions in a
    /// damaged block are lost to the writer unless a later block holds them. The producer seq of
    /// [Self::adopt_block] starts again: the first block adopted shows no gap.
    pub fn resume_on(
        mut sink: S,
        options: WriterOptions,
    ) -> Result<(Self, Resumed), WriteError<S::Error>> {
        let size = sink
            .size()
            .map_err(sink_err("reading the length to resume"))?;
        let (layout, cut) = read_layout(&sink)?;
        let mut kept = KeptEnd::default();
        let mut sessions = alloc::collections::BTreeSet::new();
        // close records kept, by offset: the content hash chains over each from there on
        let mut closes: BTreeMap<u64, (u64, Close)> = BTreeMap::new();
        // the steps that end a write session (final index, close record, tail) since the last other step
        let mut ending: Vec<Step> = Vec::new();
        let mut at = 0;
        let mut ended_at = None;
        while at < layout.end {
            let step = layout.step(&sink, at)?;
            at = step.end();
            // an end whose close and tail read whole goes on with an append head naming them; anything else after it
            // is what a crash left there, ended below as the bytes are
            if let Some((c, tail)) = whole_close(&sink, &ending)? {
                if !appends_to(&sink, &step, c.0, tail, &c.1)? {
                    let none = alloc::collections::BTreeSet::new();
                    let end = tail.saturating_add(wide(TAIL_LEN));
                    if let Some(offset) = good_block_after(&sink, end, size, &none)? {
                        return Err(WriteError::Resume(ResumeError::GoodBlockAfterCut {
                            offset,
                        }));
                    }
                    break;
                }
                ending.drain(..).for_each(|e| kept.take(&e));
                closes.insert(c.0, (tail, c.1));
            } else if ends_piece(&sink, &step)? {
                ending.push(step);
                continue;
            } else if let Some(first) = ending.first() {
                // an end that isn't whole with a block after it: a block after the end of the piece
                ended_at = Some(first.end());
                break;
            }
            if let What::Block { .. } = step.what {
                let mut tag = [0u8; 4];
                sink.read_at(step.at.saturating_add(24), &mut tag)
                    .map_err(sink_err("reading back a block's session"))?;
                sessions.insert(u32::from_le_bytes(tag));
            }
            kept.take(&step);
        }
        // an end the bytes stop at, or only junk follows: kept when its close and tail are whole, the writer then
        // writes on after them; else dropped, the piece open again
        let mut write_on = false;
        if ended_at.is_none() && !ending.is_empty() {
            match whole_close(&sink, &ending)? {
                Some(((offset, close), tail)) => {
                    if let Some(next) = close.next {
                        // bytes after the tail: junk a crash left is cut off, a good block refuses
                        let end = tail.saturating_add(wide(TAIL_LEN));
                        if size > end {
                            let none = alloc::collections::BTreeSet::new();
                            if let Some(offset) = good_block_after(&sink, end, size, &none)? {
                                return Err(WriteError::Resume(ResumeError::GoodBlockAfterCut {
                                    offset,
                                }));
                            }
                            if let Some(offset) = torn_end(&sink, end, size)? {
                                return Err(WriteError::Resume(ResumeError::Damaged { offset }));
                            }
                            sink.truncate(end)
                                .map_err(sink_err("cutting the bytes after the tail"))?;
                        }
                        sink.close()
                            .map_err(sink_err("giving the closed piece its name"))?;
                        return Err(WriteError::Resume(ResumeError::Continued { next }));
                    }
                    ending.drain(..).for_each(|e| kept.take(&e));
                    closes.insert(offset, (tail, close));
                    write_on = true;
                }
                None => ended_at = ending.first().map(Step::end),
            }
        }
        // the reason of a close record dropped with the end it belongs to: the piece was being closed
        let ended = match write_on {
            true => None,
            false => close_in(&sink, &ending)?.map(|(_, c)| c.reason),
        };
        if let Some(from) = ended_at
            && let Some(offset) = taken_after_end(&sink, &layout, from, &sessions)?
        {
            return Err(WriteError::Resume(ResumeError::GoodBlockAfterCut {
                offset,
            }));
        }
        if let Some(offset) = good_block_after(&sink, layout.end, size, &sessions)? {
            return Err(WriteError::Resume(ResumeError::GoodBlockAfterCut {
                offset,
            }));
        }
        let kept_bytes = kept.last;
        if let Some(offset) = torn_end(&sink, kept_bytes, size)? {
            return Err(WriteError::Resume(ResumeError::Damaged { offset }));
        }
        let mut w = Self::create_on(sink, options.sync);
        w.apply(&options)?;
        // the content hash of the bytes kept and the steps of its walk, as a reader's verify takes them: good
        // blocks, and bytes no block explains
        let mut hash = blake3::Hasher::new();
        let mut walked: Vec<Walk> = Vec::new();
        let mut pos = 0u64;
        let tries = walk_piece(
            kept_bytes,
            |at, buf| w.sink.read_at(at, buf),
            |s| {
                if let Some((_, c)) = closes.get(&pos) {
                    hash = crate::piece::chained(&c.hash);
                }
                let (len, good) = match s {
                    Walked::Block(b) => {
                        hash_block(&mut hash, b);
                        (wide(b.len()), true)
                    }
                    Walked::Raw(r) => {
                        hash.update(r);
                        (wide(r.len()), false)
                    }
                };
                match walked.last_mut() {
                    Some(last) if !good && !last.good => last.len = last.len.saturating_add(len),
                    _ => walked.push(Walk { at: pos, len, good }),
                }
                pos = pos.saturating_add(len);
            },
        )
        .map_err(sink_err("reading back the piece to hash"))?;
        if !tries {
            return Err(WriteError::Resume(ResumeError::HashWalkSpent {
                end: kept_bytes,
            }));
        }
        let mut buf = Vec::new();
        let mut named = Named::default();
        let mut past_head = false;
        for (i, s) in walked.iter().enumerate() {
            if s.good {
                let len = usize::try_from(s.len).map_err(|_overflow| too_long(s.len))?;
                buf.resize(len, 0);
                w.sink
                    .read_at(s.at, &mut buf)
                    .map_err(sink_err("reading back a block"))?;
                let kind = BlockKind::from_u8(buf.get(5).copied().unwrap_or(0));
                if let Some((tail, c)) = closes.get(&s.at) {
                    w.closed_by(s.at, *tail, c);
                }
                w.replay_good(s.at, &buf)?;
                let step = Step {
                    at: s.at,
                    len: s.len,
                    what: What::Block {
                        kind,
                        flags: 0,
                        entry: None,
                    },
                };
                named.take(&w.sink, &step)?;
                past_head |= kind != BlockKind::PieceHead;
                continue;
            }
            past_head = true;
            // the damaged blocks the index lists in these bytes stand by their entries, each ending before the next
            // good block and within the bytes kept; other bytes are junk
            let next_good = walked
                .get(i.saturating_add(1)..)
                .and_then(|rest| rest.iter().find(|n| n.good))
                .map_or(kept_bytes, |n| n.at);
            let mut done = s.at;
            for (&x, entries) in layout.entries.range(s.at..s.end()) {
                let Some(e) = entries.first().filter(|_| x >= done) else {
                    continue;
                };
                let Some(len) = FromFile::new(e.len)
                    .at_most(MAX_BLOCK_LEN)
                    .ok()
                    .filter(|len| *len >= HEADER_LEN + CRC_LEN)
                else {
                    continue;
                };
                let end = x.saturating_add(wide(len));
                if end > next_good.min(kept_bytes) {
                    continue;
                }
                w.replay_junk(&Step::junk(done, x), &mut buf)?;
                buf.resize(len, 0);
                w.sink
                    .read_at(x, &mut buf)
                    .map_err(sink_err("reading back a damaged block"))?;
                w.replay_damaged(entries, *e, e.kind, &buf)?;
                done = end;
            }
            if done < s.end() {
                w.replay_junk(&Step::junk(done, s.end()), &mut buf)?;
            }
        }
        if let Some(described) = named.layout
            && described != options.layout
        {
            return Err(WriteError::Input(InputError::Layout {
                described,
                given: options.layout,
            }));
        }
        if let Some(described) = named.producer
            && described != options.producer
        {
            return Err(WriteError::Input(InputError::Producer {
                described,
                given: options.producer,
            }));
        }
        if size > kept_bytes {
            w.sink
                .truncate(kept_bytes)
                .map_err(sink_err("truncating the piece to resume"))?;
        }
        w.hash = hash;
        let entries = w.indexer.entries().to_vec();
        let after_close = closes.last_key_value().map(|(_, (_, c))| *c);
        // the layout and the producer follow the head first: a block past it means they were written
        w.head_put = past_head;
        if write_on {
            w.write_on()?;
        } else if kept.counted {
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
            after_close,
        };
        Ok((w, resumed))
    }

    fn replay_good(&mut self, at: u64, block: &[u8]) -> Result<(), WriteError<S::Error>> {
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
                self.account(block, header, *crc).map(drop)
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
                self.account(block, header, *crc).map(drop)
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
    ) -> Result<(), WriteError<S::Error>> {
        let end = self.end_after(bytes.len())?;
        self.hash.update(bytes);
        self.indexer.placed_entries(entries, bytes.len());
        // its range relation's values go up to its range's top, as its writer saw them; its other values are unread
        for e in entries
            .iter()
            .filter(|e| e.kind == BlockKind::Events && !e.repeat)
        {
            if let Some(key) = self.indexer.key_of(e.track_number, Some(e.track_id)) {
                self.relations
                    .seed_damaged(key, e.range.map(|(_, hi)| hi), e.relation_break);
            }
        }
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
            // into its track's hash by the unit hash the writer stated in its entry, as when it was placed; without
            // one the track's hash leaves it out and the close says so
            if !e.repeat {
                self.indexer.begin_block();
                match e.unit == crate::piece::UnitHash::NONE {
                    true => self.indexer.tally_unread(e.track_number, e.track_id),
                    false => self
                        .indexer
                        .tally_chunk(e.track_number, e.track_id, &e.unit),
                }
            }
        }
        // the records of a damaged block of data: its entry names no track, so each track that may hold records
        // leaves them out of its hash
        if kind == BlockKind::Records && !e.repeat && e.track_id != STATE_TRACK_ID {
            self.indexer.begin_block();
            let relations = &self.relations;
            self.indexer
                .tally_unread_records(|n| relations.takes_records(n));
        }
        // a state record in it: its slot's newest record is here, its item unknown
        for s in entries.iter().filter_map(IndexEntry::state) {
            self.near.wrote(s.slot, None, e.offset, &self.near_policy);
        }
        Ok(())
    }

    /// Bytes no block or entry explains: hashed, placed, never listed.
    fn replay_junk(&mut self, step: &Step, buf: &mut Vec<u8>) -> Result<(), WriteError<S::Error>> {
        let mut at = step.at;
        let end = step.end();
        while at < end {
            let n = usize::try_from(end.saturating_sub(at))
                .map_or(JUNK_READ_MAX, |n| n.min(JUNK_READ_MAX));
            buf.resize(n, 0);
            self.sink
                .read_at(at, buf)
                .map_err(sink_err("reading back damaged bytes"))?;
            self.hash.update(buf);
            self.indexer.placed_unlisted(n);
            self.bytes = self.end_after(n)?;
            // n is at most end - at
            at = at.saturating_add(wide(n));
        }
        Ok(())
    }
}

/// The piece at the start of `sink` as a reader finds it: its blocks by index entry and gap, where it ends, and where
/// a scan was cut.
fn read_layout<S: TapeSink>(sink: &S) -> Result<(Layout, Option<u64>), WriteError<S::Error>> {
    let reader = TapeReader::open(SinkSource(sink)).map_err(WriteError::Read)?;
    let Some(p) = reader.pieces().first() else {
        return Ok((
            Layout {
                entries: BTreeMap::new(),
                gaps: BTreeMap::new(),
                end: 0,
            },
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
    Ok((Layout { entries, gaps, end }, p.cut))
}

/// The block of step `s` in `src`, read whole: None for a step of bytes no block explains.
fn block_of<S: TapeSink>(src: &S, s: &Step) -> Result<Option<Vec<u8>>, WriteError<S::Error>> {
    let What::Block { .. } = s.what else {
        return Ok(None);
    };
    // a block step is at most MAX_BLOCK_LEN long
    let len = usize::try_from(s.len).map_err(|_overflow| too_long(s.len))?;
    let mut buf = vec![0u8; len];
    src.read_at(s.at, &mut buf)
        .map_err(sink_err("reading back a block"))?;
    Ok(Some(buf))
}

/// The close record among the steps `ending` that end a write session, read: its offset and what it says.
fn close_in<S: TapeSink>(
    src: &S,
    ending: &[Step],
) -> Result<Option<(u64, Close)>, WriteError<S::Error>> {
    for s in ending {
        if let Some(buf) = block_of(src, s)?
            && let Ok(b) = Block::read(&buf)
            && let Ok(c) = CloseBlock::read(&b)
        {
            return Ok(Some((s.at, c.close())));
        }
    }
    Ok(None)
}

/// A close record's offset and what it says, and its tail's offset.
type WholeClose = ((u64, Close), u64);

/// The close record and tail the steps `ending` end with, when both read whole and the tail names that close and a
/// piece starting the bytes: the close's offset and what it says, and the tail's offset.
fn whole_close<S: TapeSink>(
    src: &S,
    ending: &[Step],
) -> Result<Option<WholeClose>, WriteError<S::Error>> {
    let [.., close_step, tail_step] = ending else {
        return Ok(None);
    };
    let (Some(cb), Some(tb)) = (block_of(src, close_step)?, block_of(src, tail_step)?) else {
        return Ok(None);
    };
    let (Ok(c), Ok(tail)) = (Block::read(&cb), Tail::read(&tb)) else {
        return Ok(None);
    };
    let Ok(close) = CloseBlock::read(&c).map(|c| c.close()) else {
        return Ok(None);
    };
    let whole = tail.ends(&close, close_step.at)
        && u64::from(tail.close_len) == close_step.len
        && tail_step.at == close_step.end();
    Ok(whole.then_some(((close_step.at, close), tail_step.at)))
}

/// Whether step `s` of `src` is an append head going on from the close record `close` at `close_at` and its tail at
/// `tail_at`.
fn appends_to<S: TapeSink>(
    src: &S,
    s: &Step,
    close_at: u64,
    tail_at: u64,
    close: &Close,
) -> Result<bool, WriteError<S::Error>> {
    if close.next.is_some() || s.at != tail_at.saturating_add(wide(TAIL_LEN)) {
        return Ok(false);
    }
    let Some(buf) = block_of(src, s)? else {
        return Ok(false);
    };
    Ok(Block::read(&buf)
        .ok()
        .and_then(|b| AppendHead::read(&b).ok())
        .is_some_and(|h| h.follows(close_at, tail_at, &close.hash)))
}

fn too_long<E>(len: u64) -> WriteError<E> {
    WriteError::Block(crate::BlockError::BadLength(
        u32::try_from(len).unwrap_or(u32::MAX),
    ))
}
