//! The bytes of a [MemTape](super::MemTape): extents that are filled and never moved, and the seals that mark where
//! pieces start.
//!
//! Appending fills the last extent and then adds a new one as large as the tape holds, at least [EXTENT_MIN] and at
//! most [EXTENT_MAX] (or as a [Segments::reserve] hint asks), so a write never copies a byte written before and a
//! reader waits only for the bytes being appended. A new piece seals the bytes before it ([Segments::rebase] to the
//! end), and so does a close. [Segments::drop_front] cuts at a seal and keeps every offset: the dropped bytes are
//! counted in [Segments::front], not shifted out. An extent may hold bytes of several pieces, so the memory of the
//! extent a cut falls inside is freed with the next cut past it.

use alloc::vec::Vec;

use crate::reader::wide;

/// Smallest extent added when the last one is full, and the unit of every extent's size: a huge page of x86-64
/// and aarch64. An extent of whole huge pages lets the kernel back it with them, a page fault per 2 MiB, not per
/// 4 KiB.
pub(super) const EXTENT_MIN: usize = 2 << 20;

/// What an extent's size leaves for the allocator's own header, so that the mapping with it is whole huge pages:
/// one page, which covers glibc's header on its mmap path.
const EXTENT_HEADROOM: usize = 4 << 10;

/// Largest extent added when the last one is full and no hint asked for more: what an extent may leave unused.
pub(super) const EXTENT_MAX: usize = 64 << 20;

/// Why a read or a cut of [Segments] failed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(super) enum SegmentError {
    /// Past the end of the bytes, or beyond `u64`.
    OutOfRange,
    /// Before [Segments::front]: dropped by [Segments::drop_front].
    Dropped { front: u64 },
}

#[derive(Debug, Default)]
pub(super) struct Segments {
    /// Bytes dropped before the first extent.
    front: u64,
    /// The extents in order, each with its offset; contiguous from `front`. Only the last one grows, within its
    /// capacity.
    extents: Vec<(u64, Vec<u8>)>,
    /// Where pieces start after `front`, ascending: the places [Self::drop_front] may cut at.
    seals: Vec<u64>,
}

impl Segments {
    /// No bytes, the first one to come at `front`: a copy of a tape whose oldest bytes were dropped keeps its
    /// offsets.
    pub(super) fn starting_at(front: u64) -> Self {
        Self {
            front,
            ..Self::default()
        }
    }

    /// Offset of the first byte kept.
    pub(super) fn front(&self) -> u64 {
        self.front
    }

    /// Offset of the end: every byte ever written, dropped ones included.
    pub(super) fn len(&self) -> u64 {
        self.extents
            .last()
            .map_or(self.front, |(at, b)| Self::end_of(*at, b))
    }

    /// Bytes held in memory: [Self::len] less [Self::front].
    pub(super) fn held(&self) -> u64 {
        self.len().saturating_sub(self.front)
    }

