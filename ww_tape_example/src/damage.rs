//! Damage to an archived piece of the drive's recording: a flipped bit, lost sectors at the start or the end, and
//! what each reader still gets from the piece.

use core::ops::Range;

use ww_tape::reader::Found;
use ww_tape::{BlockKind, ColumnKind, FileBlocks, FileError, Recovery, Samples, Slot, TapeReader};

use crate::defs::{MODE, PHASE_A, phase_def};
use crate::record::Recording;
use crate::signals::US_PER_MS;
use crate::{Drive, Error};

/// The minute whose first chunk of `phase_a` the story's flipped bit lands in: in the last piece, which holds the
/// fault.
pub const FLIP_MINUTE: u32 = 50;

/// Bytes a lost stretch at the start or the end of a piece covers: a 4 KiB unit of a disk or card.
pub const LOST_UNIT: u64 = 4096;

/// A piece of the recording after damage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Damaged {
    /// The piece of the chain, from 0.
    pub piece: usize,
    /// Its bytes as written.
    pub whole: Vec<u8>,
    /// Its bytes after the damage.
    pub bytes: Vec<u8>,
    /// The bytes the damage changed, offsets in the piece.
    pub damage: Range<u64>,
}

/// A block of a piece: where it starts, its length and kind.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Placed {
    pub at: u64,
    pub len: u64,
    pub kind: BlockKind,
}

/// A chunk a Resync walk handed out with a damaged payload, and what of it still reads.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Salvaged {
    pub block: Placed,
    /// Segments of its payload, and those whose CRC fails.
    pub segments: usize,
    pub bad_segments: usize,
    /// Items it holds, and those in groups whose bytes lie in a bad segment: samples of a series chunk. None for a
    /// chunk this report doesn't decode.
    pub items: u64,
    pub items_lost: Option<u64>,
    /// The tick of its first item.
    pub first_tick: i64,
}

/// One walk over a piece's bytes in one recovery mode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Walk {
    /// The blocks it handed out, in order.
    pub blocks: Vec<Placed>,
    /// Those handed out with a damaged payload.
    pub salvaged: Vec<Salvaged>,
    /// What it reported: where, what, and the bytes it covers (0 when the report names none).
    pub reports: Vec<(u64, &'static str, u64)>,
}

/// What the readers get from a damaged piece.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Damage {
    /// The blocks of the piece as written, in order.
    pub blocks: Vec<Placed>,
    /// How [TapeReader] found the piece: by its tail, or by a checkpoint and a scan.
    pub found: Found,
    /// Where [TapeReader]'s scan stopped, when it stopped before the end.
    pub cut: Option<u64>,
    /// Whether the piece still checks against its content hash; None when the reader finds no close to check it by.
    pub holds: Option<bool>,
    /// Blocks of data and summaries the index lists that read, and those it lists that don't; each block once.
    pub read: Vec<Placed>,
    pub unreadable: Vec<Placed>,
    /// Blocks of data and summaries of the piece as written that no index entry the reader has lists.
    pub unlisted: Vec<Placed>,
    /// The definitions in force at the piece's end that the reader finds, and those the piece as written gives.
    pub definitions: usize,
    pub definitions_written: usize,
    /// The first ticks of the chunks the index lists at whose start the reader finds no mode in force.
    pub mode_unknown: Vec<i64>,
    /// The walks over the bytes in [Recovery::Prefix] and [Recovery::Resync].
    pub prefix: Walk,
    pub resync: Walk,
}

impl Damage {
    /// The blocks of data and summaries the piece holds: what the index lists.
    pub fn data_blocks(&self) -> usize {
        self.blocks.iter().filter(|b| is_data(b.kind)).count()
    }
}

/// A block the index lists: data, summaries and state; not the piece head, an index, the close or the tail.
fn is_data(kind: BlockKind) -> bool {
    !ww_tape::index::is_structural(kind)
}

