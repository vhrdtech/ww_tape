//! Blocks in a file or any byte stream: appended back to back, read one after the other.

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Read};
use std::ops::Deref;

use crate::piece::PieceHeadBlock;
use crate::reader::{OwnedBlock, report_len, wide};
use crate::walk::{Head, SessionState, Verdict, is_free};
use crate::{
    Block, BlockError, BlockKind, GAP_CHECKS_MAX, HEADER_LEN, MAGIC, MAX_BLOCK_LEN, Medium,
    SessionTag, block_len, is_blank,
};

/// Bytes a [FileBlocks] asks its reader for at once, at least: the buffer grows by at most this much ahead of the
/// bytes that came, so a damaged length never allocates what it says.
const READ_STEP: usize = 64 * 1024;

/// Bytes a [Recovery::Resync] walk reads ahead of where it is, at most (128 MiB).
///
/// That is a stretch as long as the longest gap a layout allows ([GAP_MAX_MAX](crate::GAP_MAX_MAX)) and the block that
/// ends it, read before the walk passes over the stretch's own bytes.
///
/// A [Recovery::Prefix] walk holds one block.
pub const RESYNC_AHEAD_MAX: usize = 128 << 20;
const _: () = assert!(
    crate::GAP_MAX_MAX == 64 << 20,
    "RESYNC_AHEAD_MAX holds the longest gap"
);
const _: () = assert!(
    RESYNC_AHEAD_MAX - MAX_BLOCK_LEN == 64 << 20,
    "RESYNC_AHEAD_MAX holds the longest gap and a block"
);

/// Bytes read and not yet passed over, `bytes[pos..]`. Passing over moves `pos`; the bytes before it are dropped when
/// the buffer next grows, so passing over a few bytes at a time never moves the rest.
#[derive(Debug, Default)]
struct Window {
    bytes: Vec<u8>,
    pos: usize,
}

impl Deref for Window {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.bytes.get(self.pos..).unwrap_or_default()
    }
}

impl Window {
    /// Pass over the first `n` bytes (at most all of them).
    fn pass(&mut self, n: usize) {
        self.pos = self.pos.saturating_add(n).min(self.bytes.len());
        if self.pos == self.bytes.len() {
            self.bytes.clear();
            self.pos = 0;
        }
    }

    /// Grow to `want` bytes, the new ones zero, dropping those passed over first; the new bytes.
    fn grow(&mut self, want: usize) -> &mut [u8] {
        let have = self.len();
        self.bytes.drain(..self.pos);
        self.pos = 0;
        self.bytes.resize(want.max(have), 0);
        self.bytes.get_mut(have..).unwrap_or_default()
    }

    /// Keep the first `len` bytes (at most all of them).
    fn truncate(&mut self, len: usize) {
        self.bytes.truncate(self.pos.saturating_add(len));
    }
}

/// A checked block's header and length, kept apart from its bytes: the bytes are lent again without a second check.
#[derive(Copy, Clone, Debug)]
struct Checked {
    /// The header fields, with no bytes
    head: Block<'static>,
    len: usize,
}

impl Checked {
    fn of(b: &Block<'_>) -> Self {
        Self {
            head: b.detach(),
            len: b.len(),
        }
    }

    /// The block on the bytes it was checked on, at the start of `buf`; None when `buf` is shorter.
    fn on(self, buf: &[u8]) -> Option<Block<'_>> {
        self.head.on(buf.get(..self.len)?)
    }
}

/// An owned block, e.g. one handed over by a producer, checked once when made.
#[derive(Clone, Debug)]
pub struct BlockBuf(OwnedBlock);

impl PartialEq for BlockBuf {
    fn eq(&self, other: &Self) -> bool {
        self.bytes() == other.bytes()
    }
}

impl Eq for BlockBuf {}

impl BlockBuf {
    /// Check `bytes` as one whole block.
    pub fn new(bytes: Vec<u8>) -> Result<Self, BlockError> {
        OwnedBlock::read(bytes).map(BlockBuf)
    }

    pub fn block(&self) -> Block<'_> {
        self.0.block()
    }

    pub fn bytes(&self) -> &[u8] {
        self.0.bytes()
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.0.into_bytes()
    }
}

/// Data blocks a [Recovery::Resync] walk remembers by session and seq, at most (65536, about 4 MiB).
///
/// It remembers the last ones it handed out, so a later good block of one of their ids with other bytes is a
/// [FileError::Conflict], however far apart the two are.
///
/// A block whose id has left the window is taken without the check.
pub const RESYNC_SEQS_MAX: usize = 1 << 16;

/// The data blocks a [Recovery::Resync] walk handed out last, at most [RESYNC_SEQS_MAX]: where each starts, its length
/// and CRC, by its session and seq.
#[derive(Default)]
struct Seen {
    ids: VecDeque<(SessionTag, u64)>,
    blocks: BTreeMap<(SessionTag, u64), (u64, usize, u32)>,
}

/// What [Seen] knows of a block's id.
enum Met {
    /// No block of its id in the window
    New,
    /// A block of its id with the same length and CRC: a copy, handed out again (where a Prefix walk may read it)
    Same,
    /// A block of its id with other bytes, at this offset
    Other(u64),
}

impl Seen {
    /// What the window holds of the block of `id`, `len` bytes, CRC `crc`.
    fn met(&self, id: (SessionTag, u64), len: usize, crc: u32) -> Met {
        match self.blocks.get(&id) {
            None => Met::New,
            Some((_, l, c)) if (*l, *c) == (len, crc) => Met::Same,
            Some((at, _, _)) => Met::Other(*at),
        }
    }

