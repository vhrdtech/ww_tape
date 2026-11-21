//! The program of the book's chapter "A first tape".
//!
//! The chapter (`docs/first_tape.md`) includes the marked sections of this file: the drive's run written into a new
//! file and closed ([write()]), then the file opened and its tracks listed ([tracks]).
//! `cargo run -p ww_tape_example --bin first_tape` runs both on the drive's first minute.

use std::path::Path;
use ww_tape::TrackNum;

use shrink_wrap::prelude::*;
use ww_tape::{
    ColumnSpec, Describe, Slot, SyncPolicy, TapeReader, TrackKind, TrackRepeat, writer::Closed,
};

use ww_tape::reader::{OutOfRange, ReadError};
use ww_tape_fs::FsError;

use crate::{Act, Drive, MODE, records_block};

/// Write `drive`'s run into a new file at `path`, then close it: the piece's end written and the file synced.
pub fn write(path: &Path, drive: &Drive) -> Result<Closed, FirstTapeError> {
    // --8<-- [start:write]
    let defs = drive.definitions()?; // the clocks, the clock map and the five tracks
    let mut writer = ww_tape_fs::writer::create(path, SyncPolicy::default())?;
    for act in drive.acts()? {
        match act? {
            // the mode in force from `ticks` on, put at each use
            Act::Put { ticks, mode } => {
                writer.put(&defs, TrackNum::new(MODE), ticks, mode.item())?;
            }
            // log lines and temperatures, one records block a second
            Act::Records(records) => {
                let block = records_block(writer.next_id(), &records)?;
                writer.write_block(&defs, &block)?;
            }
            // a second of CAN frames or of phase current, as the columns of a chunk
            Act::Chunk { head, columns } => {
                let columns: Vec<(ColumnSpec, &[u8])> = columns
                    .iter()
                    .map(|(spec, bytes)| (*spec, &bytes[..]))
                    .collect();
                writer.write_chunk(&defs, &head, &columns)?;
            }
            // the summaries of phase current, placed where they fit
            Act::Summary(block) => writer.defer_block(&defs, block)?,
        }
    }
    let closed = writer.close()?; // the final index, the close record and the tail
    // --8<-- [end:write]
    Ok(closed)
}

// --8<-- [start:tracks_types_item]
/// What a track of a tape is, as [tracks] lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Track {
    /// The track's number in the file.
    pub number: TrackNum,
    /// The label of its definition.
    pub label: String,
    /// How its items are kept: `records`, `state`, `events chunks` or `series chunks`.
    pub kind: &'static str,
    /// Its items in the piece: records, or the items of its chunks; for a state track, its item in force at the end.
    pub items: Items,
}

/// What [Track::items] holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Items {
    /// The items a track of a stream holds in the piece.
    Count(u64),
    /// The item of a state track in force at the end of the piece, none when no record reads.
    InForce(Option<Vec<u8>>),
}
// --8<-- [end:tracks_types_item]

impl core::fmt::Display for Track {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let Self {
            number,
            label,
            kind,
            items,
        } = self;
        write!(f, "track {number}  {label:<8} {kind:<14}")?;
        match items {
            Items::Count(n) => write!(f, " {n:>8} items"),
            Items::InForce(Some(item)) => {
                write!(f, " in force: {:?}", String::from_utf8_lossy(item))
            }
            Items::InForce(None) => write!(f, " none in force"),
        }
    }
}

