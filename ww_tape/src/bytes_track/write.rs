//! Writing values of a bytes track as parts.

use super::{
    BYTES_PART_LEN, BYTES_PART_LEN_MAX, PART_FIRST, PART_SEAL, SEAL_LEN, Seal, ValueId, ValueSeal,
};
use crate::chunk::{ChunkHead, ColumnKind, ColumnSpec};
use crate::open::Definitions;
use crate::payloads::{EncodeError as PayloadsEncodeError, EncodedPayloads, PayloadCodec};
use crate::times::{EncodeError as TimesEncodeError, EncodedTimes};
use crate::{Codec, ItemNum, TrackId, TrackNum};
use alloc::vec::Vec;

/// How a [BytesWriter] cuts and codes parts.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BytesOptions {
    /// Bytes of each part but a value's last, 1 to [BYTES_PART_LEN_MAX].
    pub part_len: usize,
    /// The codec of the parts' Payloads columns; a seal's is always [Codec::RAW].
    pub codec: PayloadCodec,
}

impl Default for BytesOptions {
    /// Parts of [BYTES_PART_LEN] bytes, [Codec::RAW].
    fn default() -> Self {
        Self {
            part_len: BYTES_PART_LEN,
            codec: PayloadCodec::Raw,
        }
    }
}

/// Why a [BytesWriter] refused a call or a part.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BytesError {
    /// The track is not held, or not a bytes track.
    NotBytes { track: TrackNum },
    /// A part length of 0 or past [BYTES_PART_LEN_MAX].
    PartLen { len: usize, max: usize },
    /// [BytesWriter::begin] while a value is open.
    ValueOpen,
    /// [BytesWriter::write] or [BytesWriter::finish] with no value open.
    NoValue,
    /// The part numbers ran past `u64`.
    PartsSpent,
    /// The payload columns of a part didn't encode.
    Payloads(PayloadsEncodeError),
    /// The times of a part didn't encode.
    Times(TimesEncodeError),
}

impl core::fmt::Display for BytesError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotBytes { track } => write!(f, "track {track} is not a held bytes track"),
            Self::PartLen { len, max } => write!(f, "a part of {len} bytes, not 1 to {max}"),
            Self::ValueOpen => write!(f, "a value is open"),
            Self::NoValue => write!(f, "no value is open"),
            Self::PartsSpent => write!(f, "part numbers past u64"),
            Self::Payloads(_) => write!(f, "a part's payload columns don't encode"),
            Self::Times(_) => write!(f, "a part's times don't encode"),
        }
    }
}

impl core::error::Error for BytesError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Payloads(e) => Some(e),
            Self::Times(e) => Some(e),
            _ => None,
        }
    }
}

