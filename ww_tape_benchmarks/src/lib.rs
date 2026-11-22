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
pub const WINDOW_COLUMNS: [&str; 8] = [
    "Capture ms",
    "Of the pace",
    "Wall ms",
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
    /// Until every byte was in memory.
    pub capture: Duration,
    /// Until the triage read the last piece.
    pub wall: Duration,
    /// What the triage of each chain did.
    pub triaged: Vec<Triaged>,
    /// Bytes of the results' chain.
    pub results_bytes: u64,
}

impl WindowRun {
    /// The sum of `f` over the chains.
    fn sum(&self, f: impl Fn(&Triaged) -> u64) -> u64 {
        self.triaged.iter().map(f).sum()
    }

    /// Bytes the capture takes in memory per second of the signal, in all chains.
    fn tape_per_s(&self) -> f64 {
        self.sum(|t| t.piece_bytes) as f64 / self.signal_s.max(f64::MIN_POSITIVE)
    }
}

/// The bytes the triage of `t` read, the pieces it looked at, of the same size on average as all of them; and the
/// seconds it took.
fn read_of(t: &Triaged) -> (f64, f64) {
    let read = t.piece_bytes as f64 * t.looked as f64 / t.pieces.max(1) as f64;
    (read, t.looking.as_secs_f64().max(f64::MIN_POSITIVE))
}

/// The cells of `r` under [WINDOW_COLUMNS], its window in half of `memory_bytes`, the machine's memory. "Held max
/// MB" adds up the most each chain held.
pub fn window_row(r: &WindowRun, memory_bytes: Option<u64>) -> Vec<Cell> {
    let capture = r.capture.as_secs_f64();
    let window = memory_bytes.map_or(Cell::text("unknown"), |m| {
        Cell::num(m as f64 / 2.0 / r.tape_per_s(), 1)
    });
    vec![
        Cell::num(capture * 1e3, 0),
        Cell::num(r.signal_s / capture, 2),
        Cell::num(r.wall.as_secs_f64() * 1e3, 0),
        Cell::count(r.sum(|t| t.pieces)),
        Cell::count(r.sum(|t| t.overrun.len() as u64)),
        Cell::num(r.sum(|t| t.held_max) as f64 / 1e6, 0),
        Cell::num(r.tape_per_s() / 1e6, 0),
        window,
    ]
}

/// The cells of `r` under [TRIAGE_COLUMNS]. "Triage MB/s" is what one triage thread reads a second, and "Of the
/// pace" that of the slowest chain against its own share of the signal.
pub fn triage_row(r: &WindowRun) -> Vec<Cell> {
    let (read, looking) = r
        .triaged
        .iter()
        .map(read_of)
        .fold((0.0, 0.0), |(r, l), (r1, l1)| (r + r1, l + l1));
    let pace = r
        .triaged
        .iter()
        .map(|t| {
            let (read, looking) = read_of(t);
            read / looking / (t.piece_bytes as f64 / r.signal_s.max(f64::MIN_POSITIVE))
        })
        .fold(f64::INFINITY, f64::min);
    let (pieces, kept) = (r.sum(|t| t.piece_bytes), r.sum(|t| t.kept_bytes));
    let copying: f64 = r.triaged.iter().map(|t| t.copying.as_secs_f64()).sum();
    vec![
        Cell::num(read / looking.max(f64::MIN_POSITIVE) / 1e6, 0),
        Cell::num(pace, 1),
        Cell::count(r.sum(|t| t.kept.len() as u64)),
        Cell::num(100.0 * kept as f64 / pieces.max(1) as f64, 1),
        Cell::num(r.results_bytes as f64 / 1e3, 0),
        Cell::num((kept + r.results_bytes) as f64 / r.signal_s / 1e6, 1),
        if copying > 0.0 {
            Cell::num(kept as f64 / copying / 1e6, 0)
        } else {
            Cell::text("none kept")
        },
    ]
}