    pub(super) fn append(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let spare = self
                .open_extent()
                .map_or(0, |v| v.capacity().saturating_sub(v.len()));
            if spare == 0 {
                self.add_extent(self.extent_len(bytes.len()));
                continue;
            }
            let (now, rest) = bytes.split_at(spare.min(bytes.len()));
            if let Some(v) = self.open_extent() {
                v.extend_from_slice(now);
            }
            bytes = rest;
        }
    }

    /// Room for `bytes` more in the last extent: when it is short, a new one at least that large.
    pub(super) fn reserve(&mut self, bytes: usize) {
        let spare = self
            .open_extent()
            .map_or(0, |v| v.capacity().saturating_sub(v.len()));
        if spare < bytes {
            self.add_extent(self.extent_len(bytes));
        }
    }

    /// Seal the bytes written: a piece starts at the end.
    pub(super) fn seal(&mut self) {
        let end = self.len();
        if end > self.front && self.seals.last().is_none_or(|s| *s < end) {
            self.seals.push(end);
        }
    }

    /// A piece starts at `base`, at most [Self::len] and not before [Self::front]: the bytes before it are sealed.
    pub(super) fn rebase(&mut self, base: u64) -> Result<(), SegmentError> {
        self.within(base)?;
        self.seals.retain(|s| *s < base);
        if base > self.front {
            self.seals.push(base);
        }
        Ok(())
    }

    /// Drop every byte from `len` on, not before [Self::front].
    pub(super) fn truncate(&mut self, len: u64) -> Result<(), SegmentError> {
        self.within(len)?;
        self.extents.retain(|(at, _)| *at < len);
        if let Some((at, v)) = self.extents.last_mut() {
            let keep = len
                .checked_sub(*at)
                .and_then(|n| usize::try_from(n).ok())
                .ok_or(SegmentError::OutOfRange)?;
            v.truncate(keep);
        }
        self.seals.retain(|s| *s <= len);
        Ok(())
    }

    /// Free the bytes before the last seal at or before `to`: the new [Self::front]. Pieces go whole; the bytes
    /// after the last seal, the piece being written, are kept.
    pub(super) fn drop_front(&mut self, to: u64) -> u64 {
        let n = self.seals.partition_point(|s| *s <= to);
        let Some(&cut) = n.checked_sub(1).and_then(|i| self.seals.get(i)) else {
            return self.front;
        };
        self.seals.drain(..n);
        let gone = self
            .extents
            .partition_point(|(at, b)| Self::end_of(*at, b) <= cut);
        self.extents.drain(..gone);
        self.front = cut;
        self.front
    }

    /// Fill `buf` from `offset`, across extents.
    pub(super) fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), SegmentError> {
        if offset < self.front {
            return Err(SegmentError::Dropped { front: self.front });
        }
        let end = offset
            .checked_add(wide(buf.len()))
            .ok_or(SegmentError::OutOfRange)?;
        if end > self.len() {
            return Err(SegmentError::OutOfRange);
        }
        // the first extent the read touches: the last one starting at or before `offset`
        let first = self
            .extents
            .partition_point(|(at, _)| *at <= offset)
            .saturating_sub(1);
        let mut at = offset;
        let mut out = buf;
        for (start, bytes) in self.extents.get(first..).unwrap_or_default() {
            if out.is_empty() {
                break;
            }
            let skip = at.checked_sub(*start).ok_or(SegmentError::OutOfRange)?;
            let from = usize::try_from(skip)
                .ok()
                .and_then(|s| bytes.get(s..))
                .ok_or(SegmentError::OutOfRange)?;
            let n = from.len().min(out.len());
            let (head, rest) = out.split_at_mut(n);
            head.copy_from_slice(from.get(..n).ok_or(SegmentError::OutOfRange)?);
            out = rest;
            at = at.saturating_add(wide(n));
        }
        if out.is_empty() {
            Ok(())
        } else {
            Err(SegmentError::OutOfRange)
        }
    }

    /// The last extent, which appends go on in.
    fn open_extent(&mut self) -> Option<&mut Vec<u8>> {
        self.extents.last_mut().map(|(_, v)| v)
    }

    /// Capacity of a new extent that takes `bytes`: as large as the tape holds, within [EXTENT_MIN] and
    /// [EXTENT_MAX], or `bytes` when more, in whole [EXTENT_MIN]s.
    fn extent_len(&self, bytes: usize) -> usize {
        let held = usize::try_from(self.held()).unwrap_or(usize::MAX);
        round_up(
            held.clamp(EXTENT_MIN - EXTENT_HEADROOM, EXTENT_MAX - EXTENT_HEADROOM)
                .max(bytes),
        )
    }

    fn add_extent(&mut self, cap: usize) {
        let at = self.len();
        self.extents.push((at, Vec::with_capacity(cap)));
    }

    /// `at` within [Self::front] and [Self::len].
    fn within(&self, at: u64) -> Result<(), SegmentError> {
        if at < self.front {
            return Err(SegmentError::Dropped { front: self.front });
        }
        if at > self.len() {
            return Err(SegmentError::OutOfRange);
        }
        Ok(())
    }

    /// End of an extent at `at`.
    fn end_of(at: u64, bytes: &[u8]) -> u64 {
        at.saturating_add(wide(bytes.len()))
    }
}

