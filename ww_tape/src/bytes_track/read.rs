//! Reading a value of a bytes track whole, its hash checked.

use super::{
    PartError, PartShape, Seal, ValueId, ValueSeal, check_layout, flags_of, read_part, shape_of,
};
use crate::chunk::{ChunkHeadError, ColumnKind, PAYLOAD_PADDING, head_columns};
use crate::index::IndexEntry;
use crate::reader::{ByteSource, ReadError, TapeReader};
use crate::{BlockKind, ItemNum, TrackId};
use alloc::vec::Vec;

/// Where a part lies: the reader among those given, its piece, the block's offset in the piece, the part's number.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PartAt {
    pub reader: usize,
    pub piece: usize,
    pub offset: u64,
    pub part: ItemNum,
    /// Where the part's bytes start in the value; None when the value's parts were not yet read that far.
    pub value_offset: Option<u64>,
}

/// Why a value reads as absent.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Absence {
    /// No part of that number is in the pieces given.
    NotFound,
    /// The part of that number doesn't start a value.
    NotFirst,
    /// The value's parts end before its seal: another value starts, or the track ends. A crash or a write given up.
    Torn { parts: u64 },
    /// Parts of the value are missing from the pieces given: a piece removed on purpose, or lost.
    Gap { missing: core::ops::Range<u64> },
}

/// What is wrong with a part's chunk.
#[derive(Debug)]
#[non_exhaustive]
pub enum PartDamage<E> {
    /// The block doesn't read: a bad CRC, a bad head, the source failed.
    Read(ReadError<E>),
    /// The chunk's head names columns that don't read.
    Head(ChunkHeadError),
    /// The chunk is not a part as the format says.
    Part(PartError),
    /// The block read whole is not the chunk its index entry and first read named.
    Changed,
}

/// Why a value doesn't read whole.
#[derive(Debug)]
#[non_exhaustive]
pub enum ValueError<E, K> {
    /// The value is not there whole: nothing of it went to the sink.
    Absent { value: ValueId, why: Absence },
    /// A piece's index entries of the track don't read.
    Index {
        reader: usize,
        piece: usize,
        error: ReadError<E>,
    },
    /// A part's chunk is damaged, named.
    Damaged { at: PartAt, cause: PartDamage<E> },
    /// The value's bytes don't have the length or the hash its seal states: a changed byte the CRCs didn't catch (a
    /// piece written without payload CRCs).
    Mismatch {
        value: ValueId,
        stated_len: u64,
        len: u64,
        hash_matches: bool,
    },
    /// The sink failed.
    Sink(K),
}

impl<E: core::fmt::Debug, K: core::fmt::Debug> core::fmt::Display for ValueError<E, K> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Absent { value, why } => write!(f, "value {} absent: {why:?}", value.0),
            Self::Index { reader, piece, .. } => {
                write!(f, "index of reader {reader}, piece {piece} doesn't read")
            }
            Self::Damaged { at, cause } => write!(
                f,
                "part {} of reader {}, piece {}, at {} damaged: {cause:?}",
                at.part, at.reader, at.piece, at.offset
            ),
            Self::Mismatch {
                value,
                stated_len,
                len,
                hash_matches,
            } => write!(
                f,
                "value {}: {len} bytes read, {stated_len} stated, hash {}",
                value.0,
                if *hash_matches { "matches" } else { "differs" }
            ),
            Self::Sink(e) => write!(f, "the sink failed: {e:?}"),
        }
    }
}

impl<E: core::fmt::Debug, K: core::fmt::Debug> core::error::Error for ValueError<E, K> {}

/// Where a part lies: its reader, its piece and its index entry.
#[derive(Copy, Clone, Debug)]
struct Found {
    reader: usize,
    piece: usize,
    entry: IndexEntry,
}

impl Found {
    fn at(&self, value_offset: Option<u64>) -> PartAt {
        PartAt {
            reader: self.reader,
            piece: self.piece,
            offset: self.entry.offset,
            part: self.entry.first_item,
            value_offset,
        }
    }
}

/// What the first pass found of a value: its first part, its seal's part and the seal.
struct Span {
    first: Found,
    last: Found,
    seal: Seal,
}

type VE<S, K> = ValueError<<S as ByteSource>::Error, K>;

