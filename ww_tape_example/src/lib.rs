//! The running example of the ww_tape book: a motor controller board, called **drive**, recorded for an hour on an
//! endurance bench.
//!
//! | Track | What | Kind in a tape |
//! |---|---|---|
//! | `log` | the firmware's log lines, 3 a second | records blocks |
//! | `mode` | the controller's mode: idle, run, fault | state track |
//! | `can0` | CAN frames on the vehicle bus, 1 kHz | events chunks |
//! | `phase_a` | phase current, 25 kHz, 16-bit fixed point in mA, with summaries | series chunks |
//! | `temp` | board temperature, 1 Hz | records blocks |
//!
//! Every track runs on the board's µs timer since boot; a clock map takes it to the host's wall clock. The host
//! records the run into a chain of pieces in memory ([ww_tape::MemTapes]). The drive runs from minute 1
//! ([RUN_MINUTE]) and faults on overcurrent at minute 52 ([FAULT_MINUTE]); in the story the host loses power at minute
//! 37 ([CRASH_MINUTE]) and resumes.
//!
//! [Drive] says how much of the recording to make: [Drive::full] is the hour as the story tells it, and
//! [Drive::scaled] the same hour with a fraction of the data and of every byte policy, so that a figure shows the
//! same pieces and checkpoints at a size that fits a page. Everything is a pure function of the settings: the same
//! [Drive] gives the same bytes, and a crash at any byte ([Drive::crash]) resumes ([Crashed::resume]) to the bytes of
//! the run without it.
//!
//! ```
//! let drive = ww_tape_example::Drive::scaled(1000).expect("1000 divides the rates");
//! let whole = drive.record()?;
//! let crashed = drive.crash(whole.story_crash_byte()?)?;
//! assert_eq!(crashed.resume()?.recording.pieces, whole.pieces);
//! # Ok::<(), ww_tape_example::Error>(())
//! ```
//!
//! `cargo run -p ww_tape_example` prints the numbers the story tells, at full scale.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    reason = "the example computes times, counts and offsets of a run it makes itself from its settings: an hour of µs and \
              a recording of a few hundred MiB are far from any limit, and it reads no bytes of others"
)]

mod card;
mod damage;
mod defs;
pub mod ethernet;
pub mod fault;
pub mod first_tape;
pub mod handles;
pub mod producers;
mod record;
pub mod rf;
pub mod rf_chains;
mod signals;
pub mod wideband;
pub mod writing;

pub use card::{Card, CardRead, EARLIER_CHAIN};
pub use damage::{Damage, Damaged, FLIP_MINUTE, LOST_UNIT, Placed, Salvaged, Walk, walk};
pub use defs::{BOOT_UNIX_NS, DriveNumbers, define, definitions, key, phase_def, phase_summaries};
use exact_units::Bytes;
pub use record::{Act, CRASH_PAST_END, Crash, Crashed, Recording, Resume, Story, records_block};
pub use signals::{
    CRASH_MINUTE, EMERGENCY_ID, FAULT_MINUTE, LIMIT_MA, Mode, RUN_MINUTE, emergency_frame, frame,
    log_line, phase_ma, temp_centi,
};
use ww_tape::{MemError, OwnerId};

/// `bytes` rounded for prose, in decimal units: "115 MB", "14 KB", "2.1 KB"; under 1 000, exact ("164 bytes").
///
/// Two significant digits at least: one decimal below 10 of a unit, dropped when it is 0. A book page writes a size
/// whose precision doesn't matter this way, and a test checks that the page holds `about` of the exact value.
pub fn about(bytes: u64) -> String {
    let (b, half) = (u128::from(bytes), |unit: u128| unit / 2);
    for (unit, name) in [(1_000_000_000_u128, "GB"), (1_000_000, "MB"), (1_000, "KB")] {
        if b < unit {
            continue;
        }
        // rounded half up: whole units from 10 on, tenths below
        if b >= unit * 10 {
            return format!("{} {name}", (b + half(unit)) / unit);
        }
        let tenths = (b * 10 + half(unit)) / unit;
        return match (tenths / 10, tenths % 10) {
            (w, 0) => format!("{w} {name}"),
            (w, t) => format!("{w}.{t} {name}"),
        };
    }
    format!("{bytes} bytes")
}

