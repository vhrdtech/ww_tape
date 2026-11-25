//! Bytes tracks: values of any length, each written streamed as parts and read whole with its hash checked.
//!
//! A track of [TrackKind::Bytes](crate::TrackKind::Bytes) holds values made elsewhere and read whole: a document, a
//! firmware image, a design file. A value is written as **parts**, each an events chunk
//! ([BlockKind::Events](crate::BlockKind::Events)) of one item, and ends in a **seal**, a last part holding its length
//! and hash. Nothing reads a value at a byte offset: data that needs seeking is a track, and its index does the
//! seeking.
//!
//! A part's chunk is an events chunk of instants with one item:
//!
//! ```text
//! header count   1
//! first item     the part's number on the track: parts are numbered on from 0 across the track's pieces, as the
//!                items of an events track are
//! first tick, last tick, latest end
//!                the value's tick, the same in every part of a value
//! columns, in this order:
//!   Payloads        the part's bytes, Codec::RAW or an lz4 payload codec; first, so in an aligned file the bytes
//!                   start on the alignment
//!   PayloadLengths  one length, the part's bytes decoded
//!   Starts          one tick
//!   Flags           one flags word, absent when it is 0 (as in every events chunk)
//! ```
//!
//! The flags word marks where a part stands in its value: bit 0 [PART_FIRST] on the first part of a value, bit 1
//! [PART_SEAL] on its seal. A writer writes the other bits 0 and a reader ignores them: they may only ever mark. A
//! future bit that changes how a part reads needs another track kind or the version byte.
//!
//! A seal's Payloads column is [Codec::RAW](crate::Codec::RAW) and [SEAL_LEN] bytes:
//!
//! ```text
//!  0 u64  the value's first part: its item number, the value's id (ValueId)
//!  8 u64  the value's length in bytes
//! 16 [32] BLAKE3 of the value's bytes
//! ```
//!
//! A value of no bytes is its seal alone, marked [PART_FIRST] too. A value is whole when its parts are items
//! numbered one after the other from a part marked [PART_FIRST] to a seal naming that part, with no other part marked
//! [PART_FIRST] between them. Another part marked [PART_FIRST], or the end of the track, before the seal is a torn
//! value: a crash or a write given up (`BytesWriter::abandon`). A reader takes a torn value as absent, never partial
//! ([read_value]).
//!
//! The parts of a value are ordinary chunks, not a run of blocks belonging together: a value has no length limit,
//! the writer places its checkpoints between them, and a cut may fall between two parts, so a value spans pieces.
//!
//! A changed byte fails its segment's CRC, and the error names the part's chunk. A piece written at
//! [Integrity::None](crate::Integrity::None) has no payload CRCs: there only the value's hash catches a changed byte,
//! and the error names the value.

mod read;
#[cfg(test)]
mod tests;
#[cfg(feature = "std")]
mod write;

pub use read::{Absence, PartAt, PartDamage, ValueError, read_value};
#[cfg(feature = "std")]
pub use write::{BytesError, BytesOptions, BytesPart, BytesWriter, WriteValueError};

use crate::chunk::{ChunkHead, ColumnKind, ColumnSpec};
use crate::times::Flags;
use crate::{Codec, EndMode, ItemNum};
use alloc::vec::Vec;

/// Flags bit of the first part of a value.
pub const PART_FIRST: u32 = 1;
/// Flags bit of a value's seal, its last part.
pub const PART_SEAL: u32 = 2;
/// Bytes of a seal's payload: the first part, the length and the hash.
pub const SEAL_LEN: usize = 48;
/// Bytes of the parts a [BytesWriter] writes unless told otherwise (4 MiB).
pub const BYTES_PART_LEN: usize = 4 << 20;
/// Most bytes of one part a [BytesWriter] takes (32 MiB): a part and its chunk fit a block.
pub const BYTES_PART_LEN_MAX: usize = 32 << 20;

