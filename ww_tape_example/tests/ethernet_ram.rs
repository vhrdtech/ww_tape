//! The analyser capturing into memory: the pieces around each TCP reset kept and readable alone, every reset found,
//! and the results' chain holding what the analyser found and kept.
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

use ww_tape::payloads::Payloads;
use ww_tape::times::{EventTimes, RelationColumn, StoredRelations};
use ww_tape::{
    BlockKind, ByteSource, ChainOptions, ClockNum, MemTapes, OwnerId, TapeReader, TrackDef,
    TrackNum,
};
use ww_tape_example::ethernet::ram::{RamCapture, RamRun, Reset, kept_track, resets_spec};
use ww_tape_example::ethernet::{self, Link, Rx, Traffic, decode_frame, frames_id};
use ww_tape_example::window::Window;
use ww_tape_mux::{MuxOptions, TapeMux};

/// 5 ms of the link, a reset in every 5 000 frames, chunks of 16 KiB of frames, pieces of 128 KiB.
fn capture() -> RamCapture {
    RamCapture {
        link: Link {
            chunk_bytes: 16 << 10,
            ..Link::full().micros(5_000).queues(2).resets(5_000)
        },
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

/// Each kept piece reads alone, its frames those the link sent with those numbers, and each reset's frame lies in
/// the piece the analyser found it in.
fn kept_pieces_hold_their_frames(c: &RamCapture, run: &RamRun, kept: &MemTapes) {
    let t = &run.triaged;
    let truth = traffic(c.link);
    let mut kept_frames = BTreeMap::new();
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
            kept_frames.insert(k, *seq);
        }
    }
    assert_eq!(kept_bytes, t.kept_bytes);
    for r in &run.resets {
        assert_eq!(
            kept_frames.get(&(r.rx.port, r.rx.queue, r.frame)),
            Some(&r.piece),
            "the frame of {r:?} lies in its kept piece"
        );
        let (_, frame) = &truth[&(r.rx.port, r.rx.queue)][r.frame as usize];
        let (ipv4, l4) = decode_frame(frame).expect("an IPv4 frame");
        assert_eq!((r.ipv4, Some(r.l4)), (ipv4, l4));
    }
}

#[test]
fn the_window_keeps_the_pieces_around_each_reset() {
    let c = capture();
    let mut kept = MemTapes::new();
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
    let run = c.run(&mut kept, &results).expect("the capture runs");
    let t = &run.triaged;
    assert!(t.pieces > 40, "{} pieces", t.pieces);
    assert_eq!(t.looked, t.pieces, "every piece read");
    assert!(t.overrun.is_empty(), "the window never filled");
    assert!(t.held_max <= c.window.bytes_max);

    // every reset sent is found, in the piece that holds it
    let found: BTreeSet<_> = run
        .resets
        .iter()
        .map(|r| (r.tick, r.rx.port, r.rx.queue, r.frame))
        .collect();
    let sent = resets_sent(c.link);
    assert!(sent.len() >= 5, "{} resets", sent.len());
    assert_eq!(found, sent);

    // the pieces kept are those with a reset and their neighbours
    let hits: BTreeSet<u64> = run.resets.iter().map(|r| r.piece).collect();
    let want: Vec<u64> = (0..t.pieces)
        .filter(|&p| (p.saturating_sub(1)..=p + 1).any(|q| hits.contains(&q)))
        .collect();
    assert_eq!(t.kept, want);
    assert!(
        t.kept.len() < t.pieces as usize,
        "{} of {} kept",
        t.kept.len(),
        t.pieces
    );
    assert_eq!(kept.keys().expect("the keys"), t.kept);

    kept_pieces_hold_their_frames(&c, &run, &kept);

    // the results' chain holds the resets and the pieces kept
    let closed = results.close().expect("the results close");
    let pieces = ethernet::pieces(&results_tapes, &closed).expect("the results' pieces");
    let (resets, kept_items) = results_of(&pieces, c.link);
    let mut found: Vec<Reset> = run
        .resets
        .iter()
        .map(|r| Reset { piece: 0, ..*r })
        .collect();
    found.sort_by_key(|r| (r.tick, r.rx.port, r.rx.queue));
    assert_eq!(resets, found);
    let mut seqs: Vec<u64> = kept_items.iter().map(|k| k.0).collect();
    seqs.sort_unstable();
    assert_eq!(seqs, t.kept, "a `kept` item per piece kept");
    assert!(kept_items.iter().all(|k| k.1 <= k.2), "spans");
    assert!(kept_items.is_sorted_by_key(|k| k.1), "in time order");
}

/// The `resets` items of the results' chain in time order, and each `kept` item's piece, start and end.
fn results_of(pieces: &[Vec<u8>], link: Link) -> (Vec<Reset>, Vec<(u64, i64, i64)>) {
    let (mut resets, mut kept) = (Vec::new(), Vec::new());
    let mut buf = Vec::new();
    let mut tracks: Vec<(Option<Rx>, TrackDef<'static>)> = Vec::new();
    for rx in link.rxs() {
        let relation = vec![ethernet::frame_relation(rx)].leak();
        tracks.push((
            Some(rx),
            resets_spec(rx, ClockNum::MIN, relation).numbered(TrackNum::MIN),
        ));
    }
    tracks.push((None, kept_track(ClockNum::MIN).numbered(TrackNum::MIN)));
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
                    if let Some(rx) = rx {
                        let frames = RelationColumn::read(&chunk, StoredRelations::of(def), 0)
                            .expect("the relation")
                            .expect("frame");
                        let r = Reset::from_bytes(*rx, times.start(j), frames.get(j), p);
                        resets.push(r.expect("a reset"));
                    } else {
                        let seq = u64::from_le_bytes(p.try_into().expect("8 bytes"));
                        kept.push((seq, times.start(j), times.end(j)));
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
        link: capture().link.resets(0),
        ..capture()
    };
    let mut kept = MemTapes::new();
    let results = TapeMux::create(
        MemTapes::new(),
        OwnerId([0xe8; 32]),
        0,
        &[],
        ChainOptions::default(),
        MuxOptions::default(),
    )
    .expect("the results' mux");
    let run = c.run(&mut kept, &results).expect("the capture runs");
    assert!(run.resets.is_empty());
    assert!(run.triaged.kept.is_empty());
    assert_eq!(run.triaged.looked, run.triaged.pieces);
    assert!(kept.keys().expect("the keys").is_empty());
    results.close().expect("the results close");
}

/// The kept store's bytes are each piece whole: what reads at its end is its tail.
#[test]
fn a_kept_piece_ends_on_its_tail() {
    let c = capture();
    let mut kept = MemTapes::new();
    let results = TapeMux::create(
        MemTapes::new(),
        OwnerId([0xe8; 32]),
        0,
        &[],
        ChainOptions::default(),
        MuxOptions::default(),
    )
    .expect("the results' mux");
    let run = c.run(&mut kept, &results).expect("the capture runs");
    for seq in &run.triaged.kept {
        let tape = kept.closed(seq).expect("the store").expect("a kept piece");
        let r = TapeReader::open(tape.clone()).expect("it opens");
        assert_eq!(r.pieces().len(), 1);
        let p = &r.pieces()[0];
        assert!(p.close.is_some() && p.tail.is_some(), "piece {seq} closed");
        assert_eq!(p.end, tape.size().expect("its size"));
    }
    results.close().expect("the results close");
}