/// Open the tape in `bytes` and list the tracks of its first piece: each track's definition and the items its close
/// record counts.
pub fn tracks(bytes: &[u8]) -> Result<Vec<Track>, FirstTapeError> {
    // --8<-- [start:read]
    let reader = TapeReader::open(bytes)?;
    let piece = reader.pieces().first().ok_or(FirstTapeError::NoPiece)?;
    let (_close, ranges) = piece.close.as_ref().ok_or(FirstTapeError::NotClosed)?;
    let defs = reader.definitions_at(0, piece.end)?; // every definition in force at the end
    let mut tracks = Vec::new();
    for (slot, state) in &defs.items {
        let Slot::Track { number } = *slot else {
            continue;
        }; // a clock or the clock map
        let Ok(Describe::Track(def)) = Describe::from_ww_bytes(&state.item) else {
            return Err(FirstTapeError::Definition(number));
        };
        let range = ranges.iter().find(|r| r.track_number == number);
        let kind = match (&def.repeat, &def.kind, range.map_or(0, |r| r.chunks)) {
            (TrackRepeat::State(_), _, _) => "state",
            (_, TrackKind::Series(_), _) => "series chunks",
            (_, TrackKind::Events(_), 0) => "records",
            (_, TrackKind::Events(_), _) => "events chunks",
            (_, TrackKind::Unknown(_), _) => "unknown", // a kind newer than this build
        };
        let items = match def.repeat {
            // the record of the state in force where the piece ends
            TrackRepeat::State(_) => Items::InForce(
                reader
                    .state_at(0, Slot::State { track: number }, piece.end)?
                    .item
                    .map(|s| s.item),
            ),
            _ => Items::Count(range.map_or(0, |r| if r.chunks == 0 { r.records } else { r.items })),
        };
        tracks.push(Track {
            number,
            label: def.label.to_owned(),
            kind,
            items,
        });
    }
    // --8<-- [end:read]
    Ok(tracks)
}

/// Why the first tape could not be written or read.
#[derive(Debug)]
#[non_exhaustive]
pub enum FirstTapeError {
    /// The drive's definitions or acts.
    Drive(crate::Error),
    /// Writing the file.
    Write(ww_tape::WriteError<FsError>),
    /// Reading the tape.
    Read(ReadError<OutOfRange>),
    /// Reading the file back.
    Io(std::io::Error),
    /// The tape holds no piece.
    NoPiece,
    /// The piece has no close record: it was never closed.
    NotClosed,
    /// The definition of this track does not decode.
    Definition(TrackNum),
}

impl core::fmt::Display for FirstTapeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Drive(e) => write!(f, "the drive's run: {e}"),
            Self::Write(e) => write!(f, "writing the file: {e}"),
            Self::Read(e) => write!(f, "reading the tape: {e}"),
            Self::Io(e) => write!(f, "reading the file back: {e}"),
            Self::NoPiece => f.write_str("the tape holds no piece"),
            Self::NotClosed => f.write_str("the piece was never closed"),
            Self::Definition(n) => write!(f, "the definition of track {n} does not decode"),
        }
    }
}

impl core::error::Error for FirstTapeError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Drive(e) => Some(e),
            Self::Write(e) => Some(e),
            Self::Read(e) => Some(e),
            Self::Io(e) => Some(e),
            Self::NoPiece | Self::NotClosed | Self::Definition(_) => None,
        }
    }
}

impl From<crate::Error> for FirstTapeError {
    fn from(e: crate::Error) -> Self {
        Self::Drive(e)
    }
}

impl From<ww_tape::WriteError<FsError>> for FirstTapeError {
    fn from(e: ww_tape::WriteError<FsError>) -> Self {
        Self::Write(e)
    }
}

impl From<ReadError<OutOfRange>> for FirstTapeError {
    fn from(e: ReadError<OutOfRange>) -> Self {
        Self::Read(e)
    }
}

/// Write `drive`'s run into a new file at `path` ([write()]), read it back ([tracks]) and report both: the piece's
/// bytes, then a line per track.
pub fn run(path: &Path, drive: &Drive) -> Result<String, FirstTapeError> {
    let closed = write(path, drive)?;
    let bytes = std::fs::read(path).map_err(FirstTapeError::Io)?;
    let mut out = format!("{} bytes in one piece\n", closed.len);
    for track in tracks(&bytes)? {
        out.push_str(&format!("{track}\n"));
    }
    Ok(out)
}