/// Read value `value` of bytes track `track` whole into `sink`, its length and hash checked: its id, length, hash
/// and parts as its seal states them.
///
/// `pieces` are readers of the pieces of the track's chain in order: one reader of a file holding every piece, or
/// one per piece file. Parts are found by their numbers in each piece's index, never by a byte offset. A piece whose
/// close or whose next piece's head says the track's items end at or before the value is skipped unread.
///
/// A first pass reads only each part's head and flags and finds the seal: a value torn or missing, or with parts
/// missing between its first and its seal, is [ValueError::Absent] before any byte goes to the sink. The parts that
/// decide an absence are read whole first, their CRCs checked: a damaged one is [ValueError::Damaged], not an
/// absence. A value whose parts run on into a piece not given reads as [Absence::Gap] when a later piece's head says
/// where the track's items go on; with no piece after it, as [Absence::Torn]. The second pass walks the index again,
/// reads each part whole, its CRCs checked as the reader checks payloads, and hands its bytes to `sink`. A part that
/// doesn't read is [ValueError::Damaged], named; bytes that don't match the seal are [ValueError::Mismatch]. Either
/// comes after the sink took bytes: its caller drops them.
pub fn read_value<S: ByteSource, K>(
    pieces: &[&TapeReader<S>],
    track: TrackId,
    value: ValueId,
    mut sink: impl FnMut(&[u8]) -> Result<(), K>,
) -> Result<ValueSeal, VE<S, K>> {
    let mut buf = Vec::new();
    let span = find_parts(pieces, track, value, &mut buf)?;
    let seal_item = span.last.entry.first_item;
    let mut hasher = blake3::Hasher::new();
    let mut len = 0u64;
    let mut parts = 0u64;
    let mut expected = value.0;
    for (r, reader) in pieces.iter().enumerate().skip(span.first.reader) {
        let from = if r == span.first.reader {
            span.first.piece
        } else {
            0
        };
        for i in from..reader.pieces().len() {
            if (r, i) > (span.last.reader, span.last.piece) {
                break;
            }
            let entries = reader
                .track_entries(i, track)
                .map_err(|error| ValueError::Index {
                    reader: r,
                    piece: i,
                    error,
                })?;
            let ours = entries
                .into_iter()
                .filter(|e| e.kind == BlockKind::Events && e.first_item <= seal_item);
            for entry in ours {
                if entry.first_item < expected {
                    continue;
                }
                let f = Found {
                    reader: r,
                    piece: i,
                    entry,
                };
                let at = f.at(Some(len));
                let damaged = |cause| ValueError::Damaged { at, cause };
                // the index read the same as in the first pass: its parts follow on
                if entry.first_item != expected {
                    return Err(damaged(PartDamage::Changed));
                }
                let block = reader
                    .read_block(i, &entry, &mut buf)
                    .map_err(|e| damaged(PartDamage::Read(e)))?;
                let chunk = block
                    .chunk()
                    .ok_or(damaged(PartDamage::Part(PartError::NotOneItem)))?;
                if !entry.names_chunk(&chunk.head()) {
                    return Err(damaged(PartDamage::Changed));
                }
                let (shape, payloads) =
                    read_part(&chunk).map_err(|e| damaged(PartDamage::Part(e)))?;
                parts = parts.saturating_add(1);
                if let Some(s) = shape.seal {
                    if s != span.seal || entry.first_item != seal_item {
                        return Err(damaged(PartDamage::Changed));
                    }
                    return finish(value, span.seal, len, parts, &hasher);
                }
                let bytes = payloads
                    .get(0)
                    .ok_or(damaged(PartDamage::Part(PartError::Empty)))?;
                hasher.update(bytes);
                len = len.saturating_add(crate::chunk::len_u64(bytes.len()));
                sink(bytes).map_err(ValueError::Sink)?;
                expected = expected
                    .checked_add(1)
                    .ok_or(damaged(PartDamage::Changed))?;
            }
        }
    }
    // the seal the first pass found is gone: the source changed between the passes
    Err(ValueError::Damaged {
        at: span.last.at(Some(len)),
        cause: PartDamage::Changed,
    })
}