    /// Remember the block of `id` handed out at `at`, the oldest one dropped past [RESYNC_SEQS_MAX]; of copies, the
    /// first.
    fn take(&mut self, id: (SessionTag, u64), at: u64, len: usize, crc: u32) {
        if self.blocks.contains_key(&id) {
            return;
        }
        self.blocks.insert(id, (at, len, crc));
        self.ids.push_back(id);
        if self.ids.len() > RESYNC_SEQS_MAX
            && let Some(old) = self.ids.pop_front()
        {
            self.blocks.remove(&old);
        }
    }
}

/// Why a file could not be read further, or what was skipped in it.
#[derive(Debug)]
#[non_exhaustive]
pub enum FileError {
    Io(std::io::Error),
    /// [Recovery::Prefix]: the file is read up to `offset`, the bytes from there on are left out, for `why`. Comes
    /// once, the walk ends after it.
    Cut {
        offset: u64,
        why: Cut,
    },
    /// [Recovery::Resync]: bytes at `offset` are not a good block (`error`); `skipped` bytes were passed over to the
    /// next block magic.
    Bad {
        offset: u64,
        error: BlockError,
        skipped: u64,
    },
    /// [Recovery::Resync]: the file ends inside a block (a writer stopped mid-way): `len` bytes from `offset` on are
    /// left out.
    Truncated {
        offset: u64,
        len: u64,
    },
    /// A blank or bad stretch of `len` bytes at `offset` that the session's data goes on after, within the layout's
    /// [gap](crate::Layout::gap_max): a writer could not go on there (a failed program, a bad block, a torn unit,
    /// page padding). The walk goes on after it.
    Gap {
        offset: u64,
        len: u64,
    },
    /// [Recovery::Prefix]: a good block at `offset` with the session and seq of the block before it but other bytes:
    /// the later copy wins (a writer that retried a write), it comes next. (A copy with the same bytes is dropped
    /// silently.)
    Replaced {
        offset: u64,
        session: SessionTag,
        seq: u64,
    },
    /// [Recovery::Resync]: two good blocks of session `session` and seq `seq` with other bytes, the one handed out at
    /// `first` and `block`, met at `offset`, however far apart (the last [RESYNC_SEQS_MAX] data blocks handed out are
    /// remembered). In damage a salvage can't tell a copy from a rewrite, so neither is taken for the other: `block`
    /// comes only here, never as a block of its own. The block at `first` was handed out already, as the walk streams:
    /// a caller that keeps what a salvage hands out holds it back or can take it back, and marks both. The walk goes on
    /// after `block`. Blocks of puts share seqs by design ([FLAG_REPEAT](crate::FLAG_REPEAT)) and are never a
    /// conflict.
    Conflict {
        first: u64,
        offset: u64,
        session: SessionTag,
        seq: u64,
        block: Vec<u8>,
    },
    /// [Recovery::Resync]: a good block of session `session` at `offset`, `len` bytes, where no session may start: stale data
    /// (an earlier life of a medium), never data. Skipped by its length.
    Stale {
        offset: u64,
        session: SessionTag,
        len: u64,
    },
    /// A block at `offset`, `len` bytes by its head-CRC-checked length, of an integrity level this build doesn't
    /// handle (`error`: [BlockError::PayloadUnchecked] for a chunk, [BlockError::UnhandledIntegrity] for a piece
    /// head): not damage. Both recoveries step over it and go on.
    Refused {
        offset: u64,
        error: BlockError,
        len: u64,
    },
}

/// Why a [Recovery::Prefix] walk stopped before the end of the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cut {
    /// The bytes there are not a good block (bad magic, length or CRC), and no block of the session follows within the
    /// layout's gap.
    Bad(BlockError),
    /// The file ends inside a block (a writer stopped mid-way), `len` bytes of it are there.
    Truncated { len: u64 },
    /// A data block's seq is not after the one before it in its session: not this file's next block. (Describe, index,
    /// close, tail and piece head blocks take no part in the order.)
    SeqBackwards { seq: u64, after: u64 },
    /// A good block of session `session` where no session may start ([Medium]): stale data behind the end of data.
    Stale { session: SessionTag },
}

impl core::fmt::Display for FileError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Cut { offset, why } => {
                write!(f, "read up to byte {offset}, the rest is left out: ")?;
                match why {
                    Cut::Bad(e) => write!(f, "not a good block ({e:?})"),
                    Cut::Truncated { len } => {
                        write!(f, "the file ends inside a block ({len} bytes of it)")
                    }
                    Cut::SeqBackwards { seq, after } => {
                        write!(f, "block seq {seq} after {after}")
                    }
                    Cut::Stale { session } => {
                        write!(f, "a block of another session ({:08x})", session.get())
                    }
                }
            }
            Self::Bad {
                offset,
                error,
                skipped,
            } => write!(
                f,
                "bad block at byte {offset} ({error:?}), skipped {skipped} bytes"
            ),
            Self::Truncated { offset, len } => {
                write!(f, "file ends inside a block: {len} bytes at byte {offset}")
            }
            Self::Gap { offset, len } => {
                write!(
                    f,
                    "a gap of {len} bytes at byte {offset}, the data goes on after it"
                )
            }
            Self::Replaced {
                offset,
                session,
                seq,
            } => write!(
                f,
                "block {seq} of session {:08x} again at byte {offset} with other bytes: the later copy is taken",
                session.get()
            ),
            Self::Conflict {
                first,
                offset,
                session,
                seq,
                block,
            } => write!(
                f,
                "block {seq} of session {:08x} at bytes {first} and {offset} ({} bytes) with other bytes: a conflict",
                session.get(),
                block.len()
            ),
            Self::Stale {
                offset,
                session,
                len,
            } => write!(
                f,
                "a stale block of another session ({:08x}) at byte {offset}, {len} bytes skipped",
                session.get()
            ),
            Self::Refused { offset, error, len } => write!(
                f,
                "a block of an integrity level this reader doesn't handle at byte {offset} ({error:?}), {len} bytes skipped"
            ),
        }
    }
}

