//! A chain written through a [TapeMux]: producers on their own threads, a queue bounded by bytes, a sink slow or
//! failing, cuts and closes under live handles, a crash at any byte and the resume (MUX-1).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use exact_units::{Bytes, Count};
use ww_tape::index::IndexPolicy;
use ww_tape::piece::check_chain;
use ww_tape::{
    BlockKind, ChainOptions, CutPolicy, Definitions, Describe, ItemNum, Layout, MemError, MemPiece,
    MemPieces, MemSink, OwnerId, PieceChain, PieceStore, PieceTapes, SyncPolicy, TapeReader,
    TapeSink, TrackNum, WriterOptions,
};
use ww_tape_mux::{MuxError, MuxOptions, Pressure, RelayMode, Sent, TapeMux, Unit};

use crate::definitions::{base, hold, state};
use crate::pieces::{chunk, chunk_id, records};

const OWNER: OwnerId = OwnerId([0x3c; 32]);
/// The state track the tests put on.
const STATE: u32 = 10;

/// The tests' definitions: [base]'s clock and events tracks 1 to 9, and state track [STATE].
fn batch() -> Vec<Describe<'static>> {
    let mut b = base(Layout::UNALIGNED)[2..].to_vec();
    b.push(state(STATE));
    b
}

/// [batch] held as an application holds it.
fn defs() -> Definitions {
    let mut d = Definitions::new();
    hold(&mut d, &batch()).unwrap();
    d
}

/// Define [batch] on `mux`: each track and clock gets the number [defs] gives it.
fn define<T>(mux: &TapeMux<T>)
where
    T: PieceStore + Send + 'static,
    T::Sink: Send,
    T::Key: Send,
    T::Error: Send + Sync + std::fmt::Debug,
{
    for d in batch() {
        match d {
            Describe::Clock(c) => assert_eq!(mux.define_clock(c.spec()).unwrap(), c.number),
            Describe::Track(t) => assert_eq!(mux.define_track(t.spec()).unwrap(), t.number),
            _ => unreachable!("batch holds clocks and tracks"),
        }
    }
}

/// A chain cut every `chunks` chunk blocks (0: never), with no sync and no checkpoint by time.
fn options(chunks: u64) -> ChainOptions {
    ChainOptions {
        cut: CutPolicy {
            chunks: Count::new(chunks),
            ..CutPolicy::default()
        },
        writer: WriterOptions {
            sync: SyncPolicy::None,
            index: IndexPolicy {
                interval: None,
                ..IndexPolicy::default()
            },
            ..WriterOptions::default()
        },
        tapes: PieceTapes::Each,
    }
}

/// The bytes of pieces 0, 1, ... of `mem`, while there are any.
pub(crate) fn pieces(mem: &MemPieces) -> Vec<Vec<u8>> {
    (0..)
        .map_while(|seq| mem.get(&seq).unwrap())
        .map(|t| t.to_vec().unwrap())
        .collect()
}

/// The (first item, count) of every chunk of track `t` ([chunk]) in `pieces`, in item order.
pub(crate) fn items_of(pieces: &[Vec<u8>], t: u64) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    for p in pieces {
        let r = TapeReader::open(&p[..]).unwrap();
        for e in r.track_entries(0, chunk_id(t)).unwrap() {
            if e.kind == BlockKind::Events {
                out.push((e.first_item.get(), e.count));
            }
        }
    }
    out.sort_unstable();
    out
}

/// The end of the items of `items`, each chunk right after the one before from item 0.
pub(crate) fn contiguous(items: &[(u64, u64)]) -> u64 {
    let mut end = 0;
    for (first, count) in items {
        assert_eq!(*first, end, "the chunk after item {end}: {items:?}");
        end += count;
    }
    end
}

/// Every piece of `pieces` checks, and they form one whole chain.
pub(crate) fn whole(pieces: &[Vec<u8>]) {
    let mut summaries = Vec::new();
    for (i, p) in pieces.iter().enumerate() {
        let r = TapeReader::open(&p[..]).unwrap();
        assert!(r.verify(0).unwrap().holds, "piece {i}");
        summaries.extend(r.pieces().iter().filter_map(|p| p.summary()));
    }
    assert_eq!(summaries.len(), pieces.len(), "every piece closed");
    assert!(check_chain(&summaries).is_whole());
}