/// Blocks of one part of a piece, as the book's anatomy of a piece names the parts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Part {
    pub name: &'static str,
    pub count: u64,
    pub bytes: u64,
}

impl Part {
    /// The parts a reader finds its way with: the piece head, checkpoints, final index, close and tail.
    #[must_use]
    pub fn is_structure(&self) -> bool {
        matches!(
            self.name,
            "piece head" | "checkpoints" | "final index" | "close" | "tail"
        )
    }
}

/// The blocks of `piece` by part, in order of first appearance.
///
/// # Errors
///
/// A block that does not read, or an index block that does not decode.
pub fn parts(piece: &[u8]) -> Result<Vec<Part>, Error> {
    use ww_tape::index::{IndexBlock, IndexKind};
    use ww_tape::{BlockKind, FLAG_REPEAT, FileBlocks, Recovery};
    let mut parts: Vec<Part> = Vec::new();
    let mut walk = FileBlocks::with_recovery(piece, Recovery::Prefix);
    while let Some(b) = walk.next_block() {
        let b = b?;
        let name = match b.kind {
            BlockKind::PieceHead => "piece head",
            BlockKind::Records if b.flags & FLAG_REPEAT != 0 => "records blocks of puts and copies",
            BlockKind::Records => "records blocks",
            BlockKind::Events => "events chunks",
            BlockKind::Series => "series chunks",
            BlockKind::Summary => "summary blocks",
            BlockKind::Index => match IndexBlock::read(&b)?.kind() {
                IndexKind::Checkpoint => "checkpoints",
                IndexKind::Final => "final index",
            },
            BlockKind::Close => "close",
            BlockKind::Tail => "tail",
            _ => "other blocks",
        };
        let len = b.len() as u64;
        match parts.iter_mut().find(|p| p.name == name) {
            Some(p) => {
                p.count += 1;
                p.bytes += len;
            }
            None => parts.push(Part {
                name,
                count: 1,
                bytes: len,
            }),
        }
    }
    Ok(parts)
}

/// `n` with its digits in groups of three, as the book writes an exact count: "41 425", "3 120 000", "999".
pub fn grouped(n: u64) -> String {
    let mut out = String::new();
    for (i, d) in n.to_string().chars().rev().enumerate() {
        if i != 0 && i % 3 == 0 {
            out.push(' ');
        }
        out.push(d);
    }
    out.chars().rev().collect()
}

/// The owner of the drive's chain: the board's identity.
pub const OWNER: OwnerId = OwnerId([0xd7; 32]);
/// The number of the drive's chain of its owner.
pub const CHAIN: u64 = 0;
/// The producer the pieces name.
pub const PRODUCER: &str = "drive bench host";

// --8<-- [start:drive_item]
/// How much of the drive's recording to make, and the writer's policies for it.
///
/// [Drive::full] is the story's hour; [Drive::scaled] divides the data rates and the byte policies alike and keeps
/// the pieces: each holds the same minutes of the run at every scale.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Drive {
    /// Length of the run, minutes from the board's boot.
    pub minutes: u32,
    /// Samples of `phase_a` per second; it divides 1 000 000, so a sample period is a whole number of µs.
    pub phase_hz: u32,
    /// CAN frames on `can0` per second.
    pub can_hz: u32,
    /// Log lines per hour.
    pub log_per_hour: u32,
    /// Temperature readings per hour.
    pub temp_per_hour: u32,
    /// Milliseconds of the run each block covers: a records block, a chunk of `can0` and one of `phase_a`. It divides
    /// a minute.
    pub block_ms: u32,
    /// Minutes of the run in a piece: the chain cuts to a new piece once a piece holds the chunks of this many minutes
    /// ([ww_tape::CutPolicy::chunks]), so the pieces fall at the same minutes at every scale.
    pub piece_minutes: u32,
    /// The writer places a checkpoint every this many bytes ([ww_tape::IndexPolicy::bytes]).
    pub checkpoint_bytes: Bytes<u64>,
    /// The definitions and the mode lie at most this far behind each use ([ww_tape::StatePolicy::near]).
    pub near_bytes: Bytes<u32>,
    /// The writer copies a state item first written in a piece this far on
    /// ([ww_tape::NearPolicy::copy_spacing]).
    pub copy_spacing: Bytes<u64>,
    /// `phase_a` carries summaries.
    pub summaries: bool,
    /// The number of the chain of [OWNER] the run records into: another number gives other piece ids and session
    /// tags, as another day's run on the same bench does.
    pub chain: u64,
    /// How the writer places blocks: [ww_tape::Layout::UNALIGNED] for a file, [ww_tape::Layout::on_sd_card] for a
    /// raw card.
    pub layout: ww_tape::Layout,
}
// --8<-- [end:drive_item]