impl core::error::Error for FileError {}

/// What a reader does at damage.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Recovery {
    /// The longest valid prefix: stop at the first block with a bad magic, length or CRC, a cut one, one of a session
    /// that may not start there, or a seq that goes backwards, and report where ([FileError::Cut]). What a crash
    /// leaves (a cut last block, blank preallocated space) reads as the file up to there. Where the layout allows
    /// gaps ([crate::Layout::gap_max]), a blank or bad stretch the session's data goes on after is a [FileError::Gap],
    /// not the end. A block of an integrity level this build doesn't handle is no damage: both recoveries report it
    /// ([FileError::Refused]) and step over it by its length. The default.
    #[default]
    Prefix,
    /// Salvage: report a bad block and read on at the next block magic one byte after its start ([FileError::Bad]);
    /// no seq check, but a data block with the session and seq of one handed out before (of the last
    /// [RESYNC_SEQS_MAX]) and other bytes is a conflict ([FileError::Conflict]): it comes only in the report, which
    /// names both (a copy with the same bytes is no conflict). The first block of a conflicting pair was
    /// handed out before the second is met: a caller that keeps what it takes holds back or can take back what it took
    /// since the last damage. A block of a session that may not start there is reported ([FileError::Stale]) and
    /// skipped. A
    /// chunk block whose head is good (head CRC) but whose payload isn't is handed out instead, with
    /// [Block::is_intact] false and its bad segments marked ([crate::Chunk::bad_segments]), and the walk goes on
    /// after it by its length. Any byte may start a block, also in a file aligned for its chunk payloads: a block is
    /// taken only when its CRCs match. In a stretch [Recovery::Prefix] reads past as a [FileError::Gap], the good
    /// blocks are handed out as they are met (a session starting in the stretch, with the blocks after it, too), and
    /// the walk goes on with the session it had where Prefix does: every block Prefix reads, Resync reads. Looking
    /// for where Prefix goes on reads up to [RESYNC_AHEAD_MAX] ahead, once per stretch, and not past where Prefix ends.
    Resync,
}

/// Reads blocks one after the other from a byte stream into one reused buffer, see [Recovery] for what happens at
/// damage and [Medium] for where a new session may start.
///
/// Blank bytes ([is_blank]: 0x00 or 0xFF) where the next block would start, up to the end, are the end (preallocated
/// space, an erased or trimmed medium); on a raw medium so are blank bytes longer than the layout's
/// [gap](crate::Layout::gap_max), and nothing after them is read.
pub struct FileBlocks<R> {
    rd: R,
    recovery: Recovery,
    /// The bytes read and not yet passed over, from the medium offset `offset` on
    buf: Window,
    /// The block handed out last, at the start of `buf`
    current: Option<Checked>,
    offset: u64,
    eof: bool,
    /// The session and seq taken so far, the gap rules
    walk: SessionState,
    /// A Prefix walk was cut: nothing more
    done: bool,
    /// The block at the start of `buf` replaces the one taken before with its id (reported already)
    replacing: bool,
    /// Where the last data block taken starts (a block of the seq order, [crate::walk::is_free] not)
    last_data: Option<u64>,
    /// Where the bytes after the last block passed over start: where [Self::point] goes on
    passed_end: u64,
    /// [Recovery::Resync]: the data blocks handed out last, by session and seq
    seen: Seen,
    /// [Recovery::Resync] in a stretch a [Recovery::Prefix] walk reads past as a gap: the medium offset where that walk
    /// goes on. Blocks before it change no session of `walk`, and none is passed over beyond it.
    gap_end: Option<u64>,
    /// [Recovery::Resync] in a gap: the sessions met in it, from `walk` at its start; a block either takes is handed
    /// out
    gap_walk: SessionState,
    /// [Recovery::Resync] past the place a [Recovery::Prefix] walk ends: no more gaps are looked for
    prefix_over: bool,
    /// Probe headers read looking past stretches
    #[cfg(test)]
    probes: usize,
    /// Probes checked whole looking past stretches
    #[cfg(test)]
    checks: usize,
}

/// Where a [Recovery::Prefix] walk of [FileBlocks] goes on: an offset and what the walk knows there (the session, the
/// last seq, the gap rules), see [FileBlocks::point].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct WalkPoint {
    offset: u64,
    walk: SessionState,
}

impl WalkPoint {
    /// The start of the bytes on `medium`, before any block.
    pub fn start(medium: Medium) -> Self {
        Self {
            offset: 0,
            walk: SessionState::new(medium),
        }
    }

    /// Medium offset where the walk goes on.
    pub fn offset(&self) -> u64 {
        self.offset
    }
}

/// What a step of the walk found.
enum Next {
    /// A block to hand out, at the start of `buf`
    Block(Checked),
    /// A copy of the block before it with the same bytes, passed over
    Dropped,
    End,
}

/// The stream is longer than a `u64` offset goes.
fn too_long() -> FileError {
    FileError::Io(io::Error::from(io::ErrorKind::FileTooLarge))
}

