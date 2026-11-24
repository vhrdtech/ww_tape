//! A protocol analyser on a 10 Gbps Ethernet link: every frame of both directions as an event with its bytes, and
//! the layers a decoder finds in them as tracks of their own.
//!
//! The analyser sits on a tap of a full-duplex link and sees two directions, port 0 from the clients and port 1 from
//! the servers. A port spreads its flows over [Link::queues] receive queues, as a NIC does, and each queue ([Rx]) is
//! three events tracks on the analyser's ns clock:
//!
//! | Track | Item | Bytes of an item |
//! |---|---|---|
//! | `frames` | a frame as received, from its destination address to its last payload byte | 60 to 1514 |
//! | `ipv4` | the frame's IPv4 header: source, destination, protocol, TTL, length ([Ipv4]) | 12 |
//! | `l4` | the frame's TCP or UDP header: protocol, flags, ports, payload length ([L4]) | 8 |
//!
//! An item of `ipv4` and `l4` keeps the item number of its frame in the queue's `frames` in the relation `frame`, so
//! a reader goes from a decoded header to the bytes it came from. A frame without IPv4, such as ARP, has no item in either.
//!
//! The traffic is synthetic and a pure function of the frame's number ([Traffic]): the simple IMIX of 7 frames of
//! 60 bytes, 4 of 590 and 1 of 1514, over [FLOWS] flows, three in four of them TCP. The payloads are random bytes,
//! as encrypted traffic is ([PayloadMix::Random]), so they don't compress, and the `frames` track is written raw
//! ([Link::payload_codec]). [PayloadMix::Plain] makes them the text of a plaintext protocol instead, to measure what
//! compression saves where it can. A port sends its frames back to back at the link's load. One frame
//! in [Link::reset_every] is a TCP reset ([L4::is_reset]) on a TCP flow: the rare event [ram]'s analyser looks for.
//!
//! [Link::record] records both ports through one [TapeMux] or a mux per port, each into a chain of its own. Per
//! receive queue a capture thread pushes the frames and hands each batch to a decoder thread, which pushes the
//! headers it decodes. One track has one producer, so the queues are what spreads a port's line rate over cores.
//!
//! ```
//! let link = example_ethernet::Link::full().micros(200);
//! let tapes = ww_tape::MemPieces::new();
//! let mux = ww_tape_mux::TapeMux::create(
//!     tapes.clone(),
//!     ww_tape::OwnerId([0xe7; 32]),
//!     0,
//!     &[],
//!     ww_tape::ChainOptions::default(),
//!     ww_tape_mux::MuxOptions::default(),
//! )?;
//! let captured = link.record(&[&mux])?;
//! let pieces = example_capture::pieces(&tapes, &mux.close()?)?;
//! let read = example_ethernet::read(&pieces, link.queues)?;
//! assert_eq!(read.ports[0].frames, captured.ports[0].frames);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    reason = "the case computes times, counts and offsets of a capture it makes itself from its settings, far from any \
              limit, and reads no bytes of others"
)]

pub mod ram;

use std::sync::mpsc;
use std::thread::ScopedJoinHandle;

use shrink_wrap::prelude::*;
use ww_tape::OwnerId;
use ww_tape::payloads::{PayloadCodec, Payloads};
use ww_tape::times::{EventTimes, RelationColumn, StoredRelations};
use ww_tape::{
    BlockKind, ClockNum, ClockOrigin, ClockSpec, Ends, EventsDef, Items, Magnitude, MemPieces,
    PieceStore, RelationDef, RelationIndex, RelationTarget, RelationValues, Segment, TapeReader,
    TrackId, TrackKey, TrackKind, TrackNum, TrackRepeat, TrackSpec, clock_key, track_key,
};
use ww_tape_mux::{Event, EventsOptions, MuxError, MuxOptions, TapeMux};

use example_capture::{chain_options, hash, pieces};

/// The root of the analyser's API: its track and clock keys start from it.
const API_ROOT: u64 = 0xe7_0001;

/// The owner of the analyser's chain.
const OWNER: OwnerId = OwnerId([0xe7; 32]);
/// The line rate of each direction, bits a second.
pub const LINE_BPS: u64 = 10_000_000_000;
/// Bytes a frame takes on the wire beyond the bytes the analyser keeps: preamble and delimiter 8, the frame check
/// sequence 4, the gap after it 12.
pub const WIRE_EXTRA_BYTES: u64 = 24;
/// Picoseconds a byte takes on the wire at [LINE_BPS].
const PS_PER_BYTE: u64 = 8 * 1_000_000_000_000 / LINE_BPS;
/// The simple IMIX: of 12 frames, how many have each length, without the frame check sequence.
const IMIX: [(u64, usize); 3] = [(7, 60), (4, 590), (1, 1514)];
/// The flows of the link: each a client and a server, TCP or UDP.
pub const FLOWS: u64 = 1024;
/// One frame in this many is an ARP request, no IPv4.
pub const ARP_EVERY: u64 = 1000;
/// One frame of a port in this many is a TCP reset by default, where its flow is TCP.
pub const RESET_EVERY: u64 = 4_000_000;
/// TCP's flags of a reset, RST and ACK; [L4::is_reset] tests the RST bit.
const FLAGS_RESET: u8 = 0x14;
const FLAG_RST: u8 = 0x04;
/// Bytes of random payload each port draws its frames' payloads from: larger than the LZ4 window by far, so no two
/// payloads in a chunk repeat each other.
const PAYLOAD_POOL_BYTES: usize = 16 << 20;
/// The longest frame the traffic holds.
pub const FRAME_BYTES_MAX: usize = 1514;
/// Frames a capture thread pushes in one call and hands its decoder in one batch.
pub const BATCH_FRAMES: usize = 512;
/// Batches a capture thread hands its decoder and the decoder hasn't taken, at most.
pub const DECODER_QUEUE_MAX: usize = 8;
/// Receive queues of a port at most.
pub const QUEUES_MAX: u32 = 16;
/// Bytes of an item of `ipv4` and of `l4`.
pub const IPV4_BYTES: usize = 12;
pub const L4_BYTES: usize = 8;

