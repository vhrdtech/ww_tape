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

use example_capture::window::{Analyser, Chain, RunError, Triaged, Window, run, start};
use example_ethernet::ram::RamCapture;
use example_ethernet::{Link, Rx, frames_id};
use ww_tape::{MemTape, MemTapes, OwnerId, TapeReader};

/// Keeps the pieces `keep` names; before its first look it waits for `go`, when there is one.
#[derive(Debug)]
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
        triage_threads: 2,
        link: Link {
            chunk_bytes: 16 << 10,
            ..Link::full().micros(2_000)
        },
        chains: 1,
        piece_bytes: 64 << 10,
        window,
    };
    let (go_tx, go) = mpsc::channel();
    let picks = Picks {
        keep,
        go: hold.then_some(go),
        kept: Vec::new(),
    };
    let kept = MemTapes::new();
    let (ran, chains) = run(
        OwnerId([0xe7; 32]),
        c.chain_options(),
        window,
        vec![Chain {
            kept: kept.clone(),
            analyser: picks,
        }],
        move |muxes| {
            let r = c.link.record(muxes);
            go_tx.send(()).ok();
            r
        },
    )
    .expect("the capture runs");
    let [triaged] = <[Triaged; 1]>::try_from(ran.triaged).expect("a chain");
    let [chain] = <[Chain<MemTapes, Picks>; 1]>::try_from(chains).expect("a chain");
    (triaged, kept, chain.analyser.kept)
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
        triage_threads: 2,
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
    let (kept0, kept1) = (MemTapes::new(), MemTapes::new());
    let (ran, chains) = run(
        OwnerId([0xe7; 32]),
        c.chain_options(),
        c.window,
        vec![
            Chain {
                kept: kept0.clone(),
                analyser: picks(),
            },
            Chain {
                kept: kept1.clone(),
                analyser: picks(),
            },
        ],
        move |muxes| {
            assert_eq!(muxes.len(), 2, "a mux per chain");
            c.link.record(muxes)
        },
    )
    .expect("the capture runs");
    assert_eq!(ran.triaged.len(), 2);
    for (port, (t, kept, picks)) in [
        (&ran.triaged[0], &kept0, &chains[0].analyser),
        (&ran.triaged[1], &kept1, &chains[1].analyser),
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
    let chains: Vec<Chain<MemTapes, Picks>> = Vec::new();
    let ran = run(
        OwnerId([0xe7; 32]),
        c.chain_options(),
        c.window,
        chains,
        move |muxes| c.link.record(muxes),
    );
    assert!(matches!(ran, Err(RunError::NoChain)), "{ran:?}");
}

#[test]
fn a_reader_follows_the_window_while_it_runs() {
    let c = RamCapture {
        triage_threads: 2,
        link: Link {
            chunk_bytes: 16 << 10,
            ..Link::full().micros(2_000)
        },
        chains: 1,
        piece_bytes: 64 << 10,
        window: Window::default(),
    };
    // the analyser waits for the reader, so the first pieces stay in memory until it read them
    let (go_tx, go) = mpsc::channel();
    let running = start(
        OwnerId([0xe7; 32]),
        c.chain_options(),
        c.window,
        vec![Chain {
            kept: MemTapes::new(),
            analyser: Picks {
                keep: fifth,
                go: Some(go),
                kept: Vec::new(),
            },
        }],
        move |muxes| c.link.record(muxes),
    )
    .expect("the capture starts");
    assert_eq!(running.chains().len(), 1, "a chain per port");
    let chain = running.chains()[0].clone();
    let mut version = 0;
    let progress = loop {
        let p = chain.watch.wait_past(version);
        version = p.version;
        if p.piece >= 2 || p.ended {
            break p;
        }
    };
    assert!(progress.piece >= 2, "two pieces closed: {}", progress.piece);
    let frames: Vec<_> = c.link.rxs().map(frames_id).collect();
    for seq in 0..2 {
        let piece = chain
            .pieces
            .closed(&seq)
            .expect("the window")
            .expect("a closed piece, not yet triaged");
        let r = TapeReader::open(piece).expect("a piece in memory reads alone");
        let chunks = r.entries(0).expect("the index");
        assert!(
            chunks.iter().any(|e| frames.contains(&e.track_id)),
            "piece {seq} holds frames"
        );
    }
    go_tx.send(()).expect("the analyser waits");
    let (ran, _) = running.join().expect("the capture runs");
    let t = &ran.triaged[0];
    assert_eq!(t.looked, t.pieces, "every piece read");
    assert!(t.overrun.is_empty(), "{t:?}");
    assert!(
        chain.pieces.keys().expect("the keys").is_empty(),
        "every piece left memory"
    );
}