impl<R: Read> FileBlocks<R> {
    /// A reader of a file that stops at the first damage ([Recovery::Prefix]).
    pub fn new(rd: R) -> Self {
        Self::with_recovery(rd, Recovery::Prefix)
    }

    pub fn with_recovery(rd: R, recovery: Recovery) -> Self {
        Self {
            rd,
            recovery,
            buf: Window::default(),
            current: None,
            offset: 0,
            eof: false,
            walk: SessionState::new(Medium::File),
            done: false,
            replacing: false,
            last_data: None,
            passed_end: 0,
            seen: Seen::default(),
            gap_end: None,
            gap_walk: SessionState::new(Medium::File),
            prefix_over: false,
            #[cfg(test)]
            probes: 0,
            #[cfg(test)]
            checks: 0,
        }
    }

    /// Read the bytes as kept on `medium`.
    #[must_use]
    pub fn on(mut self, medium: Medium) -> Self {
        self.walk.medium = medium;
        self
    }

    /// The bytes start at `offset` of the medium (a medium layer's hint), so gap probes land on the medium's
    /// [resume](crate::Layout::resume_align) boundaries.
    #[must_use]
    pub fn starting_at(mut self, offset: u64) -> Self {
        self.offset = offset;
        self.passed_end = offset;
        self
    }

    /// Take `layout`'s gap rules from the start, before a layout record says them (a medium layer that knows the
    /// medium); a [crate::Layout] read later replaces them.
    ///
    /// # Panics
    ///
    /// When `layout` is not [valid](crate::Layout::is_valid).
    #[must_use]
    pub fn with_layout(mut self, layout: &crate::Layout) -> Self {
        self.walk.set_layout(layout);
        self
    }

    /// Medium offset of the block [Self::next_block] returned last.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Where a [Recovery::Prefix] walk goes on after the block [Self::next_block] returned last, or from its start
    /// before the first: a walk [resumed](Self::resume) there on bytes read later reads on as this one would. None in
    /// a [Recovery::Resync] walk.
    ///
    /// A follower of a growing file takes it after each block, and resumes there when the file grew: what a walk
    /// read past it (a torn block a writer is still writing, or one a resume will cut back) is read again.
    pub fn point(&self) -> Option<WalkPoint> {
        if self.recovery != Recovery::Prefix {
            return None;
        }
        let offset = match self.current {
            Some(c) => self.offset.checked_add(wide(c.len))?,
            None => self.passed_end,
        };
        Some(WalkPoint {
            offset,
            walk: self.walk,
        })
    }

    /// A [Recovery::Prefix] walk of the bytes from `point` on, read from `rd`, which starts there: it goes on with
    /// the session, the seq and the gap rules the walk that gave `point` had.
    pub fn resume(rd: R, point: WalkPoint) -> Self {
        let mut w = Self::new(rd).starting_at(point.offset);
        w.walk = point.walk;
        w
    }