const ETHERTYPE_IPV4: u16 = 0x0800;
const ETHERTYPE_ARP: u16 = 0x0806;
const PROTO_TCP: u8 = 6;
const PROTO_UDP: u8 = 17;
/// The seeds of the frames' choices, the flows and the payload pools.
const FRAME_SEED: u64 = 0xe7_f0;
const FLOW_SEED: u64 = 0xe7_f1;
const POOL_SEED: u64 = 0xe7_f2;
const RESET_SEED: u64 = 0xe7_f3;

// --8<-- [start:link]
/// A capture of the link: how loaded it is, how long, and how its frames are cut into chunks.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Link {
    /// The load of each direction, percent of the line rate, 1 to 100.
    pub load_percent: u64,
    /// Length of the capture, µs.
    pub micros: u64,
    /// Frame bytes of a chunk of `frames` at most.
    pub chunk_bytes: u32,
    /// The receive queues of each port, 1 to [QUEUES_MAX]: the flows are spread over them, and each is a thread
    /// and tracks of its own.
    pub queues: u32,
    /// One frame of a port in this many is a TCP reset, where its flow is TCP; 0 for none.
    pub reset_every: u64,
    /// What the frames' payloads are made of.
    pub payload: PayloadMix,
    /// How each chunk of `frames` codes its payloads column: [PayloadCodec::Raw] by default, as the random payloads
    /// do not compress and LZ4 would only cost its time.
    pub payload_codec: PayloadCodec,
}
// --8<-- [end:link]

/// What the bytes after a frame's headers are.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum PayloadMix {
    /// Random bytes, as encrypted traffic is: they don't compress.
    #[default]
    Random,
    /// The text of plaintext protocols: HTTP requests and responses, JSON and log lines, drawn from a small
    /// vocabulary with counters, so headers repeat as they do on a real link. Synthetic.
    Plain,
}

impl PayloadMix {
    /// Its name, as the bench prints it.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Random => "random",
            Self::Plain => "plain",
        }
    }
}

impl Link {
    /// A second of the link at its line rate in both directions, two receive queues a port, chunks of `frames` of
    /// 1 MiB.
    pub const fn full() -> Self {
        Self {
            load_percent: 100,
            micros: 1_000_000,
            chunk_bytes: 1 << 20,
            queues: 2,
            reset_every: RESET_EVERY,
            payload: PayloadMix::Random,
            payload_codec: PayloadCodec::Raw,
        }
    }

    /// The same capture with payloads of `payload`.
    pub const fn payload(self, payload: PayloadMix) -> Self {
        Self { payload, ..self }
    }

    /// The same capture with the `frames` payloads coded by `payload_codec`.
    pub const fn payload_codec(self, payload_codec: PayloadCodec) -> Self {
        Self {
            payload_codec,
            ..self
        }
    }

    /// The same capture cut to `micros`.
    pub const fn micros(self, micros: u64) -> Self {
        Self { micros, ..self }
    }

    /// The same capture at `load_percent` of the line rate.
    pub const fn load(self, load_percent: u64) -> Self {
        Self {
            load_percent,
            ..self
        }
    }

    /// The same capture over `queues` receive queues a port.
    pub const fn queues(self, queues: u32) -> Self {
        Self { queues, ..self }
    }

    /// The same capture with one frame in `reset_every` a TCP reset, 0 for none.
    pub const fn resets(self, reset_every: u64) -> Self {
        Self {
            reset_every,
            ..self
        }
    }

    /// The receive queues of both ports.
    pub fn rxs(&self) -> impl Iterator<Item = Rx> + use<> {
        let queues = self.queues;
        [0, 1]
            .into_iter()
            .flat_map(move |port| (0..queues).map(move |queue| Rx { port, queue }))
    }

    /// Record both ports, port `p` into the chain of `muxes[p % muxes.len()]`, as the [module docs](self) say: what
    /// each port sent.
    ///
    /// # Errors
    ///
    /// A setting the capture can't be made with, or a mux or handle call that failed.
    pub fn record<T>(&self, muxes: &[&TapeMux<T>]) -> Result<Captured, EthError<T::Error>>
    where
        T: PieceStore + Send + 'static,
        T::Sink: Send,
        T::Key: Send,
        T::Error: Send + Sync,
    {
        if !(1..=100).contains(&self.load_percent) {
            return Err(EthError::Settings("the load is 1 to 100 percent"));
        }
        if !(1..=QUEUES_MAX).contains(&self.queues) {
            return Err(EthError::Settings("1 to QUEUES_MAX queues a port"));
        }
        let rxs = define(self, muxes)?;
        let mut ports = [Sent::default(), Sent::default()];
        std::thread::scope(|s| -> Result<_, EthError<T::Error>> {
            let threads: Vec<_> = rxs
                .iter()
                .map(|tracks| {
                    let mux = muxes
                        .get(port_index(tracks.rx.port) % muxes.len().max(1))
                        .copied()
                        .ok_or(EthError::Settings("a mux"))?;
                    let (to_decoder, from_capture) = mpsc::sync_channel(DECODER_QUEUE_MAX);
                    // the batches the decoder is done with go back to be filled again
                    let (to_capture, from_decoder) = mpsc::sync_channel(DECODER_QUEUE_MAX);
                    let queues = (to_decoder, from_decoder);
                    let capture = s.spawn(move || self.capture(mux, tracks, &queues));
                    let decoder =
                        s.spawn(move || decode_rx(mux, tracks, &from_capture, &to_capture));
                    Ok((tracks.rx, capture, decoder))
                })
                .collect::<Result<_, EthError<T::Error>>>()?;
            for (rx, capture, decoder) in threads {
                let mut sent = joined(capture)?;
                (sent.ipv4, sent.l4) = joined(decoder)?;
                ports
                    .get_mut(port_index(rx.port))
                    .ok_or(EthError::Settings("port 0 or 1"))?
                    .merge(&sent);
            }
            Ok(())
        })?;
        Ok(Captured { ports })
    }

