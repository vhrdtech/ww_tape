//! The analyser capturing into memory, in one chain or a chain per port: the pieces around each TCP reset kept and
//! readable alone, every reset found, and the results' chain holding what the analysers found and kept.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "tests fail by panicking and compute expected values freely"
)]

use std::collections::{BTreeMap, BTreeSet};

use example_capture::window::Window;
use example_ethernet::ram::{RamCapture, RamError, RamRun, Reset, kept_track, resets_spec};
use example_ethernet::{Link, Rx, Traffic, decode_frame, frames_id};
use ww_tape::payloads::Payloads;
use ww_tape::times::{EventTimes, RelationColumn, StoredRelations};
use ww_tape::{
    BlockKind, ByteSource, ChainOptions, ClockNum, MemTapes, OwnerId, TapeReader, TrackDef,
    TrackNum,
};
use ww_tape_mux::{MuxOptions, TapeMux};

/// 5 ms of the link in `chains` chains, a reset in every 5 000 frames, chunks of 16 KiB of frames, pieces of
/// 128 KiB.
fn capture(chains: u32) -> RamCapture {
    RamCapture {
        triage_threads: 2,
        link: Link {
            chunk_bytes: 16 << 10,
            ..Link::full().micros(5_000).queues(2).resets(5_000)
        },
        chains,
        piece_bytes: 128 << 10,
        window: Window {
            bytes_max: 1 << 30,
            before: 1,
            after: 1,
        },
    }
}

/// Frames by receive queue, each its tick and bytes.
type ByQueue = BTreeMap<(u32, u32), Vec<(i64, Vec<u8>)>>;

/// Every frame of every receive queue of `link`.
fn traffic(link: Link) -> ByQueue {
    link.rxs()
        .map(|rx| ((rx.port, rx.queue), Traffic::new(link, rx).all()))
        .collect()
}

/// Every reset `link` sends, as the analyser reports it but for its piece.
fn resets_sent(link: Link) -> BTreeSet<(i64, u32, u32, u64)> {
    let mut out = BTreeSet::new();
    for ((port, queue), frames) in traffic(link) {
        for (n, (tick, bytes)) in frames.iter().enumerate() {
            if let Some((_, Some(l4))) = decode_frame(bytes)
                && l4.is_reset()
            {
                out.insert((*tick, port, queue, n as u64));
            }
        }
    }
    out
}

/// The frames of every queue a piece holds, by queue and item number.
fn frames_of(piece: &[u8], link: Link) -> BTreeMap<(u32, u32, u64), (i64, Vec<u8>)> {
    let reader = TapeReader::open(piece).expect("a kept piece reads alone");
    let mut out = BTreeMap::new();
    let mut buf = Vec::new();
    for rx in link.rxs() {
        for entry in reader.track_entries(0, frames_id(rx)).expect("the index") {
            if entry.kind != BlockKind::Events {
                continue;
            }
            let block = reader.read_block(0, &entry, &mut buf).expect("a chunk");
            let chunk = block.chunk().expect("a chunk");
            let times = EventTimes::read(&chunk).expect("the times");
            let payloads = Payloads::read(&chunk).expect("payloads").expect("payloads");
            for j in 0..times.len() {
                let item = entry.first_item.get() + j as u64;
                let bytes = payloads.get(j).expect("a frame").to_vec();
                out.insert((rx.port, rx.queue, item), (times.start(j), bytes));
            }
        }
    }
    out
}

/// Each kept piece reads alone, its frames those the link sent with those numbers and of its chain's ports, and each
/// reset's frame lies in the piece the analyser found it in.
fn kept_pieces_hold_their_frames(c: &RamCapture, run: &RamRun, kept: &[MemTapes]) {
    let truth = traffic(c.link);
    let mut kept_frames = BTreeMap::new();
    for (chain, (t, kept)) in (0..).zip(run.triaged.iter().zip(kept)) {
        let mut kept_bytes = 0;
        for seq in &t.kept {
            let piece = kept
                .closed(seq)
                .expect("the store")
                .expect("a kept piece")
                .to_vec()
                .expect("its bytes");
            kept_bytes += piece.len() as u64;
            for (k, frame) in frames_of(&piece, c.link) {
                assert_eq!(truth[&(k.0, k.1)][k.2 as usize], frame, "frame {k:?}");
                assert_eq!(k.0 % c.chains, chain, "frame {k:?} in chain {chain}");
                kept_frames.insert(k, (chain, *seq));
            }
        }
        assert_eq!(kept_bytes, t.kept_bytes);
    }
    for r in &run.resets {
        assert_eq!(
            kept_frames.get(&(r.rx.port, r.rx.queue, r.frame)),
            Some(&(r.chain, r.piece)),
            "the frame of {r:?} lies in its kept piece"
        );
        let (_, frame) = &truth[&(r.rx.port, r.rx.queue)][r.frame as usize];
        let (ipv4, l4) = decode_frame(frame).expect("an IPv4 frame");
        assert_eq!((r.ipv4, Some(r.l4)), (ipv4, l4));
    }
}

