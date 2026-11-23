//! What the book's cases share: the capture into a window in memory and its triage, the RF signal, the chain
//! options that make the same bytes every run, and the helpers their tests and pages use.
//!
//! | Module or item | What |
//! |---|---|
//! | [window] | the memory window: muxes write chains of pieces in memory, a triage per chain reads each piece as it closes and keeps the ones worth keeping |
//! | [files] | where a headless binary keeps what its triage keeps: directories of piece files |
//! | [rf] | the RF signal and its track `iq`, recorded by the RF capture and by each channel of the wideband receiver |
//! | [chain_options] | a chain whose bytes depend only on what is written |
//! | [pieces] | the bytes of the closed pieces of a chain in memory |
//! | [hash] | the fixed numbers the synthetic signals and traffic are made from |
//! | [about] | a size rounded for prose |
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    reason = "the cases compute times, counts and offsets of captures they make themselves from their settings, far \
              from any limit, and read no bytes of others"
)]

pub mod files;
pub mod rf;
pub mod window;

use ww_tape::index::IndexPolicy;
use ww_tape::{ChainOptions, ChainPiece, ClockRef, MemError, MemTapes, SyncPolicy, WriterOptions};

/// How a case's chain is written: one piece, no sync, no checkpoint by time and no wall clock, so its bytes
/// depend only on the order the handles' chunks reach the mux.
pub fn chain_options() -> ChainOptions {
    ChainOptions {
        writer: WriterOptions {
            sync: SyncPolicy::None,
            index: IndexPolicy {
                interval: None,
                ..IndexPolicy::default()
            },
            clock: ClockRef::NONE,
            ..WriterOptions::default()
        },
        ..ChainOptions::default()
    }
}

/// The bytes of each piece of a chain in memory.
///
/// # Errors
///
/// A piece the store doesn't hold.
pub fn pieces(tapes: &MemTapes, closed: &[ChainPiece<u64>]) -> Result<Vec<Vec<u8>>, PieceError> {
    closed
        .iter()
        .map(|p| {
            tapes
                .get(&p.tape)
                .map_err(PieceError::Mem)?
                .ok_or(PieceError::Missing(p.tape))?
                .to_vec()
                .map_err(PieceError::Mem)
        })
        .collect()
}

/// Why [pieces] failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum PieceError {
    /// The memory the pieces are in.
    Mem(MemError),
    /// A closed piece the memory no longer holds, by its key.
    Missing(u64),
}

impl core::fmt::Display for PieceError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Mem(e) => write!(f, "the pieces in memory: {e}"),
            Self::Missing(key) => write!(f, "closed piece {key} is not in memory"),
        }
    }
}

impl core::error::Error for PieceError {}

/// A number from `seed` and `n`, the same for the same pair (SplitMix64).
pub fn hash(seed: u64, n: u64) -> u64 {
    let mut z = seed ^ n.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

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