    /// The capture thread of a receive queue: push its frames into its `frames` a batch at a time, and hand each
    /// batch to the decoder; fill the batches the decoder hands back again.
    fn capture<T>(
        &self,
        mux: &TapeMux<T>,
        tracks: &RxTracks,
        (to_decoder, from_decoder): &(mpsc::SyncSender<Batch>, mpsc::Receiver<Batch>),
    ) -> Result<Sent, EthError<T::Error>>
    where
        T: PieceStore + Send + 'static,
        T::Sink: Send,
        T::Key: Send,
        T::Error: Send + Sync,
    {
        // --8<-- [start:capture]
        let mut frames = mux.events(
            tracks.frames,
            EventsOptions {
                chunk_bytes: exact_units::Bytes::new(self.chunk_bytes),
                latency: None, // a capture at line rate fills its chunks
                payload_codec: self.payload_codec,
                ..EventsOptions::default()
            },
        )?;
        let mut traffic = Traffic::new(*self, tracks.rx);
        let mut sent = Sent::default();
        loop {
            let mut batch = from_decoder.try_recv().unwrap_or_default();
            batch.restart(frames.next_item().get());
            while batch.len() < BATCH_FRAMES && traffic.next_into(&mut batch) {}
            if batch.is_empty() {
                break;
            }
            let events: Vec<Event<'_>> = batch
                .frames()
                .map(|(tick, bytes)| Event {
                    start: tick,
                    end: tick,
                    payload: bytes,
                    ..Event::default()
                })
                .collect();
            frames.push_all(&events)?;
            sent.add(&batch);
            if to_decoder.send(batch).is_err() {
                return Err(EthError::Thread("a decoder"));
            }
        }
        frames.finish()?;
        // --8<-- [end:capture]
        Ok(sent)
    }
}

/// Port `port`'s index in [Captured::ports].
fn port_index(port: u32) -> usize {
    usize::try_from(port).unwrap_or(usize::MAX)
}

/// The result of a thread of the capture, a panic as an error.
fn joined<R, E>(h: ScopedJoinHandle<'_, Result<R, EthError<E>>>) -> Result<R, EthError<E>> {
    h.join()
        .map_err(|_e| EthError::Thread("a thread panicked"))?
}

/// The decoder thread of a receive queue: decode each batch of frames and push the headers into its `ipv4` and
/// `l4`, each with the number of its frame. What it pushed into each.
fn decode_rx<T>(
    mux: &TapeMux<T>,
    tracks: &RxTracks,
    from_capture: &mpsc::Receiver<Batch>,
    to_capture: &mpsc::SyncSender<Batch>,
) -> Result<(u64, u64), EthError<T::Error>>
where
    T: PieceStore + Send + 'static,
    T::Sink: Send,
    T::Key: Send,
    T::Error: Send + Sync,
{
    let options = EventsOptions {
        latency: None,
        ..EventsOptions::default()
    };
    let (mut ipv4, mut l4) = (
        mux.events(tracks.ipv4, options)?,
        mux.events(tracks.l4, options)?,
    );
    let (mut ip_items, mut l4_items) = (Decoded::default(), Decoded::default());
    let (mut n_ip, mut n_l4) = (0u64, 0u64);
    for batch in from_capture {
        ip_items.clear();
        l4_items.clear();
        for (item, (tick, bytes)) in (batch.first_item..).zip(batch.frames()) {
            let Some((ip, l)) = decode_frame(bytes) else {
                continue;
            };
            ip_items.push(tick, item, &ip.to_bytes());
            if let Some(l) = l {
                l4_items.push(tick, item, &l.to_bytes());
            }
        }
        // --8<-- [start:decoded]
        // each header keeps the number of its frame in the relation `frame`
        ipv4.push_all(&ip_items.events(IPV4_BYTES))?;
        l4.push_all(&l4_items.events(L4_BYTES))?;
        // --8<-- [end:decoded]
        n_ip += ip_items.len() as u64;
        n_l4 += l4_items.len() as u64;
        // a batch the capture thread has no room for, or no use for once done, is dropped
        to_capture.try_send(batch).ok();
    }
    ipv4.finish()?;
    l4.finish()?;
    Ok((n_ip, n_l4))
}

/// Headers of one batch decoded, of one fixed length each, with their ticks and their frames' numbers.
#[derive(Default)]
struct Decoded {
    ticks: Vec<i64>,
    frames: Vec<u64>,
    bytes: Vec<u8>,
}

impl Decoded {
    fn clear(&mut self) {
        self.ticks.clear();
        self.frames.clear();
        self.bytes.clear();
    }

    fn len(&self) -> usize {
        self.ticks.len()
    }

    fn push(&mut self, tick: i64, frame: u64, bytes: &[u8]) {
        self.ticks.push(tick);
        self.frames.push(frame);
        self.bytes.extend_from_slice(bytes);
    }

    /// The headers as events of `len` bytes each.
    fn events(&self, len: usize) -> Vec<Event<'_>> {
        self.ticks
            .iter()
            .zip(self.frames.chunks(1))
            .zip(self.bytes.chunks(len))
            .map(|((&tick, frame), payload)| Event {
                start: tick,
                end: tick,
                flags: 0,
                payload,
                values: frame,
            })
            .collect()
    }
}

/// Frames of one port in a row: their ticks, lengths and bytes, the first one's item number.
#[derive(Clone, Debug, Default)]
pub struct Batch {
    first_item: u64,
    ticks: Vec<i64>,
    lens: Vec<usize>,
    bytes: Vec<u8>,
}

impl Batch {
    /// Empty the batch for frames from item `first_item` on, keeping its room.
    fn restart(&mut self, first_item: u64) {
        self.first_item = first_item;
        self.ticks.clear();
        self.lens.clear();
        self.bytes.clear();
    }

    /// Frames in the batch.
    pub fn len(&self) -> usize {
        self.ticks.len()
    }

    /// Whether it holds no frame.
    pub fn is_empty(&self) -> bool {
        self.ticks.is_empty()
    }

    /// Each frame's tick and bytes.
    pub fn frames(&self) -> impl Iterator<Item = (i64, &[u8])> + '_ {
        let mut rest = self.bytes.as_slice();
        self.ticks.iter().zip(&self.lens).map(move |(&tick, &len)| {
            let (frame, after) = rest.split_at_checked(len).unwrap_or((rest, &[]));
            rest = after;
            (tick, frame)
        })
    }
}

/// The frames of one receive queue: the port's frames of the flows the queue takes.
///
/// A port's frame `n` is a pure function of `n`, and the port sends its frames back to back at the link's load. The
/// queue of a frame is its flow's number modulo the queues, as a NIC spreads flows by a hash; every queue walks all
/// of its port's frames for their times and makes the bytes of its own only.
#[derive(Clone, Debug)]
pub struct Traffic {
    link: Link,
    rx: Rx,
    pool: Vec<u8>,
    /// The port's next frame's number and the ps its first bit starts at.
    next: u64,
    at_ps: u64,
    end_ps: u64,
}

