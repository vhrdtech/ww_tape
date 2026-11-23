//! The wideband receiver against what the book says of it: every channel read back from chains of several muxes,
//! and the bursts found from the summaries alone.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "tests fail by panicking, compute expected values freely"
)]

use example_capture::about;
use example_capture::pieces;
use example_capture::rf::{BURSTS_MS, Signal};
use example_wideband::{Source, Wideband, overview, read_channel};
use ww_tape::{MemTapes, OwnerId};
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
        let span = |n: &example_wideband::OverviewNode| {
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

/// The text of the book page `name`, its line breaks as spaces.
fn page(name: &str) -> String {
    let path = format!("{}/../docs/{name}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).expect("the page is there");
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn the_page_gives_what_a_second_takes() {
    // one channel of the full receiver's chunks: 50 ms at 1 GS/s, 50 million samples
    let wb = Wideband::full().channels(1).ms(50);
    let chains = record(wb, 1);
    let bytes: u64 = chains[0].iter().map(|p| p.len() as u64).sum();
    let per_sample = bytes as f64 / wb.channel_samples() as f64;
    let full = Wideband::full();
    assert_eq!(
        (full.rate_hz, full.channels, full.channel_rate()),
        (1_000_000_000, 8, 125_000_000)
    );
    let second = about((per_sample * f64::from(full.rate_hz)) as u64);
    let minute = about((per_sample * f64::from(full.rate_hz) * 60.0) as u64);
    // a level-3 node: 2^(10 + 3 * 4) samples of a channel
    let node = 1_u64 << 22;
    let node_ms = (node as f64 / f64::from(full.channel_rate()) * 1e3).round();
    let ram = example_wideband::ram::RfCapture::full();
    let times = f64::from(ram.wb.rate_hz) / f64::from(example_capture::rf::Rf::capture().rate_hz);
    for phrase in [
        format!(
            "At 1 GS/s, a second of the receiver takes about {second}, and a minute about {minute}."
        ),
        format!("a second of the receiver is about {second}"),
        "splits its band into 8 channels of 125 million.".to_owned(),
        format!(
            "The example takes {} million samples a second in {} channels of {} million.",
            ram.wb.rate_hz / 1_000_000,
            ram.wb.channels,
            ram.wb.channel_rate() / 1_000_000
        ),
        format!("A node of level 3 covers 4 194 304 samples, {node_ms} ms of a channel."),
        format!("at {times} times its rate"),
        format!("a piece every {} MiB", ram.wb.piece_bytes >> 20),
    ] {
        assert!(
            page("wideband.md").contains(&phrase),
            "docs/wideband.md says: {phrase}"
        );
    }
    assert_eq!(node, 4_194_304);
}