/// `n` and [EXTENT_HEADROOM] rounded up to a multiple of [EXTENT_MIN], less the headroom; `n` itself past
/// `usize::MAX`.
fn round_up(n: usize) -> usize {
    n.saturating_add(EXTENT_HEADROOM)
        .div_ceil(EXTENT_MIN)
        .checked_mul(EXTENT_MIN)
        .and_then(|m| m.checked_sub(EXTENT_HEADROOM))
        .unwrap_or(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(s: &Segments) -> Vec<u8> {
        let mut out = vec![0; usize::try_from(s.held()).unwrap()];
        s.read_at(s.front(), &mut out).unwrap();
        out
    }

    #[test]
    fn reads_across_extents() {
        let mut s = Segments::default();
        s.append(b"abc");
        s.rebase(3).unwrap();
        s.append(b"de");
        s.rebase(5).unwrap();
        s.append(b"fgh");
        assert_eq!(s.seals, [3, 5]);
        s.reserve(EXTENT_MIN);
        s.append(b"ij");
        assert_eq!(
            s.extents.len(),
            2,
            "a hint past the room left adds an extent"
        );
        let mut buf = [0; 6];
        s.read_at(1, &mut buf).unwrap();
        assert_eq!(&buf, b"bcdefg");
        s.read_at(6, &mut buf[..4]).unwrap();
        assert_eq!(&buf[..4], b"ghij");
        assert_eq!(s.read_at(9, &mut [0; 2]), Err(SegmentError::OutOfRange));
        assert_eq!(s.read_at(10, &mut []), Ok(()));
    }

    #[test]
    fn appends_never_move_what_is_written() {
        let mut s = Segments::default();
        let chunk = vec![7u8; EXTENT_MIN / 2 + 1];
        s.append(&chunk);
        let first = s.extents[0].1.as_ptr();
        for _ in 0..8 {
            s.append(&chunk);
        }
        assert_eq!(s.extents[0].1.as_ptr(), first, "the first extent stayed");
        assert!(s.extents.len() > 1, "{} extents", s.extents.len());
        assert_eq!(s.held(), 9 * chunk.len() as u64);
        assert!(bytes(&s).iter().all(|b| *b == 7));
        // every extent is whole huge pages less the headroom
        for (_, v) in &s.extents {
            assert_eq!(
                (v.capacity() + EXTENT_HEADROOM) % EXTENT_MIN,
                0,
                "{}",
                v.capacity()
            );
        }
    }

    #[test]
    fn rebase_back_moves_the_seals() {
        let mut s = Segments::default();
        s.append(b"abc");
        s.rebase(3).unwrap();
        s.append(b"de");
        s.seal();
        s.rebase(1).unwrap();
        assert_eq!(s.seals, [1]);
        s.append(b"x");
        assert_eq!(bytes(&s), b"abcdex");
        s.truncate(2).unwrap();
        assert_eq!(bytes(&s), b"ab");
        assert_eq!(s.rebase(3), Err(SegmentError::OutOfRange));
        assert_eq!(s.drop_front(9), 1);
        assert_eq!(bytes(&s), b"b");
    }

    #[test]
    fn drop_front_keeps_offsets() {
        let mut s = Segments::default();
        s.append(b"abc");
        s.rebase(3).unwrap();
        s.append(b"de");
        s.rebase(5).unwrap();
        s.append(b"f");
        assert_eq!(s.drop_front(4), 3);
        assert_eq!(s.drop_front(6), 5);
        assert_eq!(s.len(), 6);
        assert_eq!(bytes(&s), b"f");
        assert_eq!(
            s.read_at(4, &mut [0]),
            Err(SegmentError::Dropped { front: 5 })
        );
        assert_eq!(s.truncate(4), Err(SegmentError::Dropped { front: 5 }));
        s.seal();
        assert_eq!(s.drop_front(6), 6);
        assert_eq!((s.held(), s.len()), (0, 6));
        s.append(b"g");
        assert_eq!(bytes(&s), b"g");
    }
}