/// Lines of the plaintext protocols' text, the vocabulary of [PayloadMix::Plain].
const PLAIN_LINES: [&str; 12] = [
    "GET /api/v1/items/{} HTTP/1.1\r\nHost: shop.example.test\r\nUser-Agent: curl/8.5.0\r\nAccept: */*\r\n\r\n",
    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nCache-Control: no-store\r\nServer: nginx\r\n\r\n",
    "{\"id\":{},\"state\":\"ok\",\"items\":[{\"sku\":\"A-{}\",\"qty\":1},{\"sku\":\"B-{}\",\"qty\":2}]}\n",
    "POST /api/v1/orders HTTP/1.1\r\nHost: shop.example.test\r\nContent-Type: application/json\r\n\r\n",
    "<14>1 2026-11-23T09:{}:00Z gw-1 sshd 812 - - Accepted publickey for deploy from 10.0.0.{}\n",
    "{\"level\":\"info\",\"msg\":\"request done\",\"status\":200,\"ms\":{},\"path\":\"/api/v1/items\"}\n",
    "SELECT id, name, price FROM items WHERE id = {} AND active = 1;\r\n",
    "HTTP/1.1 304 Not Modified\r\nETag: \"{}\"\r\nDate: Mon, 23 Nov 2026 09:00:00 GMT\r\n\r\n",
    "220 mail.example.test ESMTP ready\r\nEHLO client-{}.example.test\r\n250 OK\r\n",
    "<html><head><title>Items</title></head><body><ul><li>item {}</li><li>item {}</li></ul></body></html>\n",
    "{\"jsonrpc\":\"2.0\",\"id\":{},\"method\":\"get_status\",\"params\":[]}\n",
    "Cookie: session=0123456789abcdef; theme=dark; lang=en; id={}\r\n",
];

/// A pool of [PAYLOAD_POOL_BYTES] bytes of plaintext protocol text: the lines of [PLAIN_LINES] in the order `seed`
/// draws them, each `{}` a number it draws. Frames take their payloads from random offsets, so a payload starts in
/// the middle of a line, as a segment does.
fn plain_pool(seed: u64) -> Vec<u8> {
    let mut pool = Vec::with_capacity(PAYLOAD_POOL_BYTES + 256);
    let mut n = 0u64;
    while pool.len() < PAYLOAD_POOL_BYTES {
        let line = PLAIN_LINES
            .get(usize::try_from(hash(seed, n) % PLAIN_LINES.len() as u64).unwrap_or(0))
            .copied()
            .unwrap_or_default();
        for (i, part) in line.split("{}").enumerate() {
            if i > 0 {
                pool.extend_from_slice(
                    (hash(seed ^ 1, n * 8 + i as u64) % 1000)
                        .to_string()
                        .as_bytes(),
                );
            }
            pool.extend_from_slice(part.as_bytes());
        }
        n += 1;
    }
    pool.truncate(PAYLOAD_POOL_BYTES);
    pool
}

/// What a port's frame is, before its bytes: its choices, its flow and its length.
struct Shape {
    h: u64,
    flow: u64,
    arp: bool,
    /// A TCP reset, where the flow is TCP: the shortest frame.
    reset: bool,
    len: usize,
}

impl Traffic {
    /// The traffic of receive queue `rx` of `link`.
    pub fn new(link: Link, rx: Rx) -> Self {
        let seed = POOL_SEED ^ u64::from(rx.port);
        let pool = match link.payload {
            PayloadMix::Random => (0..PAYLOAD_POOL_BYTES / 8)
                .flat_map(|i| hash(seed, i as u64).to_le_bytes())
                .collect(),
            PayloadMix::Plain => plain_pool(seed),
        };
        Self {
            link,
            rx,
            pool,
            next: 0,
            at_ps: 0,
            end_ps: link.micros.saturating_mul(1_000_000),
        }
    }

    /// Add the queue's next frame to `batch`: false past the capture's end.
    pub fn next_into(&mut self, batch: &mut Batch) -> bool {
        while self.at_ps < self.end_ps {
            let n = self.next;
            let shape = self.shape(n);
            let at_ps = self.at_ps;
            let wire_ps = (shape.len as u64 + WIRE_EXTRA_BYTES) * PS_PER_BYTE;
            self.at_ps += wire_ps * 100 / self.link.load_percent.clamp(1, 100);
            self.next += 1;
            if shape.flow % u64::from(self.link.queues.max(1)) != u64::from(self.rx.queue) {
                continue;
            }
            let from = batch.bytes.len();
            self.frame(n, &shape, &mut batch.bytes);
            batch.lens.push(batch.bytes.len() - from);
            batch
                .ticks
                .push(i64::try_from(at_ps / 1_000).unwrap_or(i64::MAX));
            return true;
        }
        false
    }

    /// Every frame of the queue: its tick and bytes.
    pub fn all(mut self) -> Vec<(i64, Vec<u8>)> {
        let mut out = Vec::new();
        let mut batch = Batch::default();
        while self.next_into(&mut batch) {
            if batch.len() == BATCH_FRAMES {
                out.extend(batch.frames().map(|(t, b)| (t, b.to_vec())));
                batch.restart(0);
            }
        }
        out.extend(batch.frames().map(|(t, b)| (t, b.to_vec())));
        out
    }

    /// The port's frame `n` before its bytes.
    fn shape(&self, n: u64) -> Shape {
        let h = hash(FRAME_SEED ^ u64::from(self.rx.port), n);
        let arp = (h >> 8).is_multiple_of(ARP_EVERY);
        let pick = h % 12;
        let mut seen = 0;
        let len = IMIX
            .iter()
            .find(|(n, _)| {
                seen += n;
                pick < seen
            })
            .map_or(60, |(_, len)| *len);
        let reset = !arp
            && self.link.reset_every > 0
            && hash(RESET_SEED ^ u64::from(self.rx.port), n).is_multiple_of(self.link.reset_every);
        Shape {
            h,
            flow: (h >> 32) % FLOWS,
            arp,
            reset,
            len: if arp || reset { 60 } else { len },
        }
    }