impl Drive {
    /// The story's hour at full rate, with the writer's default byte policies and a piece every 15 minutes.
    pub const fn full() -> Self {
        Self {
            minutes: 60,
            phase_hz: 25_000,
            can_hz: 1_000,
            log_per_hour: 3 * 3_600,
            temp_per_hour: 3_600,
            block_ms: 1_000,
            piece_minutes: 15,
            checkpoint_bytes: ww_tape::index::DEFAULT_CHECKPOINT_BYTES,
            near_bytes: Bytes::new(32 << 20),
            copy_spacing: ww_tape::writer::near::COPY_SPACING,
            summaries: true,
            chain: CHAIN,
            layout: ww_tape::Layout::UNALIGNED,
        }
    }

    /// The story's hour with `1/k` of the data: the rates and every byte policy divided by `k`, a block covering the
    /// largest number of seconds up to `k` that divides a minute. The run keeps its pieces, so the crash falls in the
    /// same piece; None when `k` doesn't divide 1000.
    pub fn scaled(k: u32) -> Option<Self> {
        let full = Self::full();
        if k == 0 || 1000 % k != 0 {
            return None;
        }
        let k64 = u64::from(k);
        Some(Self {
            phase_hz: full.phase_hz / k,
            can_hz: full.can_hz / k,
            log_per_hour: full.log_per_hour / k,
            temp_per_hour: full.temp_per_hour / k,
            block_ms: full.block_ms * block_seconds(k),
            checkpoint_bytes: Bytes::new(full.checkpoint_bytes.get() / k64),
            near_bytes: Bytes::new(full.near_bytes.get() / k),
            copy_spacing: Bytes::new(full.copy_spacing.get() / k64),
            ..full
        })
    }

    /// Chunks the chain cuts after: a chunk of `can0` and one of `phase_a` for each block of a piece.
    pub(crate) fn piece_chunks(&self) -> u64 {
        let blocks =
            (u64::from(self.piece_minutes) * 60_000).div_ceil(u64::from(self.block_ms.max(1)));
        2 * blocks
    }

    /// The same run cut short at `minutes`.
    pub const fn minutes(self, minutes: u32) -> Self {
        Self { minutes, ..self }
    }

    /// The same run written onto a raw SD card ([ww_tape::Layout::on_sd_card]) instead of into files.
    pub const fn on_sd_card(self) -> Self {
        Self {
            layout: self.layout.on_sd_card(),
            ..self
        }
    }

    /// The same run recorded into chain `chain` of [OWNER].
    pub const fn chain(self, chain: u64) -> Self {
        Self { chain, ..self }
    }
}

/// The largest number of seconds up to `k` that divides a minute: what a block of the run at scale `k` covers.
fn block_seconds(k: u32) -> u32 {
    [60, 30, 20, 15, 12, 10, 6, 5, 4, 3, 2, 1]
        .into_iter()
        .find(|&s| s <= k)
        .unwrap_or(1)
}

/// Why the recording could not be made.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// A setting of [Drive] the recording can't be made with.
    Settings(&'static str),
    /// The drive's definitions.
    Describe(ww_tape::DescribeError),
    /// A records block of log lines and temperatures.
    Block(ww_tape::BlockStartError),
    /// A record into a records block.
    Push(ww_tape::PushError),
    /// The times of a chunk of frames.
    Times(ww_tape::times::EncodeError),
    /// The payloads of a chunk of frames.
    Payloads(ww_tape::payloads::EncodeError),
    /// The samples of a chunk of `phase_a`, or its grid.
    Samples(ww_tape::SampleError),
    /// The grid of a chunk of `phase_a`.
    Series(ww_tape::SeriesError),
    /// The summaries of `phase_a`.
    Summaries(ww_tape_summary::BuildError),
    /// The chain's writer.
    Write(ww_tape::WriteError<MemError>),
    /// The memory the pieces are in.
    Mem(MemError),
    /// A mux or one of its handles.
    Mux(ww_tape_mux::MuxError<MemError>),
    /// Reading a piece back.
    Read(Reading),
}