#[test]
fn the_window_keeps_the_pieces_around_each_reset() {
    keeps_the_pieces_around_each_reset(1);
}

#[test]
fn a_chain_per_port_keeps_the_pieces_around_each_reset() {
    keeps_the_pieces_around_each_reset(2);
}

fn keeps_the_pieces_around_each_reset(chains: u32) {
    let c = capture(chains);
    let kept: Vec<MemTapes> = (0..chains).map(|_| MemTapes::new()).collect();
    let results_tapes = MemTapes::new();
    let results = TapeMux::create(
        results_tapes.clone(),
        OwnerId([0xe8; 32]),
        0,
        &[],
        ChainOptions::default(),
        MuxOptions::default(),
    )
    .expect("the results' mux");
    let run = c.run(kept.to_vec(), &results).expect("the capture runs");
    assert_eq!(run.triaged.len(), chains as usize);
    let pieces: u64 = run.triaged.iter().map(|t| t.pieces).sum();
    assert!(pieces > 40, "{pieces} pieces");
    for t in &run.triaged {
        assert_eq!(t.looked, t.pieces, "every piece read");
        assert!(t.overrun.is_empty(), "the window never filled");
        assert!(t.held_max <= c.window.bytes_max);
    }

    // every reset sent is found, in the piece that holds it
    let found: BTreeSet<_> = run
        .resets
        .iter()
        .map(|r| (r.tick, r.rx.port, r.rx.queue, r.frame))
        .collect();
    let sent = resets_sent(c.link);
    assert!(sent.len() >= 5, "{} resets", sent.len());
    assert_eq!(found, sent);

    // the pieces kept are those with a reset and their neighbours, in the reset's chain
    for (chain, (t, kept)) in (0..).zip(run.triaged.iter().zip(&kept)) {
        let hits: BTreeSet<u64> = run
            .resets
            .iter()
            .filter(|r| r.chain == chain)
            .map(|r| r.piece)
            .collect();
        assert!(
            run.resets.iter().all(|r| r.chain == r.rx.port % chains),
            "a reset in its port's chain"
        );
        let want: Vec<u64> = (0..t.pieces)
            .filter(|&p: &u64| (p.saturating_sub(1)..=p + 1).any(|q| hits.contains(&q)))
            .collect();
        assert_eq!(t.kept, want, "chain {chain}");
        assert!(
            t.kept.len() < t.pieces as usize,
            "{} of {} kept",
            t.kept.len(),
            t.pieces
        );
        assert_eq!(kept.keys().expect("the keys"), t.kept);
    }

    kept_pieces_hold_their_frames(&c, &run, &kept);

    // the results' chain holds the resets and the pieces kept
    let closed = results.close().expect("the results close");
    let pieces = example_capture::pieces(&results_tapes, &closed).expect("the results' pieces");
    let (resets, kept_items) = results_of(&pieces, c.link, chains);
    let mut found: Vec<Reset> = run
        .resets
        .iter()
        .map(|r| Reset {
            chain: 0,
            piece: 0,
            ..*r
        })
        .collect();
    found.sort_by_key(|r| (r.tick, r.rx.port, r.rx.queue));
    assert_eq!(resets, found);
    for (t, items) in run.triaged.iter().zip(&kept_items) {
        let mut seqs: Vec<u64> = items.iter().map(|k| k.0).collect();
        seqs.sort_unstable();
        assert_eq!(seqs, t.kept, "a `kept` item per piece kept");
        assert!(items.iter().all(|k| k.1 <= k.2), "spans");
        assert!(items.is_sorted_by_key(|k| k.1), "in time order");
    }
}

/// Pieces kept of a chain, as its `kept` items say: each its sequence number, start and end.
type KeptItems = Vec<(u64, i64, i64)>;

