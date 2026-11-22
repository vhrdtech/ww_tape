//! The wideband receiver against what the book says of it: every channel read back from chains of several muxes,
//! and the bursts found from the summaries alone.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "tests fail by panicking, compute expected values freely"
)]

use ww_tape::{MemTapes, OwnerId};
use ww_tape_example::ethernet::pieces;
use ww_tape_example::rf::{BURSTS_MS, Signal};
use ww_tape_example::wideband::{Source, Wideband, overview, read_channel};
use ww_tape_mux::{MuxOptions, TapeMux};

/// Record `wb` live through `muxes` muxes in memory: each chain's pieces.
fn record(wb: Wideband, muxes: usize) -> Vec<Vec<Vec<u8>>> {
    let tapes: Vec<MemTapes> = (0..muxes).map(|_| MemTapes::new()).collect();
    let m: Vec<_> = tapes
        .iter()
        .enumerate()
        .map(|(chain, t)| {
            TapeMux::create(
                t.clone(),
                OwnerId([0x5d; 32]),
                chain as u64,
                &[],
                wb.chain_options(),
                MuxOptions::default(),
            )
            .expect("a mux starts")
        })
        .collect();
    let refs: Vec<_> = m.iter().collect();
    let recorded = wb
        .record(&refs, &Source::Live)
        .expect("the capture records");
    assert!(
        recorded.samples.iter().all(|&n| n == wb.channel_samples()),
        "every channel pushed its samples"
    );
    m.into_iter()
        .zip(&tapes)
        .map(|(mux, t)| pieces(t, &mux.close().expect("closes")).expect("pieces"))
        .collect()
}

#[test]
fn every_channel_reads_back_from_its_chain() {
    let wb = Wideband::scaled(100_000)
        .expect("divides")
        .ms(3_000)
        .channels(4);
    let chains = record(wb, 2);
    for k in 0..wb.channels {
        let mut want = vec![0; (wb.channel_samples() * 2) as usize];
        Signal::channel(wb.channel_rate(), k).fill(0, &mut want);
        let chain = &chains[k as usize % 2];
        assert_eq!(read_channel(chain, k).expect("reads"), want, "channel {k}");
        let other = &chains[(k as usize + 1) % 2];
        assert!(
            read_channel(other, k).expect("reads").is_empty(),
            "channel {k} only in its own chain"
        );
    }
}

#[test]
fn channels_differ_by_their_noise_only() {
    let rate = 125_000;
    let mut a = vec![0; 20_000];
    let mut b = vec![0; 20_000];
    Signal::channel(rate, 0).fill(0, &mut a);
    Signal::channel(rate, 1).fill(0, &mut b);
    let mut c = vec![0; 20_000];
    Signal::new(rate).fill(0, &mut c);
    assert_eq!(a, c, "channel 0 is the RF capture's signal");
    let close = a
        .iter()
        .zip(&b)
        .filter(|(x, y)| (i32::from(**x) - i32::from(**y)).abs() <= 40)
        .count();
    assert!(a != b && close == a.len(), "the same carriers, other noise");
}

#[test]
fn the_overview_finds_the_bursts_from_the_summaries() {
    // 10 kS/s a channel: a level-1 node holds 16 384 samples, 1.6 s
    let wb = Wideband::scaled(12_500)
        .expect("divides")
        .ms(60_000)
        .channels(8);
    assert_eq!(wb.channel_rate(), 10_000);
    let chains = record(wb, 8);
    let bursts: Vec<i64> = BURSTS_MS
        .iter()
        .map(|&ms| i64::from(ms) * 1_000_000)
        .collect();
    for k in 0..wb.channels {
        let chain = &chains[k as usize];
        let o = overview(&wb, chain.iter().map(Vec::as_slice), k, 1).expect("the overview");
        let samples: u64 = o.nodes.iter().map(|n| n.samples).sum();
        assert_eq!(
            samples,
            wb.channel_samples(),
            "channel {k}: the nodes cover the minute"
        );
        let quiet = o.nodes.iter().map(|n| n.power_max).fold(f32::MAX, f32::min);
        // a node's span in ns at 10 kS/s
        let span = |n: &ww_tape_example::wideband::OverviewNode| {
            n.first_tick..n.first_tick + n.samples.cast_signed() * 100_000
        };
        for n in &o.nodes {
            let holds = bursts.iter().any(|b| span(n).contains(b));
            assert_eq!(
                n.power_max > 1.5 * quiet,
                holds,
                "channel {k}: the node at {} ns stands out where it holds a burst",
                n.first_tick
            );
        }
        assert!(
            o.bytes_read < chain.iter().map(|p| p.len() as u64).sum(),
            "no sample read"
        );
    }
}