/// Track `t`'s chunk `k` ([chunk]), as a [ww_tape_mux::ChunkSender] takes it.
fn send_chunk<T: PieceStore>(
    s: &ww_tape_mux::ChunkSender<T>,
    t: u64,
    k: u64,
) -> Result<Sent, MuxError<T::Error>> {
    let (head, starts) = chunk(t, k);
    let column = crate::pieces::column(&starts)[0].0;
    s.send(head, vec![(column, starts)])
}

/// One producer through the mux writes the bytes the same calls write on a chain directly.
#[test]
fn one_producer_writes_the_bytes_of_a_chain_written_directly() {
    let direct = MemPieces::new();
    let defs = defs();
    let mut chain = PieceChain::create_on(direct.clone(), OWNER, 0, &[], options(5)).unwrap();
    for k in 0..12u64 {
        for t in 0..3 {
            let (h, c) = chunk(t, k);
            chain
                .write_chunk(&defs, &h, &crate::pieces::column(&c))
                .unwrap();
        }
        let id = chain.writer().unwrap().next_id();
        chain.write_block(&defs, &records(id, k as i64)).unwrap();
        if k.is_multiple_of(4) {
            chain
                .put(&defs, TrackNum::new(STATE), k as i64, &[k as u8; 8])
                .unwrap();
        }
    }
    chain.close().unwrap();

    let mem = MemPieces::new();
    let mux = TapeMux::create(
        mem.clone(),
        OWNER,
        0,
        &[],
        options(5),
        MuxOptions::default(),
    )
    .unwrap();
    define(&mux);
    let senders: Vec<_> = (0..3)
        .map(|t| mux.chunks(TrackNum::new(t + 1)).unwrap())
        .collect();
    let log = mux.records();
    let state = mux.state(TrackNum::new(STATE)).unwrap();
    for k in 0..12u64 {
        for (t, s) in senders.iter().enumerate() {
            assert_eq!(send_chunk(s, t as u64, k).unwrap(), Sent::Queued);
        }
        // any id: the mux numbers the block as the writer's next
        log.send(records(crate::id(999), k as i64)).unwrap();
        if k.is_multiple_of(4) {
            state.put(k as i64, &[k as u8; 8]).unwrap();
        }
    }
    mux.close().unwrap();
    assert!(pieces(&direct).len() > 3);
    assert_eq!(pieces(&mem), pieces(&direct));
}

/// Eight producer threads, each with its own track, the shared records track and the state track: every unit lands,
/// each track's items in order, across the cuts.
#[test]
fn eight_threads_write_one_chain() {
    const CHUNKS: u64 = 300;
    let mem = MemPieces::new();
    let mux = TapeMux::create(
        mem.clone(),
        OWNER,
        0,
        &[],
        options(64),
        MuxOptions::default(),
    )
    .unwrap();
    define(&mux);
    let producers: Vec<_> = (0..8u64)
        .map(|t| {
            let s = mux.chunks(TrackNum::new(t as u32 + 1)).unwrap();
            let log = mux.records();
            let state = mux.state(TrackNum::new(STATE)).unwrap();
            thread::spawn(move || {
                for k in 0..CHUNKS {
                    send_chunk(&s, t, k).unwrap();
                    if k.is_multiple_of(10) {
                        log.send(records(crate::id(k), k as i64)).unwrap();
                    }
                    if k.is_multiple_of(25) {
                        state.put(k as i64, &[t as u8; 4]).unwrap();
                    }
                }
                s.flush().unwrap();
            })
        })
        .collect();
    for p in producers {
        p.join().unwrap();
    }
    let stats = mux.stats();
    assert_eq!((stats.drops, stats.refused, stats.lost), (0, 0, 0));
    assert_eq!(stats.placed_units, 8 * (CHUNKS + CHUNKS / 10 + CHUNKS / 25));
    let closed = mux.close().unwrap();
    assert!(closed.len() > 8, "{} pieces", closed.len());
    let pieces = pieces(&mem);
    whole(&pieces);
    for t in 0..8 {
        assert_eq!(contiguous(&items_of(&pieces, t)), CHUNKS * 50, "track {t}");
    }
}