/// Why a piece could not be read back.
#[derive(Debug)]
#[non_exhaustive]
pub enum Reading {
    /// A piece in memory.
    Mem(ww_tape::reader::ReadError<MemError>),
    /// A piece's bytes, such as a damaged copy.
    Bytes(ww_tape::reader::ReadError<ww_tape::reader::OutOfRange>),
    /// A walk over a piece's blocks.
    Walk(ww_tape::FileError),
    /// An index block of a piece.
    Index(ww_tape::index::IndexError),
    /// A block of a piece that doesn't read.
    Block(ww_tape::BlockError),
    /// The times of a chunk of frames that don't read.
    Times(ww_tape::times::TimesError),
    /// The payloads of a chunk of frames that don't read.
    Payloads(ww_tape::payloads::PayloadsError),
}

impl From<ww_tape::reader::ReadError<MemError>> for Error {
    fn from(e: ww_tape::reader::ReadError<MemError>) -> Self {
        Self::Read(Reading::Mem(e))
    }
}

impl From<ww_tape::reader::ReadError<ww_tape::reader::OutOfRange>> for Error {
    fn from(e: ww_tape::reader::ReadError<ww_tape::reader::OutOfRange>) -> Self {
        Self::Read(Reading::Bytes(e))
    }
}

impl From<ww_tape::FileError> for Error {
    fn from(e: ww_tape::FileError) -> Self {
        Self::Read(Reading::Walk(e))
    }
}

impl From<ww_tape::BlockError> for Error {
    fn from(e: ww_tape::BlockError) -> Self {
        Self::Read(Reading::Block(e))
    }
}

impl From<ww_tape::times::TimesError> for Error {
    fn from(e: ww_tape::times::TimesError) -> Self {
        Self::Read(Reading::Times(e))
    }
}

impl From<ww_tape::payloads::PayloadsError> for Error {
    fn from(e: ww_tape::payloads::PayloadsError) -> Self {
        Self::Read(Reading::Payloads(e))
    }
}

impl From<ww_tape::index::IndexError> for Error {
    fn from(e: ww_tape::index::IndexError) -> Self {
        Self::Read(Reading::Index(e))
    }
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Settings(what) => write!(f, "the drive's settings: {what}"),
            Self::Describe(e) => write!(f, "the drive's definitions: {e:?}"),
            Self::Block(e) => write!(f, "a records block: {e:?}"),
            Self::Push(e) => write!(f, "a record: {e:?}"),
            Self::Times(e) => write!(f, "the times of a chunk of frames: {e}"),
            Self::Payloads(e) => write!(f, "the payloads of a chunk of frames: {e}"),
            Self::Samples(e) => write!(f, "the samples of phase_a: {e:?}"),
            Self::Series(e) => write!(f, "the grid of phase_a: {e:?}"),
            Self::Summaries(e) => write!(f, "the summaries of phase_a: {e:?}"),
            Self::Write(e) => write!(f, "writing the chain: {e:?}"),
            Self::Mem(e) => write!(f, "the pieces in memory: {e:?}"),
            Self::Mux(e) => write!(f, "a mux: {e:?}"),
            Self::Read(e) => write!(f, "reading a piece: {e:?}"),
        }
    }
}

impl core::error::Error for Error {}

macro_rules! from {
    ($($variant:ident($t:ty),)*) => {$(
        impl From<$t> for Error {
            fn from(e: $t) -> Self {
                Self::$variant(e)
            }
        }
    )*};
}

from! {
    Describe(ww_tape::DescribeError),
    Block(ww_tape::BlockStartError),
    Push(ww_tape::PushError),
    Times(ww_tape::times::EncodeError),
    Payloads(ww_tape::payloads::EncodeError),
    Samples(ww_tape::SampleError),
    Series(ww_tape::SeriesError),
    Summaries(ww_tape_summary::BuildError),
    Write(ww_tape::WriteError<MemError>),
    Mem(MemError),
    Mux(ww_tape_mux::MuxError<MemError>),
}