fn report(e: &FileError) -> (u64, &'static str, u64) {
    match e {
        FileError::Cut { offset, .. } => (*offset, "cut", 0),
        FileError::Bad {
            offset, skipped, ..
        } => (*offset, "bad", *skipped),
        FileError::Truncated { offset, len } => (*offset, "truncated", *len),
        FileError::Gap { offset, len } => (*offset, "gap", *len),
        FileError::Replaced { offset, .. } => (*offset, "replaced", 0),
        FileError::Conflict { offset, .. } => (*offset, "conflict", 0),
        FileError::Stale { offset, len, .. } => (*offset, "stale", *len),
        FileError::Refused { offset, len, .. } => (*offset, "refused", *len),
        _ => (0, "other", 0),
    }
}

/// The samples of a series chunk of `drive`'s `phase_a` in groups whose bytes lie in a bad segment.
fn samples_lost(drive: &Drive, chunk: &ww_tape::Chunk<'_>) -> Option<u64> {
    let head = chunk.head();
    if head.track_number != PHASE_A {
        return None;
    }
    let def = phase_def(drive.phase_hz, drive.summaries)?;
    let column = chunk.find(ColumnKind::Samples, 0)?;
    let count = usize::try_from(head.count).ok()?;
    let samples = Samples::of(&column, crate::defs::phase_shape(&def), count).ok()?;
    let mut lost = 0u64;
    for g in 0..samples.group_count() {
        let (items, bytes) = samples.group_bytes(g)?;
        let bytes = column.offset + bytes.start..column.offset + bytes.end;
        if chunk.range_damaged(bytes) {
            lost += items.len() as u64;
        }
    }
    Some(lost)
}

/// Walk `bytes` in `recovery`.
pub fn walk(drive: &Drive, bytes: &[u8], recovery: Recovery) -> Walk {
    let mut out = Walk {
        blocks: Vec::new(),
        salvaged: Vec::new(),
        reports: Vec::new(),
    };
    let mut w = FileBlocks::with_recovery(bytes, recovery);
    while let Some(b) = w.next_block() {
        let b = match b {
            Ok(b) => b,
            Err(e) => {
                out.reports.push(report(&e));
                continue;
            }
        };
        let (len, kind) = (b.len() as u64, b.kind);
        let salvaged = match (b.is_intact(), b.chunk()) {
            (false, Some(chunk)) => Some((
                chunk.segment_count(),
                chunk.bad_segments().count(),
                u64::from(chunk.head().count),
                samples_lost(drive, &chunk),
                chunk.head().first_tick,
            )),
            _ => None,
        };
        let placed = Placed {
            at: w.offset(),
            len,
            kind,
        };
        if let Some((segments, bad_segments, items, items_lost, first_tick)) = salvaged {
            out.salvaged.push(Salvaged {
                block: placed,
                segments,
                bad_segments,
                items,
                items_lost,
                first_tick,
            });
        }
        out.blocks.push(placed);
    }
    out
}

impl Damaged {
    /// What the readers get from the damaged bytes.
    pub fn read(&self, drive: &Drive) -> Result<Damage, Error> {
        let blocks = walk(drive, &self.whole, Recovery::Prefix).blocks;
        let r = TapeReader::open(&self.bytes[..])?;
        let p = r
            .pieces()
            .first()
            .ok_or(Error::Settings("the damaged piece opens"))?;
        let (found, cut) = (p.found, p.cut);
        let holds = match r.verify(0) {
            Ok(v) => Some(v.holds),
            Err(ww_tape::reader::ReadError::Unclosed) => None,
            Err(_) => Some(false),
        };
        let entries = r.entries(0)?;
        let (mut read, mut unreadable) = (Vec::new(), Vec::new());
        let mut buf = Vec::new();
        for e in &entries {
            if read
                .iter()
                .chain(&unreadable)
                .any(|b: &Placed| b.at == e.offset)
            {
                // a block of state records has an entry per record
                continue;
            }
            let placed = Placed {
                at: e.offset,
                len: u64::from(e.len),
                kind: e.kind,
            };
            match r.read_block(0, e, &mut buf) {
                Ok(_) => read.push(placed),
                Err(_) => unreadable.push(placed),
            }
        }
        let unlisted = blocks
            .iter()
            .filter(|b| is_data(b.kind) && !entries.iter().any(|e| e.offset == b.at))
            .copied()
            .collect();
        let definitions = r.definitions_at(0, u64::MAX)?.items.len();
        let definitions_written = TapeReader::open(&self.whole[..])?
            .definitions_at(0, u64::MAX)?
            .items
            .len();
        let mut mode_unknown = Vec::new();
        for e in entries
            .iter()
            .filter(|e| matches!(e.kind, BlockKind::Series | BlockKind::Events))
        {
            if r.state_at(0, Slot::State { track: MODE }, e.offset)?
                .item
                .is_none()
            {
                mode_unknown.push(e.first_tick);
            }
        }
        mode_unknown.dedup();
        Ok(Damage {
            blocks,
            definitions,
            mode_unknown,
            definitions_written,
            found,
            cut,
            holds,
            read,
            unreadable,
            unlisted,
            prefix: walk(drive, &self.bytes, Recovery::Prefix),
            resync: walk(drive, &self.bytes, Recovery::Resync),
        })
    }
}