const _: () = assert!(
    2 * BYTES_PART_LEN_MAX <= crate::MAX_BLOCK_LEN,
    "a part of BYTES_PART_LEN_MAX bytes and its chunk head fit a block"
);

/// A value on a bytes track: the item number of its first part.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ValueId(pub ItemNum);

/// A value written whole: its id, its length, its BLAKE3 hash and the parts it took, its seal included.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ValueSeal {
    pub id: ValueId,
    pub len: u64,
    pub hash: [u8; 32],
    pub parts: u64,
}

/// A seal's payload: the value's first part, its length and its hash.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct Seal {
    pub first: ItemNum,
    pub len: u64,
    pub hash: [u8; 32],
}

impl Seal {
    /// The seal `bytes` hold; None unless they are [SEAL_LEN] long.
    fn read(bytes: &[u8]) -> Option<Self> {
        let b: &[u8; SEAL_LEN] = bytes.try_into().ok()?;
        let (first, rest) = b.split_first_chunk::<8>()?;
        let (len, hash) = rest.split_first_chunk::<8>()?;
        Some(Self {
            first: ItemNum::new(u64::from_le_bytes(*first)),
            len: u64::from_le_bytes(*len),
            hash: hash.try_into().ok()?,
        })
    }
}

/// Why a chunk is not a part of a bytes track.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PartError {
    /// Not an events chunk of one instant.
    NotOneItem,
    /// Its columns are not Payloads, PayloadLengths, Starts and, when its flags are not 0, Flags, in this order.
    Columns,
    /// Its flags column is not in the flags codec.
    FlagsCodec(Codec),
    /// Its flags column doesn't read.
    Flags(crate::times::ColumnError),
    /// Its time columns don't read.
    Times(crate::times::TimesError),
    /// Its payload columns don't read.
    Payloads(crate::payloads::PayloadsError),
    /// A seal whose Payloads column is not [Codec::RAW] and [SEAL_LEN] bytes, that names a first part after itself,
    /// or that is a first part naming another or holding bytes.
    Seal,
    /// A data part of no bytes.
    Empty,
}

impl core::fmt::Display for PartError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotOneItem => write!(f, "not an events chunk of one instant"),
            Self::Columns => write!(f, "not the columns of a part, in their order"),
            Self::FlagsCodec(c) => write!(f, "a part's flags column in codec {}", c.0),
            Self::Flags(_) => write!(f, "a part's flags column doesn't read"),
            Self::Times(_) => write!(f, "a part's time columns don't read"),
            Self::Payloads(_) => write!(f, "a part's payload columns don't read"),
            Self::Seal => write!(f, "a seal that doesn't read or names a first part it can't"),
            Self::Empty => write!(f, "a data part of no bytes"),
        }
    }
}

impl core::error::Error for PartError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Flags(e) => Some(e),
            Self::Times(e) => Some(e),
            Self::Payloads(e) => Some(e),
            _ => None,
        }
    }
}

/// What a part is to its value: first, a seal, both, or neither.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct PartShape {
    pub first: bool,
    pub seal: Option<Seal>,
}

/// The kinds a part's columns have, in order; the Flags column is left out when the flags are 0.
const PART_COLUMNS: [ColumnKind; 4] = [
    ColumnKind::Payloads,
    ColumnKind::PayloadLengths,
    ColumnKind::Starts,
    ColumnKind::Flags,
];

/// Check the head and the column kinds of a part; whether it has a Flags column.
fn check_layout(head: &ChunkHead, kinds: &[ColumnKind]) -> Result<bool, PartError> {
    if head.kind != crate::BlockKind::Events
        || head.count != 1
        || head.end_mode != EndMode::Absent
        || head.first_tick != head.last_tick
    {
        return Err(PartError::NotOneItem);
    }
    let flagged = match kinds.len() {
        3 => false,
        4 => true,
        _ => return Err(PartError::Columns),
    };
    if kinds.iter().zip(PART_COLUMNS).any(|(k, want)| *k != want) {
        return Err(PartError::Columns);
    }
    Ok(flagged)
}