/// A store of tapes in memory whose sinks write only while the gate is open, and fail once told to.
#[derive(Clone, Default)]
pub(crate) struct Rigged {
    pub(crate) mem: MemPieces,
    pub(crate) rig: Arc<Rig>,
}

#[derive(Default)]
pub(crate) struct Rig {
    open: Mutex<bool>,
    opened: Condvar,
    pub(crate) fail: AtomicBool,
    writes: AtomicU64,
}

impl Rig {
    pub(crate) fn set(&self, open: bool) {
        *self.open.lock().unwrap() = open;
        self.opened.notify_all();
    }

    fn pass(&self) -> Result<(), MemError<u64>> {
        let mut open = self.open.lock().unwrap();
        while !*open {
            open = self.opened.wait(open).unwrap();
        }
        if self.fail.load(Ordering::SeqCst) {
            return Err(MemError::Poisoned);
        }
        self.writes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

impl Rigged {
    pub(crate) fn new() -> Self {
        let r = Self::default();
        r.rig.set(true);
        r
    }
}

pub(crate) struct RiggedSink {
    sink: MemSink<u64>,
    rig: Arc<Rig>,
}

impl TapeSink for RiggedSink {
    type Error = MemError<u64>;
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), Self::Error> {
        self.rig.pass()?;
        self.sink.write_all(bytes)
    }
    fn sync(&mut self) -> Result<(), Self::Error> {
        self.rig.pass()?;
        self.sink.sync()
    }
    fn close(&mut self) -> Result<(), Self::Error> {
        self.sink.close()
    }
    fn size(&self) -> Result<u64, Self::Error> {
        self.sink.size()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), Self::Error> {
        self.sink.read_at(offset, buf)
    }
    fn truncate(&mut self, len: u64) -> Result<(), Self::Error> {
        self.sink.truncate(len)
    }
    fn rebase(&mut self, base: u64) -> Result<(), Self::Error> {
        self.sink.rebase(base)
    }
    fn front(&self) -> Result<u64, Self::Error> {
        self.sink.front()
    }
}

impl PieceStore for Rigged {
    type Key = u64;
    type Error = MemError<u64>;
    type Sink = RiggedSink;
    type Source = MemPiece<u64>;
    fn key_of(&self, seq: u64) -> u64 {
        self.mem.key_of(seq)
    }
    fn create(&mut self, key: &u64) -> Result<RiggedSink, Self::Error> {
        let sink = self.mem.create(key)?;
        Ok(RiggedSink {
            sink,
            rig: self.rig.clone(),
        })
    }
    fn reopen(&mut self, key: &u64) -> Result<Option<RiggedSink>, Self::Error> {
        let rig = self.rig.clone();
        Ok(self.mem.reopen(key)?.map(|sink| RiggedSink { sink, rig }))
    }
    fn closed(&self, key: &u64) -> Result<Option<MemPiece<u64>>, Self::Error> {
        self.mem.closed(key)
    }
    fn create_whole(&mut self, key: &u64) -> Result<RiggedSink, Self::Error> {
        let sink = self.mem.create_whole(key)?;
        Ok(RiggedSink {
            sink,
            rig: self.rig.clone(),
        })
    }
    fn open_whole(&mut self, key: &u64) -> Result<Option<RiggedSink>, Self::Error> {
        let rig = self.rig.clone();
        Ok(self
            .mem
            .open_whole(key)?
            .map(|sink| RiggedSink { sink, rig }))
    }
    fn first_from(&self, from: u64) -> Result<Option<u64>, Self::Error> {
        self.mem.first_from(from)
    }
    fn remove(&mut self, key: &u64) -> Result<bool, Self::Error> {
        PieceStore::remove(&mut self.mem, key)
    }
    fn drop_front(&mut self, key: &u64, to: u64) -> Result<Option<u64>, Self::Error> {
        self.mem.drop_front(key, to)
    }
}

/// Wait until `done` holds, yielding: a condition another thread makes true, never a time.
pub(crate) fn until(mut done: impl FnMut() -> bool) {
    while !done() {
        thread::yield_now();
    }
}

/// A mux on `rigged` with a queue of `queue_bytes`, its definitions placed.
pub(crate) fn rigged_mux(rigged: &Rigged, queue_bytes: u64) -> TapeMux<Rigged> {
    let mux = TapeMux::create(
        rigged.clone(),
        OWNER,
        0,
        &[],
        options(0),
        MuxOptions {
            queue_bytes: Bytes::new(queue_bytes),
            ..MuxOptions::default()
        },
    )
    .unwrap();
    define(&mux);
    mux
}

/// A sink that stops writing makes producers that wait slow down to it: they block on the queue's budget, and once
/// the sink writes on, every unit lands.
#[test]
fn a_slow_sink_slows_waiting_producers_and_loses_nothing() {
    const CHUNKS: u64 = 60;
    let rigged = Rigged::new();
    let mux = rigged_mux(&rigged, 8 << 10);
    // made before the sink stalls: making one is a round trip to the writing thread
    let senders: Vec<_> = (0..8u64)
        .map(|t| mux.chunks(TrackNum::new(t as u32 + 1)).unwrap())
        .collect();
    mux.flush().unwrap();
    rigged.rig.set(false);
    let producers: Vec<_> = (0..8u64)
        .zip(senders)
        .map(|(t, s)| {
            thread::spawn(move || {
                for k in 0..CHUNKS {
                    assert_eq!(send_chunk(&s, t, k).unwrap(), Sent::Queued);
                }
            })
        })
        .collect();
    // the queue holds 8 KiB, about 16 chunks of 450 bytes: the rest of the 480 chunks wait
    until(|| mux.stats().waiting == 8);
    assert!(mux.stats().queued_bytes <= 8 << 10);
    rigged.rig.set(true);
    for p in producers {
        p.join().unwrap();
    }
    let stats = mux.stats();
    assert_eq!((stats.drops, stats.refused, stats.lost), (0, 0, 0));
    mux.close().unwrap();
    let pieces = pieces(&rigged.mem);
    whole(&pieces);
    for t in 0..8 {
        assert_eq!(contiguous(&items_of(&pieces, t)), CHUNKS * 50, "track {t}");
    }
}

/// Producers that drop lose whole chunks while the sink stalls: the next chunk of the track starts past them, the
/// reader sees the gap, and the mux counts each drop on its track. A producer that fails gets its unit back.
#[test]
fn dropping_producers_leave_item_gaps_and_failing_ones_get_the_unit_back() {
    let rigged = Rigged::new();
    let mux = rigged_mux(&rigged, 4 << 10);
    mux.flush().unwrap();
    rigged.rig.set(false);
    // --8<-- [start:pressure]
    let drop = mux
        .chunks(TrackNum::new(1))
        .unwrap()
        .pressure(Pressure::Drop);
    let fail = mux
        .chunks(TrackNum::new(2))
        .unwrap()
        .pressure(Pressure::Fail);
    let mut dropped = 0;
    for k in 0..40 {
        if send_chunk(&drop, 0, k).unwrap() == Sent::Dropped {
            dropped += 1;
        }
    }
    assert!(dropped > 20, "{dropped} dropped");
    match send_chunk(&fail, 1, 0) {
        Err(MuxError::Full(Unit::Chunk { head, .. })) => assert_eq!(head.first_item.get(), 0),
        other => panic!("{other:?}"),
    }
    // --8<-- [end:pressure]
    rigged.rig.set(true);
    let stats = mux.stats();
    assert_eq!(stats.drops, dropped);
    assert_eq!(stats.drops_by_track, [(TrackNum::new(1), dropped)]);
    mux.close().unwrap();
    let pieces = pieces(&rigged.mem);
    whole(&pieces);
    let items = items_of(&pieces, 0);
    assert_eq!(items.len() as u64, 40 - dropped);
    let lost: u64 = items
        .windows(2)
        .map(|w| w[1].0 - (w[0].0 + w[0].1))
        .sum::<u64>()
        + (40 * 50 - items.last().map_or(0, |l| l.0 + l.1));
    assert_eq!(lost, dropped * 50);
    assert_eq!(items_of(&pieces, 1), [] as [(u64, u64); 0]);
}

/// A failed write stops the chain: every handle's next call, the flushes, a cut and the close return the failure.
#[test]
fn a_failed_sink_reaches_every_handle() {
    let rigged = Rigged::new();
    let mux = rigged_mux(&rigged, 1 << 20);
    let chunks = mux.chunks(TrackNum::new(1)).unwrap();
    let log = mux.records();
    let state = mux.state(TrackNum::new(STATE)).unwrap();
    let relay = mux.relay(RelayMode::Adopt);
    send_chunk(&chunks, 0, 0).unwrap();
    log.send(records(crate::id(1), 1)).unwrap();
    mux.flush().unwrap();
    // --8<-- [start:failed]
    rigged.rig.fail.store(true, Ordering::SeqCst);
    // taken by the queue; the writing thread's write of it fails
    assert_eq!(send_chunk(&chunks, 0, 1).unwrap(), Sent::Queued);
    assert!(matches!(mux.flush(), Err(MuxError::Failed(_))));
    let failed = |r: Result<Sent, MuxError<MemError<u64>>>| matches!(r, Err(MuxError::Failed(_)));
    assert!(failed(send_chunk(&chunks, 0, 2)));
    assert!(failed(log.send(records(crate::id(2), 2))));
    assert!(failed(state.put(3, b"x")));
    assert!(failed(relay.send(records(crate::id(3), 3))));
    assert!(matches!(chunks.flush(), Err(MuxError::Failed(_))));
    assert!(matches!(state.sync(), Err(MuxError::Failed(_))));
    assert!(matches!(mux.cut(), Err(MuxError::Failed(_))));
    assert!(mux.stats().failed);
    assert!(matches!(mux.close(), Err(MuxError::Failed(_))));
    // --8<-- [end:failed]
    assert!(matches!(log.flush(), Err(MuxError::Failed(_))));
}

/// A unit the writer refuses comes back from that handle's next call; other handles and the mux write on.
#[test]
fn a_refused_unit_comes_back_from_its_handle() {
    let mem = MemPieces::new();
    let mux = TapeMux::create(
        mem.clone(),
        OWNER,
        0,
        &[],
        options(0),
        MuxOptions::default(),
    )
    .unwrap();
    define(&mux);
    let good = mux.chunks(TrackNum::new(1)).unwrap();
    let relay = mux.relay(RelayMode::Relay);
    // a records block on a track the relay's definitions don't hold: refused by the writer, not at the call
    let mut buf = [0u8; 256];
    let mut b = ww_tape::BlockWriter::new(
        &mut buf,
        BlockKind::Records,
        crate::id(5),
        &[ww_tape::ClockNum::new(1)],
    )
    .unwrap();
    b.push(
        TrackNum::new(30),
        ww_tape::ClockTime::new(ww_tape::ClockNum::new(1), 1),
        &[b"x"],
    )
    .unwrap();
    let len = b.finish();
    relay.send(buf[..len].to_vec()).unwrap();
    send_chunk(&good, 0, 0).unwrap();
    assert!(matches!(relay.flush(), Err(MuxError::Refused(_))));
    relay.flush().unwrap();
    good.flush().unwrap();
    assert_eq!(mux.stats().refused, 1);
    mux.close().unwrap();
    assert_eq!(contiguous(&items_of(&pieces(&mem), 0)), 50);
}

/// A close doesn't wait for the handles: a producer blocked on the budget gets [MuxError::Closed] back, what was
/// queued before the close lands, and every later call of any handle is refused.
#[test]
fn a_close_with_handles_out_refuses_their_later_calls() {
    let rigged = Rigged::new();
    let mux = rigged_mux(&rigged, 4 << 10);
    let late = mux.records();
    mux.flush().unwrap();
    rigged.rig.set(false);
    let s = mux.chunks(TrackNum::new(1)).unwrap();
    let producer = thread::spawn(move || {
        let mut sent = 0u64;
        loop {
            match send_chunk(&s, 0, sent) {
                Ok(Sent::Queued) => sent += 1,
                Ok(Sent::Dropped) => unreachable!("a waiting handle never drops"),
                Err(MuxError::Closed) => return (sent, s),
                Err(e) => panic!("{e}"),
            }
        }
    });
    until(|| mux.stats().waits >= 1);
    let stats = mux.stats();
    let closer = thread::spawn(move || mux.close());
    let (sent, s) = producer.join().unwrap();
    assert!(sent > 0);
    assert!(matches!(send_chunk(&s, 0, sent), Err(MuxError::Closed)));
    assert!(matches!(
        late.send(records(crate::id(1), 1)),
        Err(MuxError::Closed)
    ));
    assert!(matches!(late.flush(), Err(MuxError::Closed)));
    rigged.rig.set(true);
    closer.join().unwrap().unwrap();
    assert!(stats.queued_bytes <= 4 << 10);
    let pieces = pieces(&rigged.mem);
    whole(&pieces);
    assert_eq!(contiguous(&items_of(&pieces, 0)), sent * 50);
}

/// Producers keep sending while the mux cuts and then closes: each chunk lies whole in one piece, the pieces form one
/// chain, and every chunk a producer got [Sent::Queued] for is in it.
#[test]
fn cuts_and_a_close_under_live_handles() {
    let mem = MemPieces::new();
    // a small queue: the producers send as fast as they can until the close
    let small = MuxOptions {
        queue_bytes: Bytes::new(256 << 10),
        ..MuxOptions::default()
    };
    let mux = TapeMux::create(mem.clone(), OWNER, 0, &[], options(0), small).unwrap();
    define(&mux);
    let producers: Vec<_> = (0..4u64)
        .map(|t| {
            let s = mux.chunks(TrackNum::new(t as u32 + 1)).unwrap();
            let state = mux.state(TrackNum::new(STATE)).unwrap();
            thread::spawn(move || {
                let mut sent = 0u64;
                loop {
                    match send_chunk(&s, t, sent) {
                        Ok(_) => sent += 1,
                        Err(MuxError::Closed) => return sent,
                        Err(e) => panic!("{e}"),
                    }
                    if sent.is_multiple_of(16) && state.put(sent as i64, &[t as u8]).is_err() {
                        return sent;
                    }
                }
            })
        })
        .collect();
    for cut in 1..=3u64 {
        until(|| mux.stats().placed_units >= cut * 200);
        mux.cut().unwrap();
    }
    until(|| mux.stats().placed_units >= 800);
    let closed = mux.close().unwrap();
    assert_eq!(closed.len(), 4);
    let sent: Vec<u64> = producers.into_iter().map(|p| p.join().unwrap()).collect();
    let pieces = pieces(&mem);
    whole(&pieces);
    for (t, n) in sent.iter().enumerate() {
        assert_eq!(
            contiguous(&items_of(&pieces, t as u64)),
            n * 50,
            "track {t}"
        );
    }
}

/// The chain [crash_ops] writes through a mux dropped unclosed: the bytes of its pieces, the last unclosed.
fn crashed_chain() -> Vec<Vec<u8>> {
    let mem = MemPieces::new();
    let mux = TapeMux::create(
        mem.clone(),
        OWNER,
        0,
        &[],
        options(6),
        MuxOptions::default(),
    )
    .unwrap();
    define(&mux);
    let s: Vec<_> = (0..2)
        .map(|t| mux.chunks(TrackNum::new(t + 1)).unwrap())
        .collect();
    let log = mux.records();
    let state = mux.state(TrackNum::new(STATE)).unwrap();
    for k in 0..8u64 {
        for (t, s) in s.iter().enumerate() {
            send_chunk(s, t as u64, k).unwrap();
        }
        log.send(records(crate::id(k), k as i64)).unwrap();
        state.put(k as i64, &[k as u8]).unwrap();
    }
    drop(mux);
    pieces(&mem)
}

/// A store holding the closed pieces `done` and the unclosed start `open` of the next.
fn plant(done: &[Vec<u8>], open: Option<&[u8]>) -> MemPieces {
    let mem = MemPieces::new();
    for (j, w) in done.iter().enumerate() {
        let mut s = mem.create(&(j as u64)).unwrap();
        s.write_all(w).unwrap();
        s.close().unwrap();
    }
    if let Some(open) = open {
        let mut s = mem.create(&(done.len() as u64)).unwrap();
        s.write_all(open).unwrap();
    }
    mem
}

/// A chain written through a mux and cut off at any byte resumes through a mux: the tracks get their numbers back,
/// and each track goes on right after the last chunk kept.
#[test]
fn a_crash_at_any_byte_resumes_through_a_mux() {
    let crashed = crashed_chain();
    assert_eq!(crashed.len(), 3);
    let (done, last) = crashed.split_at(crashed.len() - 1);
    let mut cuts: Vec<(usize, Option<usize>)> = vec![(done.len(), None)];
    // in the open piece and in each piece before it, read as the open one: every block boundary, the bytes either
    // side of it, and bytes in between
    for (i, p) in done.iter().chain(last).enumerate() {
        let mut at: Vec<usize> = crate::resume::cut_points(p, 64)
            .into_iter()
            .flat_map(|b| [b.saturating_sub(1), b, b + 1])
            .filter(|&b| b <= p.len())
            .collect();
        at.sort_unstable();
        at.dedup();
        cuts.extend(at.into_iter().map(|at| (i, Some(at))));
    }
    assert!(cuts.len() > 300, "{} crashes", cuts.len());
    for (i, at) in cuts {
        let open = at.map(|at| &crashed[i][..at]);
        let mem = plant(&crashed[..i], open);
        let (mux, resumed) =
            TapeMux::resume(mem.clone(), OWNER, 0, options(6), MuxOptions::default())
                .unwrap_or_else(|e| panic!("piece {i} cut at {at:?}: {e}"));
        define(&mux);
        for t in 0..2u64 {
            let mut next = contiguous(&items_of(&crashed[..i], t));
            for e in resumed.entries.iter().filter(|e| e.track_id == chunk_id(t)) {
                if e.kind == BlockKind::Events {
                    assert_eq!(e.first_item, ItemNum::new(next), "piece {i} cut at {at:?}");
                    next = e.end_item().get();
                }
            }
            let s = mux.chunks(TrackNum::new(t as u32 + 1)).unwrap();
            send_chunk(&s, t, next / 50).unwrap();
        }
        mux.close().unwrap();
        let pieces = pieces(&mem);
        whole(&pieces);
        for t in 0..2 {
            let end = contiguous(&items_of(&pieces, t));
            assert!(end >= 50, "piece {i} cut at {at:?}");
        }
    }
}

/// A mux on a directory of piece files: written, dropped unclosed, resumed and closed by path.
#[test]
fn a_mux_on_files_resumes_by_path() {
    let dir = crate::pieces::scratch("mux_files");
    let mux =
        ww_tape_fs::mux::create(&dir, OWNER, 0, &[], options(4), MuxOptions::default()).unwrap();
    define(&mux);
    let s = mux.chunks(TrackNum::new(1)).unwrap();
    for k in 0..10 {
        send_chunk(&s, 0, k).unwrap();
    }
    s.sync().unwrap();
    drop(mux);
    let (mux, resumed) =
        ww_tape_fs::mux::resume(&dir, OWNER, 0, options(4), MuxOptions::default()).unwrap();
    assert_ne!(resumed.entries, [] as [ww_tape::IndexEntry; 0]);
    define(&mux);
    let s = mux.chunks(TrackNum::new(1)).unwrap();
    for k in 10..14 {
        send_chunk(&s, 0, k).unwrap();
    }
    let closed = mux.close().unwrap();
    let pieces: Vec<Vec<u8>> = (0..closed.len() as u64)
        .map(|seq| std::fs::read(dir.join(ww_tape_fs::chain::piece_name(seq))).unwrap())
        .collect();
    whole(&pieces);
    assert_eq!(contiguous(&items_of(&pieces, 0)), 14 * 50);
    std::fs::remove_dir_all(&dir).unwrap();
}
