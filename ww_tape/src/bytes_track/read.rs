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

/// A part found by the first pass.
struct Found {
    reader: usize,
    piece: usize,
    entry: IndexEntry,
}

type VE<S, K> = ValueError<<S as ByteSource>::Error, K>;

/// Read value `value` of bytes track `track` whole into `sink`, its length and hash checked: its id, length, hash
/// and parts as its seal states them.
///
/// `pieces` are readers of the pieces of the track's chain in order: one reader of a file holding every piece, or
/// one per piece file. Parts are found by their numbers in each piece's index, never by a byte offset.
///
/// A first pass reads only each part's head and flags and finds the seal: a value torn or missing, or with parts
/// missing between its first and its seal, is [ValueError::Absent] before any byte goes to the sink. The second pass
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
    let (parts, seal) = find_parts(pieces, track, value, &mut buf)?;
    let mut hasher = blake3::Hasher::new();
    let mut len = 0u64;
    for f in &parts {
        let reader = pieces.get(f.reader).ok_or(ValueError::Absent {
            value,
            why: Absence::NotFound,
        })?;
        let at = PartAt {
            reader: f.reader,
            piece: f.piece,
            offset: f.entry.offset,
            part: f.entry.first_item,
            value_offset: Some(len),
        };
        let damaged = |cause| ValueError::Damaged { at, cause };
        let block = reader
            .read_block(f.piece, &f.entry, &mut buf)
            .map_err(|e| damaged(PartDamage::Read(e)))?;
        let chunk = block
            .chunk()
            .ok_or(damaged(PartDamage::Part(PartError::NotOneItem)))?;
        if !f.entry.names_chunk(&chunk.head()) {
            return Err(damaged(PartDamage::Changed));
        }
        let (shape, payloads) = read_part(&chunk).map_err(|e| damaged(PartDamage::Part(e)))?;
        if let Some(s) = shape.seal {
            if s != seal {
                return Err(damaged(PartDamage::Changed));
            }
            break;
        }
        let bytes = payloads
            .get(0)
            .ok_or(damaged(PartDamage::Part(PartError::Empty)))?;
        hasher.update(bytes);
        len = len.saturating_add(crate::chunk::len_u64(bytes.len()));
        sink(bytes).map_err(ValueError::Sink)?;
    }
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
        parts: crate::chunk::len_u64(parts.len()),
    })
}

/// The first pass: the parts of `value` from its first to its seal, each found by its number and read only as far
/// as its head, flags and seal, and the seal.
fn find_parts<S: ByteSource, K>(
    pieces: &[&TapeReader<S>],
    track: TrackId,
    value: ValueId,
    buf: &mut Vec<u8>,
) -> Result<(Vec<Found>, Seal), VE<S, K>> {
    let absent = |why| ValueError::Absent { value, why };
    let mut parts: Vec<Found> = Vec::new();
    let mut expected = value.0;
    for (r, reader) in pieces.iter().enumerate() {
        for i in 0..reader.pieces().len() {
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
                    return Err(absent(if parts.is_empty() {
                        Absence::NotFound
                    } else {
                        Absence::Gap {
                            missing: expected.get()..entry.first_item.get(),
                        }
                    }));
                }
                let at = PartAt {
                    reader: r,
                    piece: i,
                    offset: entry.offset,
                    part: entry.first_item,
                    value_offset: None,
                };
                let shape = probe(reader, i, &entry, buf)
                    .map_err(|cause| ValueError::Damaged { at, cause })?;
                if parts.is_empty() && !shape.first {
                    return Err(absent(Absence::NotFirst));
                }
                if !parts.is_empty() && shape.first {
                    return Err(absent(Absence::Torn {
                        parts: crate::chunk::len_u64(parts.len()),
                    }));
                }
                parts.push(Found {
                    reader: r,
                    piece: i,
                    entry,
                });
                if let Some(seal) = shape.seal {
                    if seal.first != value.0 {
                        return Err(ValueError::Damaged {
                            at,
                            cause: PartDamage::Part(PartError::Seal),
                        });
                    }
                    return Ok((parts, seal));
                }
                expected = expected.checked_add(1).ok_or(absent(Absence::Torn {
                    parts: crate::chunk::len_u64(parts.len()),
                }))?;
            }
        }
    }
    Err(absent(if parts.is_empty() {
        Absence::NotFound
    } else {
        Absence::Torn {
            parts: crate::chunk::len_u64(parts.len()),
        }
    }))
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
