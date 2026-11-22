//! The analyser's capture against what the book says of it: every frame and header read back, each header matched to
//! its frame, and the bytes the tracks take.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::print_stdout,
    reason = "tests fail by panicking, compute expected values freely and print what they saw"
)]

use ww_tape::{ChainOptions, MemTapes, OwnerId};
use ww_tape_example::ethernet::{self, Link, Rx, Traffic, digest};
use ww_tape_mux::{MuxOptions, TapeMux};

/// Record `link` into one chain in memory: what it sent, and the chain read back.
fn record(link: Link) -> (ethernet::Captured, ethernet::ReadBack) {
    let tapes = MemTapes::new();
    let mux = TapeMux::create(
        tapes.clone(),
        OwnerId([0xe7; 32]),
        0,
        &[],
        ChainOptions::default(),
        MuxOptions::default(),
    )
    .expect("the mux starts");
    let captured = link.record(&[&mux]).expect("the link records");
    let closed = mux.close().expect("the chain closes");
    let pieces = ethernet::pieces(&tapes, &closed).expect("the pieces");
    (
        captured,
        ethernet::read(&pieces, link.queues).expect("the chain reads"),
    )
}

#[test]
fn every_frame_and_header_reads_back() {
    for queues in [1, 3] {
        let link = Link::full().micros(2_000).queues(queues);
        let (captured, read) = record(link);
        for port in [0, 1] {
            let (sent, got) = (&captured.ports[port], &read.ports[port]);
            let traffic: Vec<_> = (0..queues)
                .map(|queue| {
                    Traffic::new(
                        link,
                        Rx {
                            port: port as u32,
                            queue,
                        },
                    )
                    .all()
                })
                .collect();
            let frames: Vec<_> = traffic.iter().flatten().collect();
            assert_eq!(got.frames, frames.len() as u64, "port {port}: every frame");
            assert_eq!(
                sent.frames, got.frames,
                "port {port}: what the threads sent"
            );
            let digests: Vec<_> = traffic
                .iter()
                .map(|q| digest(q.iter().map(|(t, b)| (*t, b.as_slice()))))
                .collect();
            assert_eq!(
                got.digests, digests,
                "port {port}: the same ticks and bytes"
            );
            let ip = frames
                .iter()
                .filter(|(_, f)| ethernet::decode_frame(f).is_some())
                .count() as u64;
            assert_eq!(
                (got.ipv4, sent.ipv4),
                (ip, ip),
                "port {port}: a header per IPv4 frame"
            );
            assert!(ip < got.frames, "port {port}: some frames are ARP");
            assert_eq!(
                (got.l4, sent.l4),
                (ip, ip),
                "port {port}: TCP or UDP on each"
            );
            assert_eq!(
                got.matched,
                got.ipv4 + got.l4,
                "port {port}: each header decodes from the frame its relation names"
            );
        }
    }
}

#[test]
fn the_queues_split_the_ports_frames() {
    let link = Link::full().micros(500);
    let one = Traffic::new(link.queues(1), Rx { port: 0, queue: 0 }).all();
    let mut split: Vec<_> = (0..4)
        .flat_map(|queue| Traffic::new(link.queues(4), Rx { port: 0, queue }).all())
        .collect();
    split.sort_by_key(|(t, _)| *t);
    assert_eq!(
        split, one,
        "four queues hold the frames of one, at the same ticks"
    );
}

#[test]
fn a_mux_per_port_writes_two_chains() {
    let link = Link::full().micros(1_000);
    let tapes = [MemTapes::new(), MemTapes::new()];
    let muxes: Vec<_> = (0..2)
        .map(|chain| {
            TapeMux::create(
                tapes[chain].clone(),
                OwnerId([0xe7; 32]),
                chain as u64,
                &[],
                ChainOptions::default(),
                MuxOptions::default(),
            )
            .expect("a mux starts")
        })
        .collect();
    let captured = link
        .record(&[&muxes[0], &muxes[1]])
        .expect("the link records");
    let mut pieces = Vec::new();
    for (mux, tapes) in muxes.into_iter().zip(&tapes) {
        let chain = ethernet::pieces(tapes, &mux.close().expect("closes")).expect("pieces");
        let alone = ethernet::read(&chain, link.queues).expect("a chain reads alone");
        assert_eq!(
            alone.ports.iter().filter(|p| p.frames > 0).count(),
            1,
            "each chain holds one port"
        );
        pieces.extend(chain);
    }
    let read = ethernet::read(&pieces, link.queues).expect("both chains read");
    for port in [0, 1] {
        assert_eq!(read.ports[port].frames, captured.ports[port].frames);
        assert_eq!(read.ports[port].matched, 2 * captured.ports[port].ipv4);
    }
}

#[test]
fn the_link_runs_at_its_load() {
    for load in [100, 40] {
        let link = Link::full().micros(1_000).load(load);
        let traffic: Vec<_> = (0..link.queues)
            .flat_map(|queue| Traffic::new(link, Rx { port: 0, queue }).all())
            .collect();
        let wire: u64 = traffic
            .iter()
            .map(|(_, f)| f.len() as u64 + ethernet::WIRE_EXTRA_BYTES)
            .sum();
        // a ms at 10 Gbps is 1.25 MB on the wire
        let percent = wire as f64 / 1_250_000.0 * 100.0;
        assert!(
            (percent - load as f64).abs() < 0.2,
            "{load} %: {percent:.2} % of the line"
        );
        let last = traffic.iter().map(|f| f.0).max().expect("frames");
        assert!(
            (990_000..1_000_000).contains(&last),
            "the last frame starts at {last} ns"
        );
    }
}

#[test]
fn decoded_headers_say_what_the_frame_holds() {
    let traffic = Traffic::new(Link::full().micros(100).queues(1), Rx { port: 1, queue: 0 }).all();
    let (mut tcp, mut udp) = (0, 0);
    for (_, f) in &traffic {
        let Some((ip, l4)) = ethernet::decode_frame(f) else {
            continue;
        };
        assert_eq!(usize::from(ip.len) + 14, f.len(), "the IPv4 length");
        assert_eq!(ip.ttl, 57, "port 1 is the servers' side");
        let l4 = l4.expect("TCP or UDP");
        let header = if l4.proto == 6 { 54 } else { 42 };
        assert_eq!(
            usize::from(l4.payload) + header,
            f.len(),
            "the payload's length"
        );
        if l4.proto == 6 {
            tcp += 1;
            assert!([80, 443].contains(&l4.src_port), "a server's port");
        } else {
            udp += 1;
        }
    }
    assert!(tcp > 2 * udp && udp > 0, "{tcp} TCP and {udp} UDP frames");
}
