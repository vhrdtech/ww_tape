//! What a reader walking blocks in order keeps: the current session, the last seq, the layout's gap rules.

use core::num::NonZeroU64;

use crate::piece::PieceHeadBlock;
use crate::{
    Block, BlockKind, Describe, FLAG_REPEAT, HEADER_LEN, Layout, Medium, RecordTrack, SessionTag,
    block_len,
};

/// Whether a good block met in a walk belongs to the data.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The next block of the current session, or the start of a session that may start there.
    Take,
    /// A block of another session where no session may start: stale data, never taken.
    Stale(SessionTag),
    /// A data block of the current session whose seq is before the last one's.
    Backwards { seq: u64, after: u64 },
    /// The same (session, seq) as the last block taken: a duplicate, not a cut. `same` when its bytes are the same
    /// (dropped); else the later copy wins.
    Duplicate { same: bool },
}

/// The header fields a walk judges a block by, read before its CRC is checked: what makes a gap probe worth checking
/// whole.
#[derive(Copy, Clone, Debug)]
pub(crate) struct Head {
    /// The whole block's length, as its header says
    #[cfg_attr(
        not(feature = "std"),
        expect(dead_code, reason = "only FileBlocks reads it, which needs std")
    )]
    pub(crate) len: usize,
    kind: BlockKind,
    session: SessionTag,
    seq: u64,
    repeat: bool,
}

impl Head {
    /// The header at the start of `bytes`; None when it has no magic or its length is out of range.
    pub(crate) fn read(bytes: &[u8]) -> Option<Self> {
        let len = block_len(bytes).ok()?;
        let h: &[u8; HEADER_LEN] = bytes.get(..HEADER_LEN)?.try_into().ok()?;
        let [_, _, _, _, _, kind, f0, f1, ..] = *h;
        let seq = u64::from_le_bytes(h[16..24].try_into().ok()?);
        let session = u32::from_le_bytes(h[24..28].try_into().ok()?);
        Some(Self {
            len,
            kind: BlockKind::from_u8(kind),
            session: SessionTag::from_u32(session),
            seq,
            repeat: u16::from_le_bytes([f0, f1]) & FLAG_REPEAT != 0,
        })
    }
}

/// The session and seq of the blocks taken so far, and the gap rules of the newest [Layout] read.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct SessionState {
    pub(crate) medium: Medium,
    session: Option<SessionTag>,
    last_seq: Option<u64>,
    /// Length and CRC of the last data block taken
    last_bytes: Option<(usize, u32)>,
    gap_max: u64,
    resume_align: NonZeroU64,
}

impl SessionState {
    pub(crate) fn new(medium: Medium) -> Self {
        Self {
            medium,
            session: None,
            last_seq: None,
            last_bytes: None,
            gap_max: 0,
            resume_align: NonZeroU64::MIN,
        }
    }

    /// Take `layout`'s gap rules (before any definition says them, for a medium layer that knows them).
    ///
    /// # Panics
    ///
    /// When `layout` is not [valid](Layout::is_valid); a decoded one always is.
    pub(crate) fn set_layout(&mut self, layout: &Layout) {
        assert!(
            layout.is_valid(),
            "a layout's gap rules are within GAP_MAX_MAX and GAP_PROBES_MAX"
        );
        self.gap_max = u64::from(layout.gap_max.get());
        // a valid layout's resume_align is 1 and up
        self.resume_align =
            NonZeroU64::new(u64::from(layout.resume_align.get())).unwrap_or(NonZeroU64::MIN);
    }

    pub(crate) fn gap_max(&self) -> u64 {
        self.gap_max
    }

    /// What `b` is to the walk. `check_seq` false leaves the seq order out (a salvage walk).
    pub(crate) fn verdict(&self, b: &Block<'_>, check_seq: bool) -> Verdict {
        let Some(session) = self.session else {
            return Verdict::Take;
        };
        if b.session != session {
            return if self.starts_session(b, session) {
                Verdict::Take
            } else {
                Verdict::Stale(b.session)
            };
        }
        if !check_seq || is_free(b) {
            return Verdict::Take;
        }
        match self.last_seq {
            Some(after) if b.seq == after => Verdict::Duplicate {
                same: self.last_bytes == Some((b.len(), b.crc())),
            },
            Some(after) if b.seq < after => Verdict::Backwards { seq: b.seq, after },
            _ => Verdict::Take,
        }
    }