/// The `resets` items of the results' chain in time order, and the `kept` items of each of `chains` chains.
fn results_of(pieces: &[Vec<u8>], link: Link, chains: u32) -> (Vec<Reset>, Vec<KeptItems>) {
    let mut resets = Vec::new();
    let mut kept = vec![Vec::new(); chains as usize];
    let mut buf = Vec::new();
    let mut tracks: Vec<(Result<Rx, u32>, TrackDef<'static>)> = Vec::new();
    for rx in link.rxs() {
        let relation = vec![example_ethernet::frame_relation(rx)].leak();
        tracks.push((
            Ok(rx),
            resets_spec(rx, ClockNum::MIN, relation).numbered(TrackNum::MIN),
        ));
    }
    for chain in 0..chains {
        tracks.push((
            Err(chain),
            kept_track(chain, ClockNum::MIN).numbered(TrackNum::MIN),
        ));
    }
    for piece in pieces {
        let reader = TapeReader::open(piece.as_slice()).expect("a results' piece");
        for (rx, def) in &tracks {
            for entry in reader.track_entries(0, def.id()).expect("the index") {
                if entry.kind != BlockKind::Events {
                    continue;
                }
                let block = reader.read_block(0, &entry, &mut buf).expect("a chunk");
                let chunk = block.chunk().expect("a chunk");
                let times = EventTimes::read(&chunk).expect("the times");
                let payloads = Payloads::read(&chunk).expect("payloads").expect("payloads");
                for j in 0..times.len() {
                    let p = payloads.get(j).expect("a payload");
                    match rx {
                        Ok(rx) => {
                            let frames = RelationColumn::read(&chunk, StoredRelations::of(def), 0)
                                .expect("the relation")
                                .expect("frame");
                            let r = Reset::from_bytes(*rx, times.start(j), frames.get(j), p);
                            resets.push(r.expect("a reset"));
                        }
                        Err(chain) => {
                            let seq = u64::from_le_bytes(p.try_into().expect("8 bytes"));
                            kept[*chain as usize].push((seq, times.start(j), times.end(j)));
                        }
                    }
                }
            }
        }
    }
    resets.sort_by_key(|r| (r.tick, r.rx.port, r.rx.queue));
    (resets, kept)
}

/// A link without resets keeps nothing, and every piece leaves memory.
#[test]
fn a_quiet_link_keeps_nothing() {
    let c = RamCapture {
        link: capture(2).link.resets(0),
        ..capture(2)
    };
    let kept = [MemTapes::new(), MemTapes::new()];
    let results = TapeMux::create(
        MemTapes::new(),
        OwnerId([0xe8; 32]),
        0,
        &[],
        ChainOptions::default(),
        MuxOptions::default(),
    )
    .expect("the results' mux");
    let run = c.run(kept.to_vec(), &results).expect("the capture runs");
    assert!(run.resets.is_empty());
    for (t, kept) in run.triaged.iter().zip(&kept) {
        assert!(t.kept.is_empty());
        assert_eq!(t.looked, t.pieces);
        assert!(kept.keys().expect("the keys").is_empty());
    }
    results.close().expect("the results close");
}

/// The kept store's bytes are each piece whole: what reads at its end is its tail.
#[test]
fn a_kept_piece_ends_on_its_tail() {
    let c = capture(1);
    let kept = [MemTapes::new()];
    let results = TapeMux::create(
        MemTapes::new(),
        OwnerId([0xe8; 32]),
        0,
        &[],
        ChainOptions::default(),
        MuxOptions::default(),
    )
    .expect("the results' mux");
    let run = c.run(kept.to_vec(), &results).expect("the capture runs");
    for seq in &run.triaged[0].kept {
        let tape = kept[0]
            .closed(seq)
            .expect("the store")
            .expect("a kept piece");
        let r = TapeReader::open(tape.clone()).expect("it opens");
        assert_eq!(r.pieces().len(), 1);
        let p = &r.pieces()[0];
        assert!(p.close.is_some() && p.tail.is_some(), "piece {seq} closed");
        assert_eq!(p.end, tape.size().expect("its size"));
    }
    results.close().expect("the results close");
}

/// A capture needs a kept store per chain, and one or two chains.
#[test]
fn a_kept_store_per_chain() {
    let results = TapeMux::create(
        MemTapes::new(),
        OwnerId([0xe8; 32]),
        0,
        &[],
        ChainOptions::default(),
        MuxOptions::default(),
    )
    .expect("the results' mux");
    for (chains, stores) in [(2, 1), (1, 2), (0, 0), (3, 3)] {
        let kept: Vec<MemTapes> = (0..stores).map(|_| MemTapes::new()).collect();
        let r = capture(chains).run(kept, &results);
        assert!(
            matches!(r, Err(RamError::Chains)),
            "{chains} chains, {stores} stores"
        );
    }
    results.close().expect("the results close");
}