    /// Have at least `n` bytes in `buf`, false at the end of the file.
    fn fill(&mut self, n: usize) -> Result<bool, FileError> {
        while self.buf.len() < n {
            if self.eof {
                return Ok(false);
            }
            let have = self.buf.len();
            // grown as bytes come, so a damaged length doesn't allocate what it says
            let want = n.min(have.saturating_add(READ_STEP)).max(READ_STEP);
            let got = self.rd.read(self.buf.grow(want));
            let now = got
                .as_ref()
                .ok()
                .and_then(|got| have.checked_add(*got))
                .filter(|now| *now <= want);
            self.buf.truncate(now.unwrap_or(have));
            match got {
                Ok(0) => self.eof = true,
                Ok(_) if now.is_none() => {
                    return Err(FileError::Io(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "the reader said it read more bytes than asked",
                    )));
                }
                Ok(_) => {}
                Err(e) => return Err(FileError::Io(e)),
            }
        }
        Ok(true)
    }

    /// Pass over the first `n` bytes of `buf`.
    fn consume(&mut self, n: usize) -> Result<(), FileError> {
        debug_assert!(n <= self.buf.len(), "consumed bytes are in buf");
        let n = n.min(self.buf.len());
        self.offset = self.offset.checked_add(wide(n)).ok_or_else(too_long)?;
        self.buf.pass(n);
        Ok(())
    }

    /// Bytes passed over since the medium offset `start`, one this walk passed.
    fn since(&self, start: u64) -> u64 {
        debug_assert!(self.offset >= start, "the walk only goes forward");
        self.offset.saturating_sub(start)
    }

    /// Pass over blank bytes from here: true when they are the end, running to the end of the file or, on a raw
    /// medium, longer than the layout's gap (what follows is no data of this session, and isn't read).
    fn skip_blank(&mut self) -> Result<bool, FileError> {
        let start = self.offset;
        loop {
            let n = self
                .buf
                .iter()
                .position(|b| !is_blank(*b))
                .unwrap_or(self.buf.len());
            self.consume(n)?;
            if self.walk.medium == Medium::Raw && self.since(start) > self.walk.gap_max() {
                self.done = true;
                return Ok(true);
            }
            if !self.buf.is_empty() {
                return Ok(false);
            }
            if !self.fill(1)? {
                return Ok(true);
            }
        }
    }

    /// The next good block, None at the end. Errors are reported one at a time: after a [FileError::Cut] the walk
    /// is over, after the others call again to go on.
    pub fn next_block(&mut self) -> Option<Result<Block<'_>, FileError>> {
        loop {
            match self.step() {
                // a copy with the same bytes as the block before it is dropped
                Ok(Next::Dropped) => {}
                Ok(Next::Block(c)) => {
                    self.current = Some(c);
                    let b = self.current();
                    debug_assert!(b.is_some(), "a block is taken with its bytes in buf");
                    return b.map(Ok);
                }
                Ok(Next::End) => return None,
                Err(e) => return Some(Err(e)),
            }
        }
    }

    /// The block handed out last, at the start of `buf`.
    fn current(&self) -> Option<Block<'_>> {
        self.current?.on(&self.buf)
    }

    /// Find the next block, at the start of `buf`.
    fn step(&mut self) -> Result<Next, FileError> {
        if self.done {
            return Ok(Next::End);
        }
        if let Some(c) = self.current.take() {
            self.consume(self.within_gap(c.len))?;
            self.passed_end = self.offset;
        }
        if self.gap_end.is_some_and(|end| end <= self.offset) {
            self.gap_end = None;
        }
        if !self.fill(1)? {
            return Ok(Next::End);
        }
        // a block starts with the magic, never with a blank byte
        let start = self.offset;
        if self.buf.first().is_some_and(|b| is_blank(*b)) && self.skip_blank()? {
            return Ok(Next::End);
        }
        let blank = self.offset > start;
        let error = match self.block_here()? {
            Here::Good(c) if blank && self.recovery == Recovery::Resync => {
                // a salvaged chunk is a bad block to a Prefix walk, never where it goes on
                let intact = c.on(&self.buf).is_some_and(|b| b.is_intact());
                self.enter_gap(start, intact.then_some(c))?;
                return Err(FileError::Bad {
                    offset: start,
                    error: BlockError::BadMagic,
                    skipped: self.since(start),
                });
            }
            Here::Good(c) => {
                // a salvaged chunk is a bad block to a Prefix walk
                if c.on(&self.buf).is_some_and(|b| !b.is_intact()) {
                    self.enter_gap(start, None)?;
                }
                if blank {
                    // blank bytes, then a block: within the gap rules a gap, else the end of data
                    return self.after_stretch(start, BlockError::BadMagic, Some(c));
                }
                return self.judge(c);
            }
            Here::Refused { len, error } if !blank => return self.refused(len, error),
            Here::Refused { error, .. } => error,
            Here::Truncated if !blank => return Err(self.truncated()),
            Here::Truncated => BlockError::BadMagic,
            Here::Bad(e) => e,
        };
        if self.recovery == Recovery::Resync {
            self.enter_gap(start, None)?;
        }
        match self.recovery {
            Recovery::Prefix => self.after_stretch(start, error, None),
            Recovery::Resync if blank => Err(FileError::Bad {
                offset: start,
                error,
                skipped: self.since(start),
            }),
            Recovery::Resync => Err(self.resync(error)),
        }
    }

    /// What starts `buf`: a good block (a damaged chunk with a good head too, in [Recovery::Resync]), a cut one, or
    /// bad bytes.
    fn block_here(&mut self) -> Result<Here, FileError> {
        if !self.fill(HEADER_LEN)? {
            return Ok(Here::Truncated);
        }
        let len = match block_len(&self.buf) {
            Ok(len) => len,
            Err(e) => return Ok(Here::Bad(e)),
        };
        if !self.fill(len)? {
            // the rest of the file is in `buf`: a block magic in it means the length was damaged
            let magic_later = self
                .buf
                .get(1..)
                .is_some_and(|rest| rest.windows(MAGIC.len()).any(|w| w == MAGIC));
            if self.recovery == Recovery::Resync && magic_later {
                return Ok(Here::Bad(BlockError::BadLength(report_len(len))));
            }
            return Ok(Here::Truncated);
        }
        Ok(match Block::read(&self.buf) {
            // a piece of a level this build doesn't handle: its head is refused, as a chunk of one is
            Ok(b)
                if b.kind == BlockKind::PieceHead
                    && let Some(level) = PieceHeadBlock::refused_level(&b) =>
            {
                Here::Refused {
                    len,
                    error: BlockError::UnhandledIntegrity { level },
                }
            }
            Ok(b) => Here::Good(Checked::of(&b)),
            // the head CRC holds: the length is the block's
            Err(error @ BlockError::PayloadUnchecked) => Here::Refused { len, error },
            // a chunk block with a good head: its length holds, the payload is handed out with its bad segments
            // marked, and the walk goes on after it without looking for a magic in the payload
            Err(e) if self.recovery == Recovery::Resync => match Block::salvage(&self.buf) {
                Ok(b) => Here::Good(Checked::of(&b)),
                Err(_) => Here::Bad(e),
            },
            Err(e) => Here::Bad(e),
        })
    }

    /// The good block `c` at the start of `buf`: taken, stale or out of order.
    fn judge(&mut self, c: Checked) -> Result<Next, FileError> {
        let Some(b) = c.on(&self.buf) else {
            return Err(self.truncated());
        };
        let check_seq = self.recovery == Recovery::Prefix;
        let mut verdict = self.walk.verdict(&b, check_seq);
        if self.recovery == Recovery::Resync {
            if self.gap_end.is_some() {
                // salvage in a gap: a session met in it goes on there, `walk` goes on past it
                if self.gap_walk.verdict(&b, false) == Verdict::Take {
                    self.gap_walk.take(&b);
                    verdict = Verdict::Take;
                }
            } else {
                match self.walk.verdict(&b, true) {
                    Verdict::Stale(_) | Verdict::Backwards { .. } => self.prefix_over = true,
                    // the block before it again with other bytes: a copy met in damage, or a rewrite; reported
                    d @ Verdict::Duplicate { same: false } if verdict == Verdict::Take => {
                        verdict = d
                    }
                    _ => {}
                }
            }
        }
        let mut verdict = match verdict {
            Verdict::Duplicate { same: false } if self.replacing => Verdict::Take,
            v => v,
        };
        self.replacing = false;
        let (session, seq) = (b.session, b.seq);
        // a data block of an id handed out before, however far back
        let mut first = None;
        let resync_data = self.recovery == Recovery::Resync && !is_free(&b);
        if verdict == Verdict::Take && resync_data {
            match self.seen.met((session, seq), c.len, b.crc()) {
                Met::New | Met::Same => {}
                Met::Other(at) => {
                    first = Some(at);
                    verdict = Verdict::Duplicate { same: false };
                }
            }
        }
        if verdict == Verdict::Take && resync_data {
            self.seen.take((session, seq), self.offset, c.len, b.crc());
        }
        if matches!(verdict, Verdict::Take) && self.gap_end.is_none() {
            self.walk.take(&b);
            if !is_free(&b) {
                self.last_data = Some(self.offset);
            }
        }
        match (verdict, self.recovery) {
            (Verdict::Take, _) => Ok(Next::Block(c)),
            (Verdict::Duplicate { same: true }, _) => {
                // dropped: the walk reads on after it
                self.consume(c.len)?;
                Ok(Next::Dropped)
            }
            (Verdict::Duplicate { same: false }, Recovery::Resync) => {
                let offset = self.offset;
                let block = b.bytes().to_vec();
                self.consume(self.within_gap(c.len))?;
                Err(FileError::Conflict {
                    first: first
                        .or_else(|| self.seen.blocks.get(&(session, seq)).map(|b| b.0))
                        .or(self.last_data)
                        .unwrap_or(offset),
                    offset,
                    session,
                    seq,
                    block,
                })
            }
            (Verdict::Duplicate { same: false }, Recovery::Prefix) => {
                self.replacing = true;
                Err(FileError::Replaced {
                    offset: self.offset,
                    session,
                    seq,
                })
            }
            (Verdict::Stale(session), Recovery::Prefix) => {
                Err(self.cut(self.offset, Cut::Stale { session }))
            }
            (Verdict::Stale(session), Recovery::Resync) => {
                let offset = self.offset;
                self.consume(self.within_gap(c.len))?;
                Err(FileError::Stale {
                    offset,
                    session,
                    len: wide(c.len),
                })
            }
            (Verdict::Backwards { seq, after }, _) => {
                Err(self.cut(self.offset, Cut::SeqBackwards { seq, after }))
            }
        }
    }

    /// A blank or bad stretch started at `start` (`buf` is past its blank bytes): the session's data going on within
    /// the layout's gap is a [FileError::Gap], else the stretch is the end of data, cut for `error`. `here` is the
    /// good block right after blank bytes.
    fn after_stretch(
        &mut self,
        start: u64,
        error: BlockError,
        here: Option<Checked>,
    ) -> Result<Next, FileError> {
        match self.resume_after(start, here, true)? {
            Some(at) if self.skip_to(at)? => self.gap(start),
            _ => Err(self.cut(start, Cut::Bad(error))),
        }
    }

    /// Where the session's data goes on past a blank or bad stretch started at `start` (`buf` is past its blank
    /// bytes), within the layout's gap; None when it doesn't. `here` is the good block right after blank bytes. With
    /// `pass_over` the bytes before each probe are passed over, else they are read ahead in `buf` (at most
    /// [RESYNC_AHEAD_MAX]). At most [GAP_CHECKS_MAX] probes whose header [may resume](SessionState::may_resume) the walk
    /// are checked whole.
    fn resume_after(
        &mut self,
        start: u64,
        here: Option<Checked>,
        pass_over: bool,
    ) -> Result<Option<u64>, FileError> {
        let walk = self.walk;
        let in_gap = self
            .offset
            .checked_sub(start)
            .is_some_and(|n| n <= walk.gap_max());
        if in_gap
            && let Some(b) = here.and_then(|c| c.on(&self.buf))
            && walk.resumes(&b)
        {
            return Ok(Some(self.offset));
        }
        let mut checks = 0;
        for p in walk.probes(start, self.offset) {
            if pass_over && !self.skip_to(p)? {
                break;
            }
            #[cfg(test)]
            {
                self.probes = self.probes.saturating_add(1);
            }
            // probes are past here, within the gap
            let Some(ahead) = p
                .checked_sub(self.offset)
                .and_then(|n| usize::try_from(n).ok())
            else {
                break;
            };
            if !self.fill(ahead.saturating_add(HEADER_LEN))? {
                break;
            }
            let Some(head) = self.buf.get(ahead..).and_then(Head::read) else {
                continue;
            };
            if !walk.may_resume(&head) {
                continue;
            }
            if checks == GAP_CHECKS_MAX {
                break;
            }
            checks = checks.saturating_add(1);
            #[cfg(test)]
            {
                self.checks = self.checks.saturating_add(1);
            }
            if !self.fill(ahead.saturating_add(head.len))? {
                break;
            }
            if let Some(bytes) = self.buf.get(ahead..).and_then(|b| b.get(..head.len))
                && let Ok(b) = Block::read(bytes)
                && walk.resumes(&b)
            {
                return Ok(Some(p));
            }
        }
        Ok(None)
    }

    /// [Recovery::Resync] at a stretch started at `start`, outside a gap: note where a [Recovery::Prefix] walk would go
    /// on past it, so blocks before there change no session of `walk` and none reaches past it. None: that walk ends
    /// here, and no stretch after it is looked past.
    fn enter_gap(&mut self, start: u64, here: Option<Checked>) -> Result<(), FileError> {
        if self.gap_end.is_none() && !self.prefix_over {
            self.gap_end = self.resume_after(start, here, false)?;
            self.prefix_over = self.gap_end.is_none();
            self.gap_walk = self.walk;
        }
        Ok(())
    }

    /// Bytes of `len` from here to pass over, short of the end of the gap the walk is in.
    fn within_gap(&self, len: usize) -> usize {
        self.gap_end
            .and_then(|end| end.checked_sub(self.offset))
            .filter(|n| *n > 0)
            .and_then(|n| usize::try_from(n).ok())
            .map_or(len, |n| n.min(len))
    }

    /// Pass over the bytes up to medium offset `to`; false when the stream ends before it.
    fn skip_to(&mut self, to: u64) -> Result<bool, FileError> {
        while let Some(ahead) = to.checked_sub(self.offset).filter(|n| *n > 0) {
            if self.buf.is_empty() && !self.fill(1)? {
                return Ok(false);
            }
            let n = usize::try_from(ahead).map_or(self.buf.len(), |n| n.min(self.buf.len()));
            self.consume(n)?;
        }
        Ok(true)
    }

    /// The gap from `start` to here, the walk going on with the block here; an error to report. Here is never
    /// before `start`; if it were, the stretch would be the end of data.
    fn gap(&mut self, start: u64) -> Result<Next, FileError> {
        match self.offset.checked_sub(start) {
            Some(len) => Err(FileError::Gap { offset: start, len }),
            None => Err(self.cut(start, Cut::Bad(BlockError::BadMagic))),
        }
    }

    /// The next good block, None at the end; what was cut, skipped or passed over is handed to `on_error`, a
    /// salvaged chunk block ([Block::is_intact] false) as [FileError::Bad].
    pub fn next_good(&mut self, mut on_error: impl FnMut(FileError)) -> Option<Block<'_>> {
        loop {
            match self.next_block()? {
                Ok(b) if b.is_intact() => break,
                Ok(b) => {
                    let skipped = wide(b.len());
                    on_error(FileError::Bad {
                        offset: self.offset,
                        error: BlockError::BadCrc,
                        skipped,
                    });
                }
                Err(e) => on_error(e),
            }
        }
        self.current()
    }

    /// The block of `len` bytes here, refused as a level (`error`): stepped over, never cut or salvaged.
    fn refused(&mut self, len: usize, error: BlockError) -> Result<Next, FileError> {
        let offset = self.offset;
        self.consume(self.within_gap(len))?;
        Err(FileError::Refused {
            offset,
            error,
            len: wide(len),
        })
    }

    fn cut(&mut self, offset: u64, why: Cut) -> FileError {
        self.done = true;
        FileError::Cut { offset, why }
    }

    fn truncated(&mut self) -> FileError {
        let (offset, len) = (self.offset, wide(self.buf.len()));
        if self.recovery == Recovery::Prefix {
            return self.cut(offset, Cut::Truncated { len });
        }
        if let Err(e) = self.consume(self.buf.len()) {
            return e;
        }
        FileError::Truncated { offset, len }
    }

    /// Skip to the next block magic after the start of the current bytes.
    fn resync(&mut self, error: BlockError) -> FileError {
        let offset = self.offset;
        match self.skip_to_magic() {
            Ok(()) => FileError::Bad {
                offset,
                error,
                skipped: self.since(offset),
            },
            Err(e) => e,
        }
    }

    /// Pass over the first byte, then up to the next block magic or the end of the stream.
    fn skip_to_magic(&mut self) -> Result<(), FileError> {
        self.consume(1)?;
        loop {
            if let Some(i) = self.buf.windows(MAGIC.len()).position(|w| w == MAGIC) {
                return self.consume(i);
            }
            // keep the last bytes, a magic may start there
            let keep = MAGIC.len().saturating_sub(1).min(self.buf.len());
            self.consume(self.buf.len().saturating_sub(keep))?;
            let more = self.buf.len().saturating_add(1);
            if !matches!(self.fill(more), Ok(true)) {
                return self.consume(self.buf.len());
            }
        }
    }
}

