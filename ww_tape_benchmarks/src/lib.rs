//! What the case benches `ethernet` and `wideband` share: the tables of a capture into a window in memory
//! (`ww_tape_example::window`).
//!
//! A run captures at full speed into memory while the triage reads each piece and keeps a few. Its rows say how fast
//! the capture went against the signal's own pace, how much memory it took, how many seconds of the signal a window
//! of half the machine's memory holds, how many bytes of the capture the analyser got through a second (whatever it
//! read of them), and how many bytes reached the kept store.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_precision_loss,
    reason = "a bench reads no file of others: it computes sizes and rates freely"
)]

use std::time::Duration;

use ww_tape_bench::Cell;
use ww_tape_example::window::Triaged;

/// The columns of [window_row], after the run's own.
pub const WINDOW_COLUMNS: [&str; 7] = [
    "Wall ms",
    "Of the pace",
    "Pieces",
    "Overrun",
    "Held max MB",
    "Tape MB per s",
    "Window s",
];

/// The columns of [triage_row], after the run's own.
pub const TRIAGE_COLUMNS: [&str; 7] = [
    "Triage MB/s",
    "Of the pace",
    "Kept pieces",
    "Kept %",
    "Results KB",
    "To disk MB per s",
    "Copy MB/s",
];

/// One capture into a window in memory.
pub struct WindowRun {
    /// Seconds of the signal the run captured.
    pub signal_s: f64,
    pub wall: Duration,
    pub triaged: Triaged,
    /// Bytes of the results' chain.
    pub results_bytes: u64,
}

impl WindowRun {
    /// Bytes the capture takes in memory per second of the signal.
    fn tape_per_s(&self) -> f64 {
        self.triaged.piece_bytes as f64 / self.signal_s.max(f64::MIN_POSITIVE)
    }
}

/// The cells of `r` under [WINDOW_COLUMNS], its window in half of `memory_bytes`, the machine's memory.
pub fn window_row(r: &WindowRun, memory_bytes: Option<u64>) -> Vec<Cell> {
    let t = &r.triaged;
    let wall = r.wall.as_secs_f64();
    let window = memory_bytes.map_or(Cell::text("unknown"), |m| {
        Cell::num(m as f64 / 2.0 / r.tape_per_s(), 1)
    });
    vec![
        Cell::num(wall * 1e3, 0),
        Cell::num(r.signal_s / wall, 2),
        Cell::count(t.pieces),
        Cell::count(t.overrun.len() as u64),
        Cell::num(t.held_max as f64 / 1e6, 0),
        Cell::num(r.tape_per_s() / 1e6, 0),
        window,
    ]
}

/// The cells of `r` under [TRIAGE_COLUMNS].
pub fn triage_row(r: &WindowRun) -> Vec<Cell> {
    let t = &r.triaged;
    let looking = t.looking.as_secs_f64().max(f64::MIN_POSITIVE);
    // the bytes read: the pieces looked at, of the same size on average as all of them
    let read = t.piece_bytes as f64 * t.looked as f64 / t.pieces.max(1) as f64;
    let copying = t.copying.as_secs_f64();
    vec![
        Cell::num(read / looking / 1e6, 0),
        Cell::num(read / looking / r.tape_per_s(), 1),
        Cell::count(t.kept.len() as u64),
        Cell::num(100.0 * t.kept_bytes as f64 / t.piece_bytes.max(1) as f64, 1),
        Cell::num(r.results_bytes as f64 / 1e3, 0),
        Cell::num(
            (t.kept_bytes + r.results_bytes) as f64 / r.signal_s / 1e6,
            1,
        ),
        if copying > 0.0 {
            Cell::num(t.kept_bytes as f64 / copying / 1e6, 0)
        } else {
            Cell::text("none kept")
        },
    ]
}