impl Recording {
    /// Piece `piece` with bit 0 of byte `at` flipped.
    pub fn flip(&self, piece: usize, at: u64) -> Result<Damaged, Error> {
        let whole = self.piece(piece)?;
        let mut bytes = whole.clone();
        let i = usize::try_from(at).map_err(|_e| Error::Settings("the byte is in memory"))?;
        let b = bytes
            .get_mut(i)
            .ok_or(Error::Settings("the flipped byte lies in the piece"))?;
        *b ^= 1;
        Ok(Damaged {
            piece,
            whole,
            bytes,
            damage: at..at + 1,
        })
    }

    /// Piece `piece` with the bytes of `range` read as zeros: sectors a disk or card lost.
    pub fn zero(&self, piece: usize, range: Range<u64>) -> Result<Damaged, Error> {
        let whole = self.piece(piece)?;
        let mut bytes = whole.clone();
        let r = usize::try_from(range.start).map_err(|_e| Error::Settings("in memory"))?
            ..usize::try_from(range.end).map_err(|_e| Error::Settings("in memory"))?;
        bytes
            .get_mut(r)
            .ok_or(Error::Settings("the lost bytes lie in the piece"))?
            .fill(0);
        Ok(Damaged {
            piece,
            whole,
            bytes,
            damage: range,
        })
    }

    /// The bytes of piece `piece`.
    fn piece(&self, piece: usize) -> Result<Vec<u8>, Error> {
        self.pieces
            .get(piece)
            .cloned()
            .ok_or(Error::Settings("the piece is one of the chain's"))
    }

    /// The story's flipped bit: the piece and the offset in it of the byte in the middle of the first chunk of
    /// `phase_a` at or after minute [FLIP_MINUTE].
    pub fn story_flip(&self) -> Result<(usize, u64), Error> {
        let at = i64::from(FLIP_MINUTE) * 60_000 * US_PER_MS;
        let byte = self.chunk_at(PHASE_A, at).ok_or(Error::Settings(
            "the run ends before the flipped bit's minute",
        ))?;
        let (piece, offset) = self
            .locate(byte)
            .ok_or(Error::Settings("the flipped byte lies in the recording"))?;
        let piece = usize::try_from(piece).map_err(|_e| Error::Settings("in memory"))?;
        Ok((piece, offset))
    }

    /// The story's three damages to the last piece, which holds the fault: a flipped bit in a chunk of `phase_a`,
    /// the first [LOST_UNIT] bytes lost, and the last [LOST_UNIT] bytes lost.
    pub fn story_damage(&self) -> Result<[Damaged; 3], Error> {
        let (piece, at) = self.story_flip()?;
        let len = self.piece(piece)?.len() as u64;
        Ok([
            self.flip(piece, at)?,
            self.zero(piece, 0..LOST_UNIT)?,
            self.zero(piece, len - LOST_UNIT..len)?,
        ])
    }
}