/// One part to write: its chunk head and columns, for [TapeWriter::write_chunk](crate::TapeWriter::write_chunk) or a
/// mux's chunk sender.
#[derive(Clone, Debug)]
pub struct BytesPart<'a> {
    pub head: ChunkHead,
    columns: [(ColumnSpec, &'a [u8]); 4],
    n: usize,
}

impl<'a> BytesPart<'a> {
    /// The part's columns in the order they are written.
    pub fn columns(&self) -> &[(ColumnSpec, &'a [u8])] {
        self.columns.get(..self.n).unwrap_or_default()
    }
}

/// The value a [BytesWriter] is writing.
#[derive(Clone, Debug)]
struct Open {
    first: ItemNum,
    ticks: i64,
    len: u64,
    hasher: blake3::Hasher,
    /// No part of the value written yet: the next one is marked [PART_FIRST].
    unstarted: bool,
}

/// Cuts values into the parts of one bytes track, on the producer's side: it holds no writer state, so its parts go
/// to a [TapeWriter](crate::TapeWriter) or a mux's chunk sender alike.
///
/// [Self::begin] opens a value, [Self::write] takes its bytes and hands out each full part, [Self::finish] hands out
/// the last part and the seal. One value is open at a time. A part is handed to `out` as it is made; when `out`
/// fails, the value is torn and reads as absent: [Self::abandon] it.
#[derive(Clone, Debug)]
pub struct BytesWriter {
    track: TrackNum,
    track_id: TrackId,
    options: BytesOptions,
    next_part: ItemNum,
    open: Option<Open>,
    /// The bytes of the open value not yet in a part, fewer than a part.
    buf: Vec<u8>,
    /// The payloads column of the last part, kept to reuse.
    spare: Vec<u8>,
}

impl BytesWriter {
    /// A writer of the values of bytes track `track` held in `defs`, its next part numbered `next_part`: 0 on a new
    /// track, else the item number after the track's last part (a resumed chain's).
    pub fn new(
        defs: &Definitions,
        track: TrackNum,
        next_part: ItemNum,
        options: BytesOptions,
    ) -> Result<Self, BytesError> {
        if !defs.track(track).is_some_and(|t| t.bytes) {
            return Err(BytesError::NotBytes { track });
        }
        let (track_id, _) = defs
            .track_identity(track)
            .ok_or(BytesError::NotBytes { track })?;
        if options.part_len == 0 || options.part_len > BYTES_PART_LEN_MAX {
            return Err(BytesError::PartLen {
                len: options.part_len,
                max: BYTES_PART_LEN_MAX,
            });
        }
        Ok(Self {
            track,
            track_id,
            options,
            next_part,
            open: None,
            buf: Vec::new(),
            spare: Vec::new(),
        })
    }

    /// The track the writer writes.
    pub fn track(&self) -> TrackNum {
        self.track
    }

    /// The number the next part gets.
    pub fn next_part(&self) -> ItemNum {
        self.next_part
    }

    /// Whether a value is open.
    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// Open a value at `ticks` of the track's clock; its id is the number its first part gets.
    pub fn begin(&mut self, ticks: i64) -> Result<ValueId, BytesError> {
        if self.open.is_some() {
            return Err(BytesError::ValueOpen);
        }
        self.buf.clear();
        self.open = Some(Open {
            first: self.next_part,
            ticks,
            len: 0,
            hasher: blake3::Hasher::new(),
            unstarted: true,
        });
        Ok(ValueId(self.next_part))
    }

    /// Give up the open value: nothing more of it is written, and the parts already written read as a torn value,
    /// absent.
    pub fn abandon(&mut self) {
        self.open = None;
        self.buf.clear();
    }

    /// Take `bytes` of the open value, handing each part that fills to `out`.
    pub fn write<E: From<BytesError>>(
        &mut self,
        mut bytes: &[u8],
        mut out: impl FnMut(BytesPart<'_>) -> Result<(), E>,
    ) -> Result<(), E> {
        let open = self.open.as_mut().ok_or(BytesError::NoValue)?;
        open.hasher.update(bytes);
        open.len = open
            .len
            .checked_add(crate::chunk::len_u64(bytes.len()))
            .ok_or(BytesError::PartsSpent)?;
        let part_len = self.options.part_len;
        while !bytes.is_empty() {
            if self.buf.is_empty() && bytes.len() >= part_len {
                // a whole part of the caller's bytes: written without a copy
                let (part, rest) = bytes.split_at_checked(part_len).unwrap_or((bytes, &[]));
                self.emit(DataOf::Given(part), 0, &mut out)?;
                bytes = rest;
                continue;
            }
            let room = part_len.saturating_sub(self.buf.len());
            let (now, rest) = bytes.split_at_checked(room).unwrap_or((bytes, &[]));
            self.buf.extend_from_slice(now);
            bytes = rest;
            if self.buf.len() == part_len {
                self.emit(DataOf::Buffered, 0, &mut out)?;
            }
        }
        Ok(())
    }

    /// Close the open value: hand its last part and its seal to `out`. Its id, length, hash and parts.
    pub fn finish<E: From<BytesError>>(
        &mut self,
        mut out: impl FnMut(BytesPart<'_>) -> Result<(), E>,
    ) -> Result<ValueSeal, E> {
        if self.open.is_none() {
            return Err(BytesError::NoValue.into());
        }
        if !self.buf.is_empty() {
            self.emit(DataOf::Buffered, 0, &mut out)?;
        }
        let open = self.open.as_ref().ok_or(BytesError::NoValue)?;
        let hash = *open.hasher.finalize().as_bytes();
        let seal = Seal {
            first: open.first,
            len: open.len,
            hash,
        };
        let bytes = seal.to_bytes();
        let (first, len) = (open.first, open.len);
        self.emit(DataOf::Seal(&bytes), PART_SEAL, &mut out)?;
        self.open = None;
        let parts = self.next_part.get().saturating_sub(first.get());
        Ok(ValueSeal {
            id: ValueId(first),
            len,
            hash,
            parts,
        })
    }

    /// Encode one part of the open value from `data` with flags `flags` (and [PART_FIRST] on its first part) and
    /// hand it to `out`; the buffer is emptied when it was the part's bytes.
    fn emit<E: From<BytesError>>(
        &mut self,
        data: DataOf<'_>,
        flags: u32,
        out: &mut impl FnMut(BytesPart<'_>) -> Result<(), E>,
    ) -> Result<(), E> {
        let open = self.open.as_mut().ok_or(BytesError::NoValue)?;
        let flags = if open.unstarted {
            flags | PART_FIRST
        } else {
            flags
        };
        let (bytes, codec): (&[u8], _) = match data {
            DataOf::Given(b) => (b, self.options.codec),
            DataOf::Buffered => (&self.buf, self.options.codec),
            DataOf::Seal(b) => (b, PayloadCodec::Raw),
        };
        let item = self.next_part;
        let next = item.checked_add(1).ok_or(BytesError::PartsSpent)?;
        // a part is at most BYTES_PART_LEN_MAX bytes (Self::new), a seal SEAL_LEN
        let Ok(len) = u32::try_from(bytes.len()) else {
            return Err(BytesError::PartLen {
                len: bytes.len(),
                max: BYTES_PART_LEN_MAX,
            }
            .into());
        };
        let times = EncodedTimes::encode(
            &[open.ticks],
            None,
            (flags != 0).then_some(core::slice::from_ref(&flags)),
        )
        .map_err(BytesError::Times)?;
        // a raw part's bytes go into the chunk as they are; a coded one's into the spare buffer
        let coded = match codec {
            PayloadCodec::Raw | PayloadCodec::Named(Codec::RAW) => None,
            _ => {
                let spare = core::mem::take(&mut self.spare);
                Some(
                    EncodedPayloads::from_parts(codec, &[len], bytes, spare)
                        .map_err(BytesError::Payloads)?,
                )
            }
        };
        let raw_lengths;
        let [lengths, payload] = if let Some(p) = &coded {
            p.columns()
        } else {
            raw_lengths =
                ww_tape_codecs::lengths::encode_lengths(&[len]).ok_or(BytesError::PartLen {
                    len: bytes.len(),
                    max: BYTES_PART_LEN_MAX,
                })?;
            let spec = |kind, codec, b: &[u8]| ColumnSpec {
                kind,
                codec,
                param: 0,
                len: b.len(),
            };
            [
                (
                    spec(
                        ColumnKind::PayloadLengths,
                        crate::payloads::LENGTHS,
                        &raw_lengths,
                    ),
                    &raw_lengths[..],
                ),
                (spec(ColumnKind::Payloads, Codec::RAW, bytes), bytes),
            ]
        };
        let head = times.head.chunk_head(self.track_id, self.track, item);
        let empty = ColumnSpec {
            kind: ColumnKind::Other(0),
            codec: Codec::RAW,
            param: 0,
            len: 0,
        };
        let mut columns = [(empty, &[][..]); 4];
        let mut n = 0usize;
        for (slot, c) in columns
            .iter_mut()
            .zip([payload, lengths].into_iter().chain(times.columns()))
        {
            *slot = c;
            n = n.saturating_add(1);
        }
        out(BytesPart { head, columns, n })?;
        if let Some(p) = coded {
            let [_, (_, spare)] = p.into_columns();
            self.spare = spare;
        }
        open.unstarted = false;
        self.next_part = next;
        if matches!(data, DataOf::Buffered) {
            self.buf.clear();
        }
        Ok(())
    }
}

/// Why [BytesWriter::write_from] wrote no whole value.
#[derive(Debug)]
#[non_exhaustive]
pub enum WriteValueError<E> {
    /// The value's parts were refused or not cut.
    Bytes(BytesError),
    /// A part was not written.
    Write(E),
    /// The source failed.
    Source(std::io::Error),
}

impl<E> From<BytesError> for WriteValueError<E> {
    fn from(e: BytesError) -> Self {
        Self::Bytes(e)
    }
}

impl<E: core::fmt::Debug> core::fmt::Display for WriteValueError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Bytes(e) => write!(f, "{e}"),
            Self::Write(e) => write!(f, "a part not written: {e:?}"),
            Self::Source(e) => write!(f, "the source failed: {e}"),
        }
    }
}

impl<E: core::fmt::Debug> core::error::Error for WriteValueError<E> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Bytes(e) => Some(e),
            Self::Write(_) => None,
            Self::Source(e) => Some(e),
        }
    }
}

