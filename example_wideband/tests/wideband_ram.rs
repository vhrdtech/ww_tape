//! The receiver captured into memory: every burst found from the summaries, the pieces around it kept and holding
//! the signal's samples, and the results' chain holding what the analyser found and kept.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "tests fail by panicking and compute expected values freely"
)]

use std::collections::BTreeMap;

use example_capture::pieces;
use example_capture::rf::{BURST_MS, BURSTS_MS, Signal, iq_shape};
use example_wideband::ram::{Burst, RfCapture, RfRun, bursts_spec, kept_track};
use example_wideband::{Source, channel_id};
use ww_tape::payloads::Payloads;
use ww_tape::samples::Samples;
use ww_tape::times::EventTimes;
use ww_tape::{
    BlockKind, ChainOptions, ClockNum, ColumnKind, MemTapes, OwnerId, TapeReader, TrackNum,
};
use ww_tape_mux::{MuxOptions, TapeMux};

/// A minute at 1/5 000 of the rate: 2 channels of 25 kS/s, pieces of 13 KB.
fn capture() -> RfCapture {
    RfCapture::scaled(5_000).expect("5 000 divides the rate")
}

/// Run `c` live: what it did, the kept store and the results' pieces.
fn run(c: &RfCapture) -> (RfRun, MemTapes, Vec<Vec<u8>>) {
    let mut kept = MemTapes::new();
    let results_tapes = MemTapes::new();
    let results = TapeMux::create(
        results_tapes.clone(),
        OwnerId([0x5e; 32]),
        0,
        &[],
        ChainOptions::default(),
        MuxOptions::default(),
    )
    .expect("the results' mux");
    let run = c
        .run(&Source::Live, &mut kept, &results)
        .expect("the capture runs");
    let closed = results.close().expect("the results close");
    let results = pieces(&results_tapes, &closed).expect("the results' pieces");
    (run, kept, results)
}

/// Each true burst's span in ns: from its first sample to its end.
fn true_bursts() -> Vec<(i64, i64)> {
    BURSTS_MS
        .iter()
        .map(|&ms| {
            let start = i64::from(ms) * 1_000_000;
            (start, start + i64::from(BURST_MS) * 1_000_000)
        })
        .collect()
}

#[test]
fn every_burst_is_found_and_its_pieces_kept() {
    let c = capture();
    let (run, kept, results) = run(&c);
    let t = &run.triaged;
    assert!(t.pieces > 200, "{} pieces", t.pieces);
    assert_eq!(t.looked, t.pieces);
    assert!(t.overrun.is_empty());

    // on each channel, one burst found around each true one, and none elsewhere
    for k in 0..c.wb.channels {
        let found: Vec<&Burst> = run.bursts.iter().filter(|b| b.channel == k).collect();
        assert_eq!(found.len(), BURSTS_MS.len(), "channel {k}: {found:?}");
        for (b, (start, end)) in found.iter().zip(true_bursts()) {
            assert!(
                b.start <= start && end <= b.end,
                "channel {k}: {b:?} holds {start}..{end}"
            );
            assert!(b.power_max >= c.power_min);
        }
    }

    // only a small part of the minute reaches the kept store
    assert!(!t.kept.is_empty());
    assert!(
        t.kept_bytes * 10 < t.piece_bytes,
        "{} of {} bytes kept",
        t.kept_bytes,
        t.piece_bytes
    );
    assert_eq!(kept.keys().expect("the keys"), t.kept);

    // each kept piece reads alone and holds the signal's samples, and every burst's samples are kept
    let mut covered: BTreeMap<u32, Vec<(u64, u64)>> = BTreeMap::new();
    for seq in &t.kept {
        let piece = kept
            .closed(seq)
            .expect("the store")
            .expect("a kept piece")
            .to_vec()
            .expect("its bytes");
        for k in 0..c.wb.channels {
            covered
                .entry(k)
                .or_default()
                .extend(samples_match(&piece, &c, k));
        }
    }
    let rate = u64::from(c.wb.channel_rate());
    for (k, ranges) in &covered {
        for (start, end) in true_bursts() {
            let (s0, s1) = (
                start as u64 * rate / 1_000_000_000,
                end as u64 * rate / 1_000_000_000,
            );
            assert!(
                (s0..s1).all(|n| ranges.iter().any(|r| (r.0..r.1).contains(&n))),
                "channel {k}: the samples of the burst at {start} ns are kept"
            );
        }
    }

    // the results' chain holds the bursts and the pieces kept
    let (bursts, kept_seqs) = results_of(&results, &c);
    let mut want: Vec<(u32, i64, i64)> = run
        .bursts
        .iter()
        .map(|b| (b.channel, b.start, b.end))
        .collect();
    want.sort_unstable();
    assert_eq!(bursts, want);
    assert_eq!(kept_seqs, t.kept);
}