    /// Whether `b`, of another session than `current`, may start a session here: on a file a record block whose
    /// first record is on track 0 (another producer session appended, starting with its definitions or a full
    /// repeat) or a piece head (concatenated pieces); on a raw medium only a piece head naming `current` as the
    /// session before it.
    fn starts_session(&self, b: &Block<'_>, current: SessionTag) -> bool {
        match self.medium {
            Medium::File => b.kind == BlockKind::PieceHead || starts_with_definitions(b),
            Medium::Raw => {
                b.kind == BlockKind::PieceHead
                    && PieceHeadBlock::read(b)
                        .map(|h| h.head().prev_session)
                        .ok()
                        .or_else(|| PieceHeadBlock::refused(b).map(|(l, _)| l.prev_session))
                        == Some(current)
            }
        }
    }

    /// Note `b`, taken: its session and seq, and the layout its track-0 records say.
    pub(crate) fn take(&mut self, b: &Block<'_>) {
        if self.session != Some(b.session) {
            self.session = Some(b.session);
            self.last_seq = None;
            self.last_bytes = None;
        }
        if !is_free(b) {
            self.last_seq = Some(b.seq);
            self.last_bytes = Some((b.len(), b.crc()));
        }
        if let Some(l) = layout_in(b) {
            self.set_layout(&l);
        }
    }

    /// Whether a good block found past a gap goes on with the data: the current session's next block (a seq after the
    /// last, or a block that takes no part in the order), the last block again (a rewrite after a failed program,
    /// same bytes or not), or a session that may start there.
    pub(crate) fn resumes(&self, b: &Block<'_>) -> bool {
        b.checked() != crate::Checked::Unchecked
            && matches!(
                self.verdict(b, true),
                Verdict::Take | Verdict::Duplicate { .. }
            )
    }

    /// Whether the block whose header is `h` may go on with the data past a gap: a superset of [Self::resumes], read
    /// from the header alone, so only such a probe is checked whole.
    pub(crate) fn may_resume(&self, h: &Head) -> bool {
        let Some(session) = self.session else {
            return true;
        };
        if h.session != session {
            return match self.medium {
                Medium::File => h.kind.holds_records() || h.kind == BlockKind::PieceHead,
                Medium::Raw => h.kind == BlockKind::PieceHead,
            };
        }
        if crate::index::is_structural(h.kind) || h.repeat {
            return true;
        }
        self.last_seq.is_none_or(|after| h.seq >= after)
    }

    /// The places past a blank or bad stretch starting at `start` a reader probes for the data going on, ascending,
    /// none at or before `from`: multiples of [Layout::resume_align] within [Layout::gap_max] of `start`. (The
    /// first non-blank byte is the caller's to probe.)
    /// At most [crate::GAP_PROBES_MAX] + 1 (a valid layout's `gap_max / resume_align` is within it), and none past
    /// `u64::MAX`.
    pub(crate) fn probes(&self, start: u64, from: u64) -> impl Iterator<Item = u64> + use<> {
        let (align, gap_max) = (self.resume_align, self.gap_max);
        // places past u64::MAX don't exist: the gap ends there
        let end = start.saturating_add(gap_max);
        let first = (from.max(start) / align)
            .checked_add(1)
            .and_then(|n| n.checked_mul(align.get()));
        (0..=u64::from(crate::GAP_PROBES_MAX))
            .map_while(move |i| first?.checked_add(i.checked_mul(align.get())?))
            .take_while(move |p| *p <= end && gap_max > 0)
    }
}

/// Whether `b` takes no part in the seq order: a block of the file structure (it carries the last counted seq) or a
/// block of puts and copies ([FLAG_REPEAT](crate::FLAG_REPEAT)).
pub(crate) fn is_free(b: &Block<'_>) -> bool {
    crate::index::is_structural(b.kind) || b.is_repeat()
}

/// Whether `b` is a record block whose first record is on track 0: definitions, made or put.
pub(crate) fn starts_with_definitions(b: &Block<'_>) -> bool {
    if !matches!(b.kind, BlockKind::Records | BlockKind::CodedRecords) {
        return false;
    }
    let mut buf = alloc::vec::Vec::new();
    b.decode(&mut buf).ok().and_then(|d| d.first_track()) == Some(RecordTrack::Definitions)
}