    /// Append the bytes of the port's frame `n` of shape `shape` to `out`.
    fn frame(&self, n: u64, shape: &Shape, out: &mut Vec<u8>) {
        let Shape { h, len, .. } = *shape;
        let port = self.rx.port;
        let flow = Flow::new(shape.flow);
        // the server's side mirrors the client's
        let (src, dst) = if port == 0 {
            (&flow.client, &flow.server)
        } else {
            (&flow.server, &flow.client)
        };
        out.extend_from_slice(&dst.mac);
        out.extend_from_slice(&src.mac);
        if shape.arp {
            out.extend_from_slice(&ETHERTYPE_ARP.to_be_bytes());
            // an ARP request for the destination, padded to the shortest frame
            out.extend_from_slice(&[0, 1, 8, 0, 6, 4, 0, 1]);
            out.extend_from_slice(&src.mac);
            out.extend_from_slice(&src.ip);
            out.extend_from_slice(&[0; 6]);
            out.extend_from_slice(&dst.ip);
            out.extend_from_slice(&[0; 18]);
            return;
        }
        out.extend_from_slice(&ETHERTYPE_IPV4.to_be_bytes());
        let ip_len = u16::try_from(len - 14).unwrap_or(u16::MAX);
        let ip_at = out.len();
        out.extend_from_slice(&[0x45, 0]);
        out.extend_from_slice(&ip_len.to_be_bytes());
        out.extend_from_slice(&n.to_be_bytes()[6..]);
        out.extend_from_slice(&[0x40, 0, if port == 0 { 64 } else { 57 }, flow.proto]);
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&src.ip);
        out.extend_from_slice(&dst.ip);
        let sum = ip_checksum(out.get(ip_at..).unwrap_or_default());
        if let Some(c) = out.get_mut(ip_at + 10..ip_at + 12) {
            c.copy_from_slice(&sum.to_be_bytes());
        }
        out.extend_from_slice(&src.port.to_be_bytes());
        out.extend_from_slice(&dst.port.to_be_bytes());
        if flow.proto == PROTO_TCP {
            // sequence and acknowledgment, a header of 5 words, ACK with PSH on a frame with data, window, no
            // checksum (left to the NIC's offload)
            out.extend_from_slice(&h.to_be_bytes());
            let flags = match (shape.reset, len > 60) {
                (true, _) => FLAGS_RESET,
                (false, true) => 0x18,
                (false, false) => 0x10,
            };
            out.extend_from_slice(&[0x50, flags, 0x01, 0xf6, 0, 0, 0, 0]);
        } else {
            let udp_len = u16::try_from(len - 34).unwrap_or(u16::MAX);
            out.extend_from_slice(&udp_len.to_be_bytes());
            out.extend_from_slice(&[0, 0]);
        }
        let header = out.len() - ip_at + 14;
        let payload = len.saturating_sub(header);
        let off =
            usize::try_from(hash(FRAME_SEED, n) % (PAYLOAD_POOL_BYTES - FRAME_BYTES_MAX) as u64)
                .unwrap_or(0);
        out.extend_from_slice(self.pool.get(off..off + payload).unwrap_or_default());
    }
}

/// One side of a flow: its MAC and IPv4 addresses and its port.
#[derive(Clone, Debug)]
struct Side {
    mac: [u8; 6],
    ip: [u8; 4],
    port: u16,
}

/// A flow of the link: a client, a server and the protocol between them.
#[derive(Clone, Debug)]
struct Flow {
    client: Side,
    server: Side,
    proto: u8,
}

impl Flow {
    fn new(f: u64) -> Self {
        let h = hash(FLOW_SEED, f).to_le_bytes();
        let [a, b, c, d, e, g, i, j] = h;
        let proto = if a % 4 == 0 { PROTO_UDP } else { PROTO_TCP };
        Self {
            client: Side {
                mac: [0x02, 0, 0, b, c, d],
                ip: [10, b, c, d],
                port: u16::from_le_bytes([e, g]) | 0x8000,
            },
            server: Side {
                mac: [0x02, 0, 1, 0, i % 16, 1],
                ip: [192, 0, 2, i],
                port: if proto == PROTO_UDP {
                    4789 + u16::from(j % 4)
                } else {
                    if j % 4 == 3 { 80 } else { 443 }
                },
            },
            proto,
        }
    }
}

/// The one's-complement sum of an IPv4 header's 16-bit words, its checksum field 0.
fn ip_checksum(header: &[u8]) -> u16 {
    let mut sum: u32 = header
        .chunks(2)
        .map(|w| {
            u32::from(u16::from_be_bytes([
                w.first().copied().unwrap_or(0),
                w.get(1).copied().unwrap_or(0),
            ]))
        })
        .sum();
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !u16::try_from(sum).unwrap_or(0)
}

/// An item of `ipv4`: what the decoder keeps of a frame's IPv4 header.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Ipv4 {
    pub src: [u8; 4],
    pub dst: [u8; 4],
    pub proto: u8,
    pub ttl: u8,
    /// The packet's length, its header included.
    pub len: u16,
}

impl Ipv4 {
    /// Its bytes in `ipv4`: source, destination, protocol, TTL, length big-endian.
    pub fn to_bytes(&self) -> [u8; IPV4_BYTES] {
        let mut b = [0; IPV4_BYTES];
        let [s0, s1, s2, s3] = self.src;
        let [d0, d1, d2, d3] = self.dst;
        let [l0, l1] = self.len.to_be_bytes();
        b.copy_from_slice(&[s0, s1, s2, s3, d0, d1, d2, d3, self.proto, self.ttl, l0, l1]);
        b
    }

    /// The item from its bytes in `ipv4`.
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        let [s0, s1, s2, s3, d0, d1, d2, d3, proto, ttl, l0, l1] =
            *b.first_chunk::<IPV4_BYTES>()?;
        Some(Self {
            src: [s0, s1, s2, s3],
            dst: [d0, d1, d2, d3],
            proto,
            ttl,
            len: u16::from_be_bytes([l0, l1]),
        })
    }
}

/// An item of `l4`: what the decoder keeps of a frame's TCP or UDP header.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct L4 {
    pub proto: u8,
    /// TCP's flags; 0 for UDP.
    pub flags: u8,
    pub src_port: u16,
    pub dst_port: u16,
    /// Bytes of the segment's or datagram's payload.
    pub payload: u16,
}

