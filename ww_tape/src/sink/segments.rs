//! The bytes of a [MemTape](super::MemTape): sealed segments, each one piece or more, and the open segment a sink
//! appends to.
//!
//! Appending grows only the open segment, so a write never copies the pieces before it. A new piece seals the open
//! segment ([Segments::rebase] to the end), and so does a close. [Segments::drop_front] frees the oldest sealed
//! segments and keeps every offset: the dropped bytes are counted in [Segments::front], not shifted out.

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::reader::wide;

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
    /// Bytes dropped before the first segment.
    front: u64,
    /// Sealed segments in order, each with its offset; contiguous from `front` to `open_at`.
    sealed: Vec<(u64, Box<[u8]>)>,
    /// Where the open segment starts.
    open_at: u64,
    open: Vec<u8>,
}

impl Segments {
    /// Offset of the first byte kept.
    pub(super) fn front(&self) -> u64 {
        self.front
    }

    /// Offset of the end: every byte ever written, dropped ones included.
    pub(super) fn len(&self) -> u64 {
        self.open_at.saturating_add(wide(self.open.len()))
    }

    /// Bytes held in memory: [Self::len] less [Self::front].
    pub(super) fn held(&self) -> u64 {
        self.len().saturating_sub(self.front)
    }

    /// Sealed segments and the open one when it holds bytes.
    #[cfg(test)]
    fn count(&self) -> usize {
        self.sealed
            .len()
            .saturating_add(usize::from(!self.open.is_empty()))
    }

    pub(super) fn append(&mut self, bytes: &[u8]) {
        self.open.extend_from_slice(bytes);
    }

    /// Room for `bytes` more in the open segment, in one allocation.
    pub(super) fn reserve(&mut self, bytes: usize) {
        _ = self.open.try_reserve_exact(bytes);
    }

    /// Seal the open segment; the next byte starts a new one.
    pub(super) fn seal(&mut self) {
        if self.open.is_empty() {
            return;
        }
        let bytes = core::mem::take(&mut self.open).into_boxed_slice();
        let at = self.open_at;
        self.open_at = Self::len_of(at, &bytes);
        self.sealed.push((at, bytes));
    }

    /// A piece starts at `base`, at most [Self::len] and not before [Self::front]: the bytes before it are sealed
    /// and the open segment starts there.
    pub(super) fn rebase(&mut self, base: u64) -> Result<(), SegmentError> {
        self.open_from(base)?;
        let split = self.local(base)?;
        let tail = self.open.split_off(split);
        self.seal();
        self.open = tail;
        Ok(())
    }

    /// Drop every byte from `len` on, not before [Self::front].
    pub(super) fn truncate(&mut self, len: u64) -> Result<(), SegmentError> {
        self.open_from(len)?;
        let keep = self.local(len)?;
        self.open.truncate(keep);
        Ok(())
    }

    /// Free the sealed segments that end at or before `to`: the new [Self::front]. The open segment, and a sealed
    /// one `to` falls inside, are kept.
    pub(super) fn drop_front(&mut self, to: u64) -> u64 {
        let n = self
            .sealed
            .iter()
            .take_while(|(at, b)| Self::len_of(*at, b) <= to)
            .count();
        if let Some((at, b)) = self.sealed.drain(..n).next_back() {
            self.front = Self::len_of(at, &b);
        }
        self.front
    }

    /// Fill `buf` from `offset`, across segments.
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
        // the first sealed segment the read touches: the last one starting at or before `offset`
        let first = self
            .sealed
            .partition_point(|(at, _)| *at <= offset)
            .saturating_sub(1);
        let mut at = offset;
        let mut out = buf;
        let sealed = self.sealed.get(first..).unwrap_or_default();
        let segments = sealed
            .iter()
            .map(|(s, b)| (*s, &**b))
            .chain(core::iter::once((self.open_at, self.open.as_slice())));
        for (start, bytes) in segments {
            if out.is_empty() {
                break;
            }
            let Some(skip) = at.checked_sub(start) else {
                return Err(SegmentError::OutOfRange);
            };
            let Some(from) = usize::try_from(skip).ok().and_then(|s| bytes.get(s..)) else {
                continue;
            };
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

    /// Move the sealed segments that end after `at` back into the open segment, so that it starts at or before
    /// `at`.
    fn open_from(&mut self, at: u64) -> Result<(), SegmentError> {
        if at < self.front {
            return Err(SegmentError::Dropped { front: self.front });
        }
        if at > self.len() {
            return Err(SegmentError::OutOfRange);
        }
        if at >= self.open_at {
            return Ok(());
        }
        let k = self
            .sealed
            .partition_point(|(s, b)| Self::len_of(*s, b) <= at);
        let back = self.sealed.split_off(k);
        let Some(&(start, _)) = back.first() else {
            return Ok(());
        };
        let mut open = Vec::new();
        let more = back
            .iter()
            .map(|(_, b)| b.len())
            .fold(self.open.len(), usize::saturating_add);
        _ = open.try_reserve_exact(more);
        for (_, b) in &back {
            open.extend_from_slice(b);
        }
        open.append(&mut self.open);
        self.open = open;
        self.open_at = start;
        Ok(())
    }

    /// `at` as an index into the open segment.
    fn local(&self, at: u64) -> Result<usize, SegmentError> {
        at.checked_sub(self.open_at)
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| *n <= self.open.len())
            .ok_or(SegmentError::OutOfRange)
    }

    /// End of a segment at `at`.
    fn len_of(at: u64, bytes: &[u8]) -> u64 {
        at.saturating_add(wide(bytes.len()))
    }
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
    fn reads_across_segments() {
        let mut s = Segments::default();
        s.append(b"abc");
        s.rebase(3).unwrap();
        s.append(b"de");
        s.rebase(5).unwrap();
        s.append(b"fgh");
        assert_eq!(s.count(), 3);
        let mut buf = [0; 6];
        s.read_at(1, &mut buf).unwrap();
        assert_eq!(&buf, b"bcdefg");
        assert_eq!(s.read_at(7, &mut [0; 2]), Err(SegmentError::OutOfRange));
        assert_eq!(s.read_at(8, &mut []), Ok(()));
    }

    #[test]
    fn rebase_back_reopens_sealed() {
        let mut s = Segments::default();
        s.append(b"abc");
        s.rebase(3).unwrap();
        s.append(b"de");
        s.seal();
        s.rebase(1).unwrap();
        assert_eq!(s.open_at, 1);
        assert_eq!(s.open, b"bcde");
        s.truncate(2).unwrap();
        assert_eq!(bytes(&s), b"ab");
        assert_eq!(s.rebase(3), Err(SegmentError::OutOfRange));
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
        s.truncate(5).unwrap();
        assert_eq!(s.held(), 0);
    }
}
