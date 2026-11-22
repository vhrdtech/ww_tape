//! The window in memory: what it keeps around a verdict, what it drops unread when its analyser falls behind, and a
//! triage of its own for each chain.
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
use ww_tape_example::ethernet::ram::RamCapture;
use ww_tape_example::ethernet::{Link, Rx, frames_id};
use ww_tape_example::window::{Analyser, Chain, RunError, Triaged, Window, run};

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
        chains: 1,
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
        &mut [Chain {
            kept: &mut kept,
            analyser: &mut picks,
        }],
        |muxes| {
            let r = c.link.record(muxes);
            go_tx.send(()).ok();
            r
        },
    )
    .expect("the capture runs");
    let [triaged] = <[Triaged; 1]>::try_from(ran.triaged).expect("a chain");
    (triaged, kept, picks.kept)
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
    // the analyser reads one piece and is held back; once the capture ended it reads what fits the window, and the
    // last piece if it closed later
    assert!(t.kept.iter().all(|s| !t.overrun.contains(s)));
    assert_eq!(t.kept.len() as u64, t.looked);
    assert!(t.looked <= 1 + 9, "{} read", t.looked);
    assert_eq!(kept.keys().expect("the keys"), t.kept);
}

/// Keeps every fifth piece.
fn fifth(seq: u64) -> bool {
    seq.is_multiple_of(5)
}

#[test]
fn each_chain_has_a_triage_of_its_own() {
    let c = RamCapture {
        link: Link {
            chunk_bytes: 16 << 10,
            ..Link::full().micros(2_000)
        },
        chains: 2,
        piece_bytes: 64 << 10,
        window: Window::default(),
    };
    let picks = || Picks {
        keep: fifth,
        go: None,
        kept: Vec::new(),
    };
    let (mut kept0, mut kept1) = (MemTapes::new(), MemTapes::new());
    let (mut picks0, mut picks1) = (picks(), picks());
    let ran = run(
        OwnerId([0xe7; 32]),
        c.chain_options(),
        c.window,
        &mut [
            Chain {
                kept: &mut kept0,
                analyser: &mut picks0,
            },
            Chain {
                kept: &mut kept1,
                analyser: &mut picks1,
            },
        ],
        |muxes| {
            assert_eq!(muxes.len(), 2, "a mux per chain");
            c.link.record(muxes)
        },
    )
    .expect("the capture runs");
    assert_eq!(ran.triaged.len(), 2);
    for (port, (t, kept, picks)) in [
        (&ran.triaged[0], &kept0, &picks0),
        (&ran.triaged[1], &kept1, &picks1),
    ]
    .into_iter()
    .zip(0u32..)
    .map(|(chain, port)| (port, chain))
    {
        assert!(t.pieces > 10, "chain {port}: {} pieces", t.pieces);
        assert_eq!(t.looked, t.pieces, "chain {port}: every piece read");
        let want: Vec<u64> = (0..t.pieces).filter(|&s| fifth(s)).collect();
        assert_eq!(t.kept, want, "chain {port}");
        assert_eq!(picks.kept, t.kept, "chain {port}");
        assert_eq!(kept.keys().expect("the keys"), t.kept, "chain {port}");
        // a chain holds the frames of its own port only
        let (own, other) = (
            Rx { port, queue: 0 },
            Rx {
                port: 1 - port,
                queue: 0,
            },
        );
        let mut own_chunks = 0;
        for seq in &t.kept {
            let tape = kept.closed(seq).expect("the store").expect("a kept piece");
            let r = TapeReader::open(tape).expect("a kept piece reads alone");
            own_chunks += r.track_entries(0, frames_id(own)).expect("the index").len();
            assert!(
                r.track_entries(0, frames_id(other))
                    .expect("the index")
                    .is_empty(),
                "chain {port}, piece {seq}"
            );
        }
        assert!(own_chunks > 0, "chain {port}");
    }
}

#[test]
fn a_capture_needs_a_chain() {
    let c = RamCapture {
        chains: 1,
        ..RamCapture::full()
    };
    let chains: &mut [Chain<'_, MemTapes, Picks>] = &mut [];
    let ran = run(
        OwnerId([0xe7; 32]),
        c.chain_options(),
        c.window,
        chains,
        |muxes| c.link.record(muxes),
    );
    assert!(matches!(ran, Err(RunError::NoChain)), "{ran:?}");
}
