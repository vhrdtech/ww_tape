//! The window in memory: what it keeps around a verdict, and what it drops unread when its analyser falls behind.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "tests fail by panicking and compute expected values freely"
)]

use std::sync::mpsc;

use ww_tape::{MemTape, MemTapes, OwnerId, TapeReader};
use ww_tape_example::ethernet::Link;
use ww_tape_example::ethernet::ram::RamCapture;
use ww_tape_example::window::{Analyser, Triaged, Window, run};

/// Keeps the pieces `keep` names; before its first look it waits for `go`, when there is one.
struct Picks {
    keep: fn(u64) -> bool,
    go: Option<mpsc::Receiver<()>>,
    kept: Vec<u64>,
}

impl Analyser for Picks {
    type Error = mpsc::RecvError;

    fn look(&mut self, seq: u64, _: &TapeReader<MemTape>) -> Result<bool, Self::Error> {
        if let Some(go) = self.go.take() {
            go.recv()?;
        }
        Ok((self.keep)(seq))
    }

    fn kept(&mut self, seq: u64, _: &TapeReader<MemTape>) -> Result<(), Self::Error> {
        self.kept.push(seq);
        Ok(())
    }
}

/// 2 ms of the link in pieces of 64 KiB through `window`, `picks` reading them: what the triage did, and the kept
/// store. With `hold`, the analyser waits until the capture ended.
fn triaged(window: Window, keep: fn(u64) -> bool, hold: bool) -> (Triaged, MemTapes, Vec<u64>) {
    let c = RamCapture {
        link: Link {
            chunk_bytes: 16 << 10,
            ..Link::full().micros(2_000)
        },
        piece_bytes: 64 << 10,
        window,
    };
    let (go_tx, go) = mpsc::channel();
    let mut picks = Picks {
        keep,
        go: hold.then_some(go),
        kept: Vec::new(),
    };
    let mut kept = MemTapes::new();
    let ran = run(
        OwnerId([0xe7; 32]),
        c.chain_options(),
        window,
        &mut kept,
        &mut picks,
        |mux| {
            let r = c.link.record(&[mux]);
            go_tx.send(()).ok();
            r
        },
    )
    .expect("the capture runs");
    (ran.triaged, kept, picks.kept)
}

#[test]
fn the_pieces_around_a_kept_one_are_kept() {
    let window = Window {
        bytes_max: 1 << 30,
        before: 2,
        after: 1,
    };
    let (t, kept, told) = triaged(window, |seq| seq == 10 || seq == 11 || seq == 20, false);
    assert!(t.pieces > 25, "{} pieces", t.pieces);
    assert_eq!(t.kept, [8, 9, 10, 11, 12, 18, 19, 20, 21]);
    assert_eq!(
        told, t.kept,
        "the analyser hears of each kept piece in order"
    );
    assert_eq!(kept.keys().expect("the keys"), t.kept);
    assert!(t.overrun.is_empty());
    assert_eq!(t.looked, t.pieces);
}

#[test]
fn a_full_window_drops_the_oldest_pieces_unread() {
    // room for about 8 pieces while the analyser waits for the whole capture
    let window = Window {
        bytes_max: 8 * (64 << 10),
        before: 1,
        after: 0,
    };
    let (t, kept, _) = triaged(window, |_| true, true);
    assert!(t.pieces > 25, "{} pieces", t.pieces);
    assert!(!t.overrun.is_empty(), "{t:?}");
    assert!(t.overrun.is_sorted(), "{:?}", t.overrun);
    assert_eq!(
        t.looked + t.overrun.len() as u64,
        t.pieces,
        "each piece read or dropped"
    );
    assert!(
        t.held_max > window.bytes_max,
        "the window filled: {}",
        t.held_max
    );
    // one piece read before the wait, then what is left once the capture ended: what fits the window, and the last
    // piece if it closed later
    assert!(t.kept.iter().all(|s| !t.overrun.contains(s)));
    assert_eq!(t.kept.len() as u64, t.looked);
    assert!(t.looked <= 1 + 9, "{} read", t.looked);
    assert_eq!(kept.keys().expect("the keys"), t.kept);
}