/// What starts the bytes a [FileBlocks] holds.
enum Here {
    /// A good block (or, in [Recovery::Resync], a chunk with a good head).
    Good(Checked),
    /// A block of an integrity level this build doesn't handle, `len` bytes by its checked head.
    Refused {
        len: usize,
        error: BlockError,
    },
    /// The file ends inside the header or the block.
    Truncated,
    Bad(BlockError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ClockNum, TrackNum};

    /// The Resync window holds the last [RESYNC_SEQS_MAX] data blocks: the oldest id leaves it, the rest still meet a
    /// block of their id.
    #[test]
    fn the_resync_window_forgets_the_oldest_id_past_its_max() {
        let tag = SessionTag::from_u32(1);
        let mut seen = Seen::default();
        for seq in 0..=RESYNC_SEQS_MAX as u64 {
            seen.take((tag, seq), seq * 100, 10, 7);
        }
        assert_eq!(
            (seen.ids.len(), seen.blocks.len()),
            (RESYNC_SEQS_MAX, RESYNC_SEQS_MAX)
        );
        assert!(matches!(seen.met((tag, 0), 10, 8), Met::New));
        assert!(matches!(seen.met((tag, 1), 10, 8), Met::Other(100)));
        assert!(matches!(seen.met((tag, 1), 10, 7), Met::Same));
        assert!(matches!(
            seen.met((SessionTag::from_u32(2), 1), 10, 8),
            Met::New
        ));
    }