impl L4 {
    /// Its bytes in `l4`: protocol, flags, ports and payload length big-endian.
    pub fn to_bytes(&self) -> [u8; L4_BYTES] {
        let [s0, s1] = self.src_port.to_be_bytes();
        let [d0, d1] = self.dst_port.to_be_bytes();
        let [p0, p1] = self.payload.to_be_bytes();
        [self.proto, self.flags, s0, s1, d0, d1, p0, p1]
    }

    /// Whether the header is a TCP reset: its RST flag set.
    pub fn is_reset(&self) -> bool {
        self.proto == PROTO_TCP && self.flags & FLAG_RST != 0
    }

    /// The item from its bytes in `l4`.
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        let [proto, flags, s0, s1, d0, d1, p0, p1] = *b.first_chunk::<L4_BYTES>()?;
        Some(Self {
            proto,
            flags,
            src_port: u16::from_be_bytes([s0, s1]),
            dst_port: u16::from_be_bytes([d0, d1]),
            payload: u16::from_be_bytes([p0, p1]),
        })
    }
}

/// The IPv4 header of a frame and its TCP or UDP header; None for a frame without IPv4, and no transport header for
/// another protocol or a header cut short.
pub fn decode_frame(frame: &[u8]) -> Option<(Ipv4, Option<L4>)> {
    let be = |at: usize| Some(u16::from_be_bytes(*frame.get(at..)?.first_chunk::<2>()?));
    if be(12)? != ETHERTYPE_IPV4 {
        return None;
    }
    let ip = frame.get(14..)?;
    let ihl = usize::from(ip.first()? & 0x0f) * 4;
    let four = |at: usize| ip.get(at..).and_then(|b| b.first_chunk::<4>()).copied();
    let header = Ipv4 {
        src: four(12)?,
        dst: four(16)?,
        proto: *ip.get(9)?,
        ttl: *ip.get(8)?,
        len: be(16)?,
    };
    let at = 14 + ihl;
    let l4 = match header.proto {
        PROTO_TCP => {
            let offset = usize::from(frame.get(at + 12)? >> 4) * 4;
            Some(L4 {
                proto: PROTO_TCP,
                flags: *frame.get(at + 13)?,
                src_port: be(at)?,
                dst_port: be(at + 2)?,
                payload: header.len.checked_sub(u16::try_from(ihl + offset).ok()?)?,
            })
        }
        PROTO_UDP => Some(L4 {
            proto: PROTO_UDP,
            flags: 0,
            src_port: be(at)?,
            dst_port: be(at + 2)?,
            payload: be(at + 4)?.checked_sub(8)?,
        }),
        _ => None,
    };
    Some((header, l4))
}

/// The analyser's clock: ns since the capture starts, stamped as each frame's first bit arrives.
pub fn clock() -> ClockSpec<'static> {
    ClockSpec {
        key: clock_key(API_ROOT, &[Segment::Name("capture_clock")], 1),
        rate: Magnitude::pow10(9).into(),
        resolution: Magnitude::pow10(-9).into(),
        origin: ClockOrigin::Boot,
        label: "analyser ns",
    }
}

/// A receive queue of a port.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Rx {
    /// 0 from the clients, 1 from the servers.
    pub port: u32,
    pub queue: u32,
}

/// The key of track `name` (`frames`, `ipv4` or `l4`) of receive queue `rx`.
pub fn key(rx: Rx, name: &str) -> TrackKey {
    track_key(
        API_ROOT,
        &[
            Segment::Name("port"),
            Segment::Index(rx.port),
            Segment::Name("queue"),
            Segment::Index(rx.queue),
            Segment::Name(name),
        ],
    )
}

/// The tracks of a receive queue, and the numbers its mux gave them.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct RxTracks {
    pub rx: Rx,
    pub frames: TrackNum,
    pub ipv4: TrackNum,
    pub l4: TrackNum,
}

/// The events track `name` of receive queue `rx` on clock `clock`, its items related to `relations`.
fn spec<'r>(
    rx: Rx,
    name: &'static str,
    clock: ClockNum,
    relations: &'r [RelationDef<'r>],
) -> TrackSpec<'r> {
    TrackSpec {
        key: key(rx, name),
        clock,
        dimensions: RefVec::Slice { slice: &[] },
        items: Items::RawBytes,
        label: name,
        repeat: TrackRepeat::Stream,
        kind: TrackKind::Events(EventsDef {
            ends: Ends::Instants,
            flags: false,
        }),
        relations: RefVec::Slice { slice: relations },
        owner: [0; 32],
        defined_twice: false,
    }
}

// --8<-- [start:frame_relation]
/// The relation of a decoded header to its frame: the frame's item number in `frames` of the same receive queue,
/// never decreasing, its range per chunk in the index.
pub fn frame_relation(rx: Rx) -> RelationDef<'static> {
    RelationDef {
        name: "frame",
        target: Some(RelationTarget {
            run: [0; 16], // this run
            track: frames_id(rx),
        }),
        values: RelationValues::Stored,
        monotone: true,
        index: RelationIndex::Range,
    }
}
// --8<-- [end:frame_relation]

/// The id of track `frames` of receive queue `rx`.
pub fn frames_id(rx: Rx) -> TrackId {
    spec(rx, "frames", ClockNum::MIN, &[]).id()
}

/// Define the analyser's clock and the tracks of every receive queue of `link`, port `p`'s on
/// `muxes[p % muxes.len()]`.
///
/// # Errors
///
/// No mux, or a definition a mux refused.
pub fn define<T>(link: &Link, muxes: &[&TapeMux<T>]) -> Result<Vec<RxTracks>, EthError<T::Error>>
where
    T: PieceStore + Send + 'static,
    T::Sink: Send,
    T::Key: Send,
    T::Error: Send + Sync,
{
    let mut clocks = Vec::new();
    for mux in muxes {
        clocks.push(mux.define_clock(clock())?);
    }
    let mut out = Vec::new();
    for rx in link.rxs() {
        let at = port_index(rx.port) % muxes.len().max(1);
        let (mux, clock) = muxes
            .get(at)
            .zip(clocks.get(at))
            .ok_or(EthError::Settings("a mux"))?;
        let relation = [frame_relation(rx)];
        out.push(RxTracks {
            rx,
            frames: mux.define_track(spec(rx, "frames", *clock, &[]))?,
            ipv4: mux.define_track(spec(rx, "ipv4", *clock, &relation))?,
            l4: mux.define_track(spec(rx, "l4", *clock, &relation))?,
        });
    }
    Ok(out)
}