/// The flags word of a part's one item from its Flags column `column`, the bytes after it in `tail` (at least
/// [PAYLOAD_PADDING](crate::chunk::PAYLOAD_PADDING) of them, the column's own first).
fn flags_of(codec: Codec, tail: &[u8], column_len: usize) -> Result<u32, PartError> {
    if codec != crate::times::FLAGS {
        return Err(PartError::FlagsCodec(codec));
    }
    let column = tail.get(..column_len).ok_or(PartError::Columns)?;
    let f = Flags::parse(1, column, tail, false).map_err(PartError::Flags)?;
    Ok(f.get(0))
}

/// The shape of a part from its flags and, for a seal, its Payloads column's codec and bytes: a seal names a first
/// part at or before itself, and a first part that is a seal names itself and holds no bytes.
fn shape_of(
    item: ItemNum,
    flags: u32,
    payload_codec: Codec,
    payload: Option<&[u8]>,
) -> Result<PartShape, PartError> {
    let first = flags & PART_FIRST != 0;
    if flags & PART_SEAL == 0 {
        return Ok(PartShape { first, seal: None });
    }
    if payload_codec != Codec::RAW {
        return Err(PartError::Seal);
    }
    let Some(payload) = payload else {
        return Ok(PartShape { first, seal: None });
    };
    let seal = Seal::read(payload).ok_or(PartError::Seal)?;
    if seal.first > item || (first && (seal.first != item || seal.len != 0)) {
        return Err(PartError::Seal);
    }
    Ok(PartShape {
        first,
        seal: Some(seal),
    })
}

/// Check a part a writer is given as a head and its columns, as [crate::TapeWriter::write_chunk] takes them.
pub(crate) fn check_part_columns(
    head: &ChunkHead,
    columns: &[(ColumnSpec, &[u8])],
) -> Result<PartShape, PartError> {
    let kinds: Vec<ColumnKind> = columns.iter().map(|(c, _)| c.kind).collect();
    let flagged = check_layout(head, &kinds)?;
    let flags = match columns.get(3) {
        Some((spec, bytes)) if flagged => {
            let mut tail =
                Vec::with_capacity(bytes.len().saturating_add(crate::chunk::PAYLOAD_PADDING));
            tail.extend_from_slice(bytes);
            tail.resize(bytes.len().saturating_add(crate::chunk::PAYLOAD_PADDING), 0);
            flags_of(spec.codec, &tail, bytes.len())?
        }
        _ => 0,
    };
    let (spec, payload) = columns.first().ok_or(PartError::Columns)?;
    let shape = shape_of(head.first_item, flags, spec.codec, Some(payload))?;
    if shape.seal.is_none() && payload.is_empty() {
        return Err(PartError::Empty);
    }
    Ok(shape)
}

/// Check a chunk read whole as a part; its shape and its bytes, decoded.
pub(crate) fn read_part<'c>(
    chunk: &crate::Chunk<'c>,
) -> Result<(PartShape, crate::payloads::Payloads<'c>), PartError> {
    let head = chunk.head();
    let kinds: Vec<ColumnKind> = chunk.columns().map(|c| c.kind).collect();
    check_layout(&head, &kinds)?;
    let times = crate::times::EventTimes::read(chunk).map_err(PartError::Times)?;
    let flags = times.flag(0);
    let payload = chunk
        .find(ColumnKind::Payloads, 0)
        .ok_or(PartError::Columns)?;
    let shape = shape_of(head.first_item, flags, payload.codec, Some(payload.bytes))?;
    let payloads = crate::payloads::Payloads::read(chunk)
        .map_err(PartError::Payloads)?
        .ok_or(PartError::Columns)?;
    if shape.seal.is_none() && payloads.get(0).is_none_or(<[u8]>::is_empty) {
        return Err(PartError::Empty);
    }
    Ok((shape, payloads))
}