    /// A records block of session 1 with seq `seq`.
    fn records_block(seq: u64) -> Vec<u8> {
        let mut buf = [0u8; 128];
        let id = crate::BlockId::new(SessionTag::writable(1), seq);
        let mut w =
            crate::BlockWriter::new(&mut buf, crate::BlockKind::Records, id, &[ClockNum::new(1)])
                .unwrap();
        let at = crate::ClockTime::new(ClockNum::new(1), 1);
        w.push(TrackNum::new(9), at, &[&[seq as u8; 20]]).unwrap();
        let len = w.finish();
        buf[..len].to_vec()
    }

    /// A walk stopped at a torn block goes on from its point once the block is whole, with the session and seq it
    /// had: the torn bytes are read again, a block that goes back is refused.
    #[test]
    fn a_walk_resumed_at_its_point_reads_on_as_it_would() {
        let blocks: Vec<Vec<u8>> = (0..3).map(records_block).collect();
        let whole = blocks.concat();
        let torn = &whole[..whole.len() - 5];
        let mut walk = FileBlocks::new(torn);
        assert_eq!(walk.point().unwrap(), WalkPoint::start(Medium::File));
        let mut seqs = Vec::new();
        let mut point = walk.point().unwrap();
        while let Some(b) = walk.next_block() {
            match b {
                Ok(b) => seqs.push(b.seq),
                Err(e) => assert!(matches!(
                    e,
                    FileError::Cut {
                        why: Cut::Truncated { .. },
                        ..
                    }
                )),
            }
            point = walk.point().unwrap();
        }
        assert_eq!(seqs, [0, 1]);
        let at = blocks[0].len() + blocks[1].len();
        assert_eq!(point.offset(), at as u64);

        let mut on = FileBlocks::resume(&whole[at..], point);
        let b = on.next_block().unwrap().unwrap();
        assert_eq!((b.seq, on.offset()), (2, at as u64));
        assert!(on.next_block().is_none());

        let mut back = FileBlocks::resume(&blocks[0][..], point);
        let e = back.next_block().unwrap().unwrap_err();
        assert!(matches!(
            e,
            FileError::Cut {
                why: Cut::SeqBackwards { seq: 0, after: 1 },
                ..
            }
        ));
        assert!(
            FileBlocks::with_recovery(&whole[..], Recovery::Resync)
                .point()
                .is_none()
        );
    }