/// The value read whole, `len` bytes hashed by `hasher` in `parts` parts, checked against its seal.
fn finish<E, K>(
    value: ValueId,
    seal: Seal,
    len: u64,
    parts: u64,
    hasher: &blake3::Hasher,
) -> Result<ValueSeal, ValueError<E, K>> {
    let hash = *hasher.finalize().as_bytes();
    if len != seal.len || hash != seal.hash {
        return Err(ValueError::Mismatch {
            value,
            stated_len: seal.len,
            len,
            hash_matches: hash == seal.hash,
        });
    }
    Ok(ValueSeal {
        id: value,
        len,
        hash,
        parts,
    })
}

/// Whether piece `i` of `reader` holds no part numbered `value` or later: its close says the track's items end at or
/// before it, or the next piece's head says they go on from at or before it.
fn ends_before<S: ByteSource>(
    reader: &TapeReader<S>,
    i: usize,
    track: TrackId,
    value: ItemNum,
) -> bool {
    let infos = reader.pieces();
    let closed = infos
        .get(i)
        .and_then(|p| p.close.as_ref())
        .is_some_and(|(_, rows)| {
            rows.iter()
                .any(|t| t.track_id == track && t.chunks > 0 && t.end_item() <= value)
        });
    closed || next_item_at(reader, i.saturating_add(1), track).is_some_and(|n| n <= value)
}

/// Where piece `i`'s head says track `track`'s items go on; None without a head or a row of the track.
fn next_item_at<S: ByteSource>(
    reader: &TapeReader<S>,
    i: usize,
    track: TrackId,
) -> Option<ItemNum> {
    let (_, rows) = reader.pieces().get(i)?.head.as_ref()?;
    rows.iter()
        .find(|t| t.track_id == track)
        .map(|t| t.next_item)
}

/// Read part `f` whole, its CRCs checked: what it is to its value. A part that decides an absence is read so, since
/// [probe] reads its flags unchecked.
fn confirm<S: ByteSource, K>(
    pieces: &[&TapeReader<S>],
    f: &Found,
    probed: PartShape,
    buf: &mut Vec<u8>,
) -> Result<(), VE<S, K>> {
    let damaged = |cause| ValueError::Damaged {
        at: f.at(None),
        cause,
    };
    let reader = pieces.get(f.reader).ok_or(damaged(PartDamage::Changed))?;
    let block = reader
        .read_block(f.piece, &f.entry, buf)
        .map_err(|e| damaged(PartDamage::Read(e)))?;
    let chunk = block
        .chunk()
        .ok_or(damaged(PartDamage::Part(PartError::NotOneItem)))?;
    if !f.entry.names_chunk(&chunk.head()) {
        return Err(damaged(PartDamage::Changed));
    }
    let (shape, _) = read_part(&chunk).map_err(|e| damaged(PartDamage::Part(e)))?;
    if shape != probed {
        return Err(damaged(PartDamage::Changed));
    }
    Ok(())
}