/// What one port, or one of its receive queues, sent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sent {
    /// Frames and their bytes.
    pub frames: u64,
    pub frame_bytes: u64,
    /// Bytes the frames took on the wire, their preambles, checks and gaps included.
    pub wire_bytes: u64,
    /// Items pushed into `ipv4` and `l4`.
    pub ipv4: u64,
    pub l4: u64,
    /// The last frame's tick.
    pub last_tick: i64,
}

impl Sent {
    fn add(&mut self, batch: &Batch) {
        self.frames += batch.len() as u64;
        let bytes: usize = batch.lens.iter().sum();
        self.frame_bytes += bytes as u64;
        self.wire_bytes += bytes as u64 + batch.len() as u64 * WIRE_EXTRA_BYTES;
        self.last_tick = batch.ticks.last().copied().unwrap_or(self.last_tick);
    }

    /// Add what another receive queue of the port sent.
    fn merge(&mut self, o: &Self) {
        self.frames += o.frames;
        self.frame_bytes += o.frame_bytes;
        self.wire_bytes += o.wire_bytes;
        self.ipv4 += o.ipv4;
        self.l4 += o.l4;
        self.last_tick = self.last_tick.max(o.last_tick);
    }
}

/// What [Link::record] wrote: what each port sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Captured {
    pub ports: [Sent; 2],
}

impl Link {
    /// Record the capture into one chain in memory and read it back: what each track takes.
    ///
    /// # Errors
    ///
    /// A setting the capture can't be made with, or a chain that doesn't read back as written.
    pub fn sizes(&self) -> Result<Sizes, EthError<ww_tape::MemError>> {
        let tapes = MemPieces::new();
        let mux = TapeMux::create(
            tapes.clone(),
            OWNER,
            0,
            &[],
            chain_options(),
            MuxOptions::default(),
        )?;
        let captured = self.record(&[&mux])?;
        let pieces = pieces(&tapes, &mux.close()?)?;
        let read = read(&pieces, self.queues).map_err(|e| EthError::Back(e.to_string()))?;
        Ok(Sizes {
            link: *self,
            captured,
            read,
        })
    }
}

/// What a capture's tracks take, as [Link::sizes] measured them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sizes {
    pub link: Link,
    pub captured: Captured,
    pub read: ReadBack,
}

impl Sizes {
    /// Frames of both ports, their bytes, and the bytes they took on the wire.
    pub fn frames(&self) -> (u64, u64, u64) {
        self.captured.ports.iter().fold((0, 0, 0), |(n, b, w), p| {
            (n + p.frames, b + p.frame_bytes, w + p.wire_bytes)
        })
    }

    /// Of both ports, the items and bytes of track `t`: 0 `frames`, 1 `ipv4`, 2 `l4`.
    pub fn track(&self, t: usize) -> (u64, TrackBytes) {
        let mut bytes = TrackBytes::default();
        let mut items = 0;
        for p in &self.read.ports {
            let b = p.bytes.get(t).copied().unwrap_or_default();
            bytes.chunks += b.chunks;
            bytes.chunk_bytes += b.chunk_bytes;
            bytes.summary_bytes += b.summary_bytes;
            items += [p.frames, p.ipv4, p.l4].get(t).copied().unwrap_or(0);
        }
        (items, bytes)
    }
}

impl core::fmt::Display for Sizes {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let (frames, frame_bytes, wire_bytes) = self.frames();
        writeln!(
            f,
            "{} µs of the link at {} % of 10 Gbps in both directions: {frames} frames of {frame_bytes} bytes, \
             {wire_bytes} bytes on the wire.\n",
            self.link.micros, self.link.load_percent
        )?;
        writeln!(
            f,
            "| Track | Items | Chunks | Chunk bytes | Summary bytes | Bytes per item |"
        )?;
        writeln!(f, "|---|---:|---:|---:|---:|---:|")?;
        for (t, name) in ["frames", "ipv4", "l4"].into_iter().enumerate() {
            let (items, b) = self.track(t);
            #[expect(
                clippy::cast_precision_loss,
                reason = "a capture's bytes and items are far below 2^52: exact in an f64"
            )]
            let per = (b.chunk_bytes + b.summary_bytes) as f64 / items.max(1) as f64;
            writeln!(
                f,
                "| `{name}` | {items} | {} | {} | {} | {per:.1} |",
                b.chunks, b.chunk_bytes, b.summary_bytes
            )?;
        }
        write!(
            f,
            "\nThe chain: {} bytes, {} per frame byte.",
            self.read.piece_bytes,
            ratio(self.read.piece_bytes, frame_bytes)
        )
    }
}

/// `a / b` to three decimals.
fn ratio(a: u64, b: u64) -> String {
    let thousandths = (a * 1000 + b.max(1) / 2) / b.max(1);
    format!("{}.{:03}", thousandths / 1000, thousandths % 1000)
}

/// The bytes a track takes in a chain: its chunks and its summary blocks.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct TrackBytes {
    pub chunks: u64,
    pub chunk_bytes: u64,
    pub summary_bytes: u64,
}

/// What a chain of the analyser holds for one port.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PortRead {
    /// Frames, and their bytes.
    pub frames: u64,
    pub frame_bytes: u64,
    /// Items of `ipv4` and `l4`.
    pub ipv4: u64,
    pub l4: u64,
    /// Items of `ipv4` and `l4` whose relation `frame` names a frame whose bytes decode to that very item.
    pub matched: u64,
    /// Per receive queue, a hash of every frame's tick and bytes, in order ([digest]).
    pub digests: Vec<u64>,
    /// The bytes each track takes: `frames`, `ipv4`, `l4`.
    pub bytes: [TrackBytes; 3],
}

/// What [read] read: each port's tracks, and the pieces' bytes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReadBack {
    pub ports: [PortRead; 2],
    pub piece_bytes: u64,
}

/// A hash of frames' ticks and bytes in order: equal for the same frames.
pub fn digest<'f>(frames: impl IntoIterator<Item = (i64, &'f [u8])>) -> u64 {
    frames.into_iter().fold(0, |d, (tick, bytes)| {
        let d = hash(d, tick.cast_unsigned());
        bytes
            .chunks(8)
            .fold(d, |d, w| hash(d, u64::from_le_bytes(pad8(w))))
    })
}