    /// A reader that says it read one byte more than it was asked for.
    struct Overreads;

    impl Read for Overreads {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            Ok(buf.len() + 1)
        }
    }

    #[test]
    fn a_reader_over_its_buffer_is_an_error() {
        let mut blocks = FileBlocks::new(Overreads);
        let e = blocks.next_block().unwrap().unwrap_err();
        assert!(matches!(e, FileError::Io(e) if e.kind() == io::ErrorKind::InvalidData));
    }

    #[test]
    fn an_offset_past_u64_is_an_error() {
        let bytes = [0u8; 16];
        let mut blocks = FileBlocks::new(&bytes[..]).starting_at(u64::MAX - 3);
        let e = blocks.next_block().unwrap().unwrap_err();
        assert!(matches!(e, FileError::Io(e) if e.kind() == io::ErrorKind::FileTooLarge));
    }

    /// Probe headers read and blocks checked whole by a Resync walk of a records block and `magics` block magics
    /// after it, on an SD card's layout, read as `medium`.
    fn work_over_magics(magics: usize, medium: Medium) -> (usize, usize) {
        let mut buf = [0u8; 256];
        let id = crate::BlockId::new(SessionTag::writable(0x7465_7374), 1);
        let mut w =
            crate::BlockWriter::new(&mut buf, crate::BlockKind::Records, id, &[ClockNum::new(1)])
                .unwrap();
        w.push(
            TrackNum::new(9),
            crate::ClockTime::new(ClockNum::new(1), 1),
            &[&[1u8; 40]],
        )
        .unwrap();
        let len = w.finish();
        let mut bytes = buf[..len].to_vec();
        bytes.extend(MAGIC.repeat(magics));
        let mut rd = FileBlocks::with_recovery(&bytes[..], Recovery::Resync)
            .on(medium)
            .with_layout(&crate::Layout::UNALIGNED.on_sd_card());
        let mut steps = 0;
        while rd.next_block().is_some() {
            steps += 1;
        }
        assert!(steps > magics / 2, "the walk looks at each magic");
        (rd.probes, rd.checks)
    }

    /// A Resync walk looks for where the data goes on only up to where a Prefix walk ends: past a stretch with no
    /// block of the session after it, the walk pays one gap's probes, however long the stretch.
    #[test]
    fn resync_looks_past_a_stretch_once() {
        for medium in [Medium::File, Medium::Raw] {
            // 1 MiB and 8 MiB of magics, the longer past the SD card's 4 MiB gap
            for magics in [1 << 18, 1 << 21] {
                let (probes, checks) = work_over_magics(magics, medium);
                assert!(
                    probes <= crate::GAP_PROBES_MAX as usize + 1,
                    "{medium:?}, {magics} magics: {probes} probes"
                );
                assert!(
                    checks <= GAP_CHECKS_MAX as usize,
                    "{medium:?}, {magics} magics: {checks} checks"
                );
            }
        }
    }
}