/// The [Layout] the track-0 records of record block `b` say, the last one in it; None for any other block.
pub(crate) fn layout_in(b: &Block<'_>) -> Option<Layout> {
    if !matches!(b.kind, BlockKind::Records | BlockKind::CodedRecords) {
        return None;
    }
    let mut buf = alloc::vec::Vec::new();
    let d = b.decode(&mut buf).ok()?;
    // a block of data holds no track-0 record: one look at its first
    if d.first_track() != Some(RecordTrack::Definitions) {
        return None;
    }
    d.records()
        .map_while(Result::ok)
        .filter(|r| r.track.is_definitions())
        .filter_map(|r| {
            match <Describe<'_> as shrink_wrap::DeserializeShrinkWrap>::from_ww_bytes(r.item) {
                Ok(Describe::Layout(l)) => Some(l),
                _ => None,
            }
        })
        .last()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ClockNum, TrackNum};
    use exact_units::Bytes;

    fn walk(gap_max: u32, resume_align: u32) -> SessionState {
        let mut w = SessionState::new(Medium::File);
        w.set_layout(&Layout {
            gap_max: Bytes::new(gap_max),
            resume_align: Bytes::new(resume_align),
            ..Layout::UNALIGNED
        });
        w
    }

    #[test]
    fn probes_are_the_resume_places_in_the_gap() {
        let w = walk(2048, 512);
        assert_eq!(
            w.probes(1000, 1000).collect::<Vec<_>>(),
            [1024, 1536, 2048, 2560]
        );
        assert_eq!(w.probes(1000, 2100).collect::<Vec<_>>(), [2560]);
        assert_eq!(walk(0, 1).probes(0, 0).count(), 0);
    }

    /// A gap reaching past u64::MAX ends there: the probes stop, they don't repeat u64::MAX.
    #[test]
    fn probes_near_the_end_of_u64_stop() {
        let w = walk(4096, 1);
        let near = u64::MAX - 5;
        assert_eq!(
            w.probes(near, near).take(10_000).collect::<Vec<_>>(),
            [near + 1, near + 2, near + 3, near + 4, near + 5]
        );
    }

    /// On a raw medium a piece head naming the session just ended starts a session whatever its integrity level, so
    /// the reader lists the piece and refuses it as that level instead of taking it for stale data.
    #[test]
    fn a_raw_medium_takes_a_head_of_another_level() {
        use crate::piece::{
            OwnerId, PieceHead, PieceId, PieceIdent, piece_head_len, write_piece_head,
        };
        let a = SessionTag::writable(0x0a0a_0a0a);
        let b = SessionTag::writable(0x0b0b_0b0b);
        let mut buf = [0u8; 256];
        let mut w = crate::BlockWriter::new(
            &mut buf,
            BlockKind::Records,
            crate::BlockId::new(a, 0),
            &[ClockNum::new(1)],
        )
        .unwrap();
        w.push(
            TrackNum::new(9),
            crate::ClockTime::new(ClockNum::new(1), 1),
            &[&[1u8; 4]],
        )
        .unwrap();
        let n = w.finish();
        let mut walk = SessionState::new(Medium::Raw);
        walk.take(&Block::read(&buf[..n]).unwrap());
        let head = PieceHead {
            ident: PieceIdent {
                id: PieceId([1; 16]),
                owner: OwnerId([2; 32]),
                seq: 0,
            },
            prev: None,
            prev_session: a,
            integrity: crate::Integrity::Full,
            unlisted_keys_unknown: false,
        };
        let mut h = alloc::vec![0u8; piece_head_len(0)];
        write_piece_head(&mut h, crate::BlockId::new(b, 0), &head, &[]).unwrap();
        for level in [0, 1, 7] {
            h[146] = level;
            crate::index::seal(&mut h);
            let block = Block::read(&h).unwrap();
            assert_eq!(walk.verdict(&block, true), Verdict::Take, "level {level}");
        }
        let other = PieceHead {
            prev_session: b,
            ..head
        };
        write_piece_head(&mut h, crate::BlockId::new(b, 0), &other, &[]).unwrap();
        h[146] = 1;
        crate::index::seal(&mut h);
        assert_eq!(
            walk.verdict(&Block::read(&h).unwrap(), true),
            Verdict::Stale(b)
        );
    }

    #[test]
    fn probes_are_at_most_the_cap() {
        let w = walk(crate::GAP_PROBES_MAX, 1);
        assert_eq!(
            w.probes(0, 0).take(100_000).count(),
            crate::GAP_PROBES_MAX as usize
        );
    }
}