fn pad8(w: &[u8]) -> [u8; 8] {
    let mut b = [0; 8];
    if let Some(d) = b.get_mut(..w.len()) {
        d.copy_from_slice(w);
    }
    b
}

/// Read the pieces of the analyser's chains back, its ports' `queues` receive queues: every chunk of every track
/// checked and decoded, each decoded header against the frame its relation names.
///
/// # Errors
///
/// A piece that doesn't read, or a track that doesn't hold what the capture writes.
pub fn read(
    pieces: &[Vec<u8>],
    queues: u32,
) -> Result<ReadBack, EthError<ww_tape::reader::OutOfRange>> {
    let mut out = ReadBack::default();
    let mut buf = Vec::new();
    for rx in Link::full().queues(queues).rxs() {
        let p = out
            .ports
            .get_mut(port_index(rx.port))
            .ok_or(EthError::Settings("port 0 or 1"))?;
        // every frame of the queue, kept to check the headers against
        let mut frames: Vec<Vec<u8>> = Vec::new();
        let mut ticks = Vec::new();
        for (t, name) in ["frames", "ipv4", "l4"].into_iter().enumerate() {
            let relation = [frame_relation(rx)];
            let def = spec(
                rx,
                name,
                ClockNum::MIN,
                if t == 0 { &[] } else { &relation },
            )
            .numbered(TrackNum::MIN);
            for piece in pieces {
                let reader = TapeReader::open(piece.as_slice())?;
                for entry in reader.track_entries(0, def.id())? {
                    let bytes = p
                        .bytes
                        .get_mut(t)
                        .ok_or(EthError::Settings("three tracks"))?;
                    if entry.kind == BlockKind::Summary {
                        bytes.summary_bytes += u64::from(entry.len);
                        continue;
                    }
                    bytes.chunks += 1;
                    bytes.chunk_bytes += u64::from(entry.len);
                    let block = reader.read_block(0, &entry, &mut buf)?;
                    let chunk = block.chunk().ok_or(EthError::Read("a chunk"))?;
                    let times = EventTimes::read(&chunk)?;
                    let payloads = Payloads::read(&chunk)?.ok_or(EthError::Read("payloads"))?;
                    if t == 0 {
                        for j in 0..times.len() {
                            let bytes = payloads.get(j).ok_or(EthError::Read("a frame"))?;
                            p.frames += 1;
                            p.frame_bytes += bytes.len() as u64;
                            ticks.push(times.start(j));
                            frames.push(bytes.to_vec());
                        }
                        continue;
                    }
                    let values = RelationColumn::read(&chunk, StoredRelations::of(&def), 0)?
                        .ok_or(EthError::Read("the relation frame"))?;
                    for j in 0..times.len() {
                        let bytes = payloads.get(j).ok_or(EthError::Read("a header"))?;
                        let frame = usize::try_from(values.get(j))
                            .ok()
                            .and_then(|f| frames.get(f))
                            .and_then(|f| decode_frame(f));
                        let hit = match (t, frame) {
                            (1, Some((ip, _))) => Ipv4::from_bytes(bytes) == Some(ip),
                            (_, Some((_, Some(l)))) => L4::from_bytes(bytes) == Some(l),
                            _ => false,
                        };
                        p.matched += u64::from(hit);
                        if t == 1 {
                            p.ipv4 += 1;
                        } else {
                            p.l4 += 1;
                        }
                    }
                }
            }
        }
        p.digests.push(digest(
            ticks.iter().copied().zip(frames.iter().map(Vec::as_slice)),
        ));
    }
    out.piece_bytes = pieces.iter().map(|p| p.len() as u64).sum();
    Ok(out)
}

/// Why a capture could not be recorded or read: `E` is the store's error.
#[derive(Debug)]
#[non_exhaustive]
pub enum EthError<E = core::convert::Infallible> {
    /// A setting of [Link] the capture can't be made with.
    Settings(&'static str),
    /// The mux or a handle.
    Mux(MuxError<E>),
    /// A thread of the capture panicked or stopped early.
    Thread(&'static str),
    /// The chain in memory.
    Mem(ww_tape::MemError),
    /// A closed piece of the chain in memory.
    Pieces(example_capture::PieceError),
    /// Reading the chain back.
    Open(ww_tape::reader::ReadError<E>),
    /// The times, payloads or relation values of a chunk.
    Times(ww_tape::times::TimesError),
    Payloads(ww_tape::payloads::PayloadsError),
    /// What a chain holds isn't what the capture writes.
    Read(&'static str),
    /// Reading a chain back in memory, its error as text.
    Back(String),
}

impl<E: core::fmt::Debug> core::fmt::Display for EthError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Settings(what) => write!(f, "the capture: {what}"),
            Self::Mux(e) => write!(f, "the mux: {e:?}"),
            Self::Thread(what) => write!(f, "{what} stopped"),
            Self::Mem(e) => write!(f, "the chain in memory: {e:?}"),
            Self::Pieces(e) => write!(f, "{e}"),
            Self::Open(e) => write!(f, "reading the chain: {e:?}"),
            Self::Times(e) => write!(f, "a chunk's times or relation: {e:?}"),
            Self::Payloads(e) => write!(f, "a chunk's payloads: {e:?}"),
            Self::Read(what) => write!(f, "reading the chain: {what}"),
            Self::Back(e) => write!(f, "{e}"),
        }
    }
}

impl<E: core::fmt::Debug> core::error::Error for EthError<E> {}

impl<E> From<example_capture::PieceError> for EthError<E> {
    fn from(e: example_capture::PieceError) -> Self {
        Self::Pieces(e)
    }
}

impl<E> From<MuxError<E>> for EthError<E> {
    fn from(e: MuxError<E>) -> Self {
        Self::Mux(e)
    }
}

impl<E> From<ww_tape::reader::ReadError<E>> for EthError<E> {
    fn from(e: ww_tape::reader::ReadError<E>) -> Self {
        Self::Open(e)
    }
}

impl<E> From<ww_tape::times::TimesError> for EthError<E> {
    fn from(e: ww_tape::times::TimesError) -> Self {
        Self::Times(e)
    }
}

impl<E> From<ww_tape::payloads::PayloadsError> for EthError<E> {
    fn from(e: ww_tape::payloads::PayloadsError) -> Self {
        Self::Payloads(e)
    }
}