impl BytesWriter {
    /// Write the bytes `src` gives, to its end, as one value at `ticks`, each part handed to `put`: a
    /// [TapeWriter::write_chunk](crate::TapeWriter::write_chunk) or a chain's. A failure leaves the value torn: it is
    /// given up ([Self::abandon]) and reads as absent.
    pub fn write_from<E>(
        &mut self,
        ticks: i64,
        mut src: impl std::io::Read,
        mut put: impl FnMut(BytesPart<'_>) -> Result<(), E>,
    ) -> Result<ValueSeal, WriteValueError<E>> {
        self.begin(ticks)?;
        let mut put = |p: BytesPart<'_>| put(p).map_err(WriteValueError::Write);
        let mut chunk = vec![0u8; self.options.part_len];
        let r = loop {
            match src.read(&mut chunk) {
                Ok(0) => break self.finish(&mut put),
                Ok(n) => {
                    let bytes = chunk.get(..n).unwrap_or_default();
                    if let Err(e) = self.write(bytes, &mut put) {
                        break Err(e);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => break Err(WriteValueError::Source(e)),
            }
        };
        if r.is_err() {
            self.abandon();
        }
        r
    }
}

/// Where the bytes of a part come from.
#[derive(Copy, Clone, Debug)]
enum DataOf<'a> {
    /// The caller's bytes, a whole part.
    Given(&'a [u8]),
    /// The writer's buffer.
    Buffered,
    /// A seal's bytes.
    Seal(&'a [u8; SEAL_LEN]),
}

impl Seal {
    pub(super) fn to_bytes(self) -> [u8; SEAL_LEN] {
        let mut b = [0u8; SEAL_LEN];
        let (first, rest) = b.split_at_mut(8);
        let (len, hash) = rest.split_at_mut(8);
        first.copy_from_slice(&self.first.get().to_le_bytes());
        len.copy_from_slice(&self.len.to_le_bytes());
        hash.copy_from_slice(&self.hash);
        b
    }
}
