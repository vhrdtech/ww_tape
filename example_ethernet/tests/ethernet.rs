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

use example_capture::about;
use example_ethernet::{Link, Rx, Traffic, digest};
use ww_tape::{ChainOptions, MemTapes, OwnerId};
use ww_tape_mux::{MuxOptions, TapeMux};

/// Record `link` into one chain in memory: what it sent, and the chain read back.
fn record(link: Link) -> (example_ethernet::Captured, example_ethernet::ReadBack) {
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
    let pieces = example_capture::pieces(&tapes, &closed).expect("the pieces");
    (
        captured,
        example_ethernet::read(&pieces, link.queues).expect("the chain reads"),
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
                .filter(|(_, f)| example_ethernet::decode_frame(f).is_some())
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
        let chain = example_capture::pieces(tapes, &mux.close().expect("closes")).expect("pieces");
        let alone = example_ethernet::read(&chain, link.queues).expect("a chain reads alone");
        assert_eq!(
            alone.ports.iter().filter(|p| p.frames > 0).count(),
            1,
            "each chain holds one port"
        );
        pieces.extend(chain);
    }
    let read = example_ethernet::read(&pieces, link.queues).expect("both chains read");
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
            .map(|(_, f)| f.len() as u64 + example_ethernet::WIRE_EXTRA_BYTES)
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
        let Some((ip, l4)) = example_ethernet::decode_frame(f) else {
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

/// The text of the book page `name`, its line breaks as spaces.
fn page(name: &str) -> String {
    let path = format!("{}/../docs/{name}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).expect("the page is there");
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn the_page_gives_what_the_link_takes() {
    let sizes = Link::full()
        .micros(10_000)
        .sizes()
        .expect("the link records");
    let (frames, frame_bytes, _) = sizes.frames();
    let per_frame_byte = sizes.read.piece_bytes as f64 / frame_bytes as f64;
    let (_, ipv4) = sizes.track(1);
    let (_, l4) = sizes.track(2);
    let per_header = |b: example_ethernet::TrackBytes, items: u64| {
        (b.chunk_bytes + b.summary_bytes) as f64 / items as f64
    };
    let (n_ip, _) = sizes.track(1);
    for per in [per_header(ipv4, n_ip), per_header(l4, n_ip)] {
        assert_eq!(per.round(), 7.0, "a decoded header takes about 7 bytes");
    }
    // a second is a hundred such 10 ms, to within one piece's head, index and close
    let second = about(sizes.read.piece_bytes * 100);
    let port = about(frame_bytes * 100 / 2);
    // a piece in memory, against what a second of the link takes
    let piece = example_ethernet::ram::RamCapture::full().piece_bytes;
    let piece_ms = (piece as f64 / (sizes.read.piece_bytes * 100) as f64 * 1e3).round();
    for phrase in [
        format!(
            "10 ms of the link hold {} frames of {} in both directions",
            grouped(frames),
            about(frame_bytes)
        ),
        format!("The chain takes {per_frame_byte:.3} bytes per byte of the frames"),
        format!("A second of the link at its line rate therefore takes about {second} on a tape"),
        format!("A chain at the line rate writes about {second} a second"),
        format!("A port's {port} of frames a second on one thread"),
        format!("The [[mux]] cuts a piece every 64 MiB, about {piece_ms} ms of the link"),
        format!(
            "The link sends one in {} million frames of a port",
            example_ethernet::RESET_EVERY / 1_000_000
        ),
    ] {
        assert!(
            page("protocol_analyser.md").contains(&phrase),
            "docs/protocol_analyser.md says: {phrase}"
        );
    }
}

/// `n` with its digits in groups of three split by spaces, as the pages write a count.
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(c);
    }
    out
}