/// The first pass: the first part of `value`, its seal's part and the seal, each part found by its number and read
/// only as far as its head, flags and seal. Only the parts that decide are kept: memory doesn't grow with the value.
fn find_parts<S: ByteSource, K>(
    pieces: &[&TapeReader<S>],
    track: TrackId,
    value: ValueId,
    buf: &mut Vec<u8>,
) -> Result<Span, VE<S, K>> {
    let absent = |why| ValueError::Absent { value, why };
    // the first part and the last part read, with its shape
    let mut first: Option<Found> = None;
    let mut last: Option<(Found, PartShape)> = None;
    let mut parts = 0u64;
    let mut expected = value.0;
    for (r, reader) in pieces.iter().enumerate() {
        for i in 0..reader.pieces().len() {
            if first.is_none() && ends_before(reader, i, track, value.0) {
                continue;
            }
            // the head says the track went on past the parts read: they lie in pieces not given
            if first.is_some()
                && let Some(next) = next_item_at(reader, i, track)
                && next > expected
            {
                return Err(absent(Absence::Gap {
                    missing: expected.get()..next.get(),
                }));
            }
            let entries = reader
                .track_entries(i, track)
                .map_err(|error| ValueError::Index {
                    reader: r,
                    piece: i,
                    error,
                })?;
            for entry in entries.into_iter().filter(|e| e.kind == BlockKind::Events) {
                if entry.first_item < expected {
                    continue;
                }
                if entry.first_item > expected {
                    return Err(absent(if first.is_none() {
                        Absence::NotFound
                    } else {
                        Absence::Gap {
                            missing: expected.get()..entry.first_item.get(),
                        }
                    }));
                }
                let f = Found {
                    reader: r,
                    piece: i,
                    entry,
                };
                let shape = probe(reader, i, &entry, buf).map_err(|cause| ValueError::Damaged {
                    at: f.at(None),
                    cause,
                })?;
                if first.is_none() && !shape.first {
                    confirm(pieces, &f, shape, buf)?;
                    return Err(absent(Absence::NotFirst));
                }
                if first.is_some() && shape.first {
                    // the part before ends the value only if its seal flag was lost: read both whole
                    if let Some((before, before_shape)) = &last {
                        confirm(pieces, before, *before_shape, buf)?;
                    }
                    confirm(pieces, &f, shape, buf)?;
                    return Err(absent(Absence::Torn { parts }));
                }
                first.get_or_insert(f);
                parts = parts.saturating_add(1);
                if let Some(seal) = shape.seal {
                    if seal.first != value.0 {
                        return Err(ValueError::Damaged {
                            at: f.at(None),
                            cause: PartDamage::Part(PartError::Seal),
                        });
                    }
                    let first = first.unwrap_or(f);
                    return Ok(Span {
                        first,
                        last: f,
                        seal,
                    });
                }
                last = Some((f, shape));
                expected = expected
                    .checked_add(1)
                    .ok_or(absent(Absence::Torn { parts }))?;
            }
        }
    }
    match last {
        None => Err(absent(Absence::NotFound)),
        Some((before, shape)) => {
            // the track ends before a seal: the last part read decides, unless its seal flag was lost
            confirm(pieces, &before, shape, buf)?;
            Err(absent(Absence::Torn { parts }))
        }
    }
}

/// Read a part's head, its flags and, for a seal, its bytes: what it is to its value. Its payload's CRCs are not
/// checked here; the second pass reads it whole.
fn probe<S: ByteSource>(
    reader: &TapeReader<S>,
    i: usize,
    entry: &IndexEntry,
    buf: &mut Vec<u8>,
) -> Result<PartShape, PartDamage<S::Error>> {
    let head = reader
        .read_chunk_head(i, entry, buf)
        .map_err(PartDamage::Read)?;
    // the entry's length is the block's: read_chunk_head checked the head against it
    let Ok(block_len) = usize::try_from(entry.len) else {
        return Err(PartDamage::Changed);
    };
    let columns = head_columns(buf, block_len).map_err(PartDamage::Head)?;
    let kinds: Vec<ColumnKind> = columns.iter().map(|c| c.kind).collect();
    let flagged = check_layout(&head, &kinds).map_err(PartDamage::Part)?;
    let flags = match columns.get(3) {
        Some(c) if flagged => {
            // the padding after the last column is in the block
            let column_len = c.range.len();
            buf.resize(column_len.saturating_add(PAYLOAD_PADDING), 0);
            reader
                .read_in_block(i, entry, c.range.start, buf)
                .map_err(PartDamage::Read)?;
            flags_of(c.codec, buf, column_len).map_err(PartDamage::Part)?
        }
        _ => 0,
    };
    let payload = columns
        .first()
        .ok_or(PartDamage::Part(PartError::Columns))?;
    if flags & super::PART_SEAL == 0 {
        return shape_of(head.first_item, flags, payload.codec, None).map_err(PartDamage::Part);
    }
    // a seal's payload is SEAL_LEN bytes, else damage
    if payload.range.len() != super::SEAL_LEN {
        return Err(PartDamage::Part(PartError::Seal));
    }
    buf.resize(super::SEAL_LEN, 0);
    reader
        .read_in_block(i, entry, payload.range.start, buf)
        .map_err(PartDamage::Read)?;
    shape_of(head.first_item, flags, payload.codec, Some(buf)).map_err(PartDamage::Part)
}

impl<S: ByteSource> TapeReader<S> {
    /// Read value `value` of bytes track `track` whole from the pieces of this source, as [read_value] reads it.
    pub fn read_value<K>(
        &self,
        track: TrackId,
        value: ValueId,
        sink: impl FnMut(&[u8]) -> Result<(), K>,
    ) -> Result<ValueSeal, VE<S, K>> {
        read_value(&[self], track, value, sink)
    }
}