/// The ranges of item numbers of channel `k`'s chunks in `piece`, each chunk's samples checked against the signal.
fn samples_match(piece: &[u8], c: &RfCapture, k: u32) -> Vec<(u64, u64)> {
    let reader = TapeReader::open(piece).expect("a kept piece reads alone");
    let signal = Signal::channel(c.wb.channel_rate(), k);
    let mut out = Vec::new();
    let mut buf = Vec::new();
    for entry in reader.track_entries(0, channel_id(k)).expect("the index") {
        if entry.kind != BlockKind::Series {
            continue;
        }
        let block = reader.read_block(0, &entry, &mut buf).expect("a chunk");
        let chunk = block.chunk().expect("a chunk");
        let count = chunk.head().count as usize;
        let column = chunk.find(ColumnKind::Samples, 0).expect("samples");
        let s = Samples::of(&column, iq_shape(), count).expect("the samples");
        let mut got = vec![0; count * 2];
        s.decode(0, &mut got).expect("they decode");
        let first = entry.first_item.get();
        let mut want = vec![0; count * 2];
        signal.fill(first, &mut want);
        assert_eq!(got, want, "channel {k}, items from {first}");
        out.push((first, first + count as u64));
    }
    out
}

/// The `bursts` items of the results' chain as (channel, start, end), sorted, and the pieces the `kept` items name,
/// sorted.
fn results_of(pieces: &[Vec<u8>], c: &RfCapture) -> (Vec<(u32, i64, i64)>, Vec<u64>) {
    let (mut bursts, mut kept) = (Vec::new(), Vec::new());
    let mut buf = Vec::new();
    let mut ids: Vec<(Option<u32>, ww_tape::TrackId)> = (0..c.wb.channels)
        .map(|k| {
            let def = bursts_spec(k, ClockNum::MIN).numbered(TrackNum::MIN);
            (Some(k), def.id())
        })
        .collect();
    ids.push((None, kept_track(ClockNum::MIN).numbered(TrackNum::MIN).id()));
    for piece in pieces {
        let reader = TapeReader::open(piece.as_slice()).expect("a results' piece");
        for (k, id) in &ids {
            for entry in reader.track_entries(0, *id).expect("the index") {
                if entry.kind != BlockKind::Events {
                    continue;
                }
                let block = reader.read_block(0, &entry, &mut buf).expect("a chunk");
                let chunk = block.chunk().expect("a chunk");
                let times = EventTimes::read(&chunk).expect("the times");
                let payloads = Payloads::read(&chunk).expect("payloads").expect("payloads");
                for j in 0..times.len() {
                    let p = payloads.get(j).expect("a payload");
                    match k {
                        Some(k) => bursts.push((*k, times.start(j), times.end(j))),
                        None => kept.push(u64::from_le_bytes(p.try_into().expect("8 bytes"))),
                    }
                }
            }
        }
    }
    bursts.sort_unstable();
    kept.sort_unstable();
    (bursts, kept)
}

/// A band quiet past the detector's level keeps nothing.
#[test]
fn a_detector_above_the_bursts_keeps_nothing() {
    let c = RfCapture {
        power_min: 1e9,
        wb: capture().wb.ms(10_000),
        ..capture()
    };
    let (run, kept, _) = run(&c);
    assert!(run.bursts.is_empty());
    assert!(run.triaged.kept.is_empty());
    assert!(kept.keys().expect("the keys").is_empty());
}
