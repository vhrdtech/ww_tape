//! Time reaches a writer only through its clock: intervals are due when the clock says so, never without one.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use ww_tape::index::IndexPolicy;
use ww_tape::{
    BlockKind, ChainOptions, Clock, ClockRef, CutPolicy, FileBlocks, MemSink, MemTapes, OwnerId,
    PieceChain, PieceTapes, Recovery, SyncPolicy, TapeWriter, WriterOptions,
};

use crate::pieces::{chunk, column};

/// A clock the test moves by hand.
#[derive(Clone, Default)]
struct HandClock(Arc<AtomicU64>);

impl HandClock {
    fn advance(&self, ms: u64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

impl Clock for HandClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

fn chain(clock: ClockRef) -> PieceChain<MemTapes> {
    PieceChain::create_on(
        MemTapes::new(),
        OwnerId([7; 32]),
        0,
        &[],
        ChainOptions {
            tapes: PieceTapes::Each,
            writer: WriterOptions {
                sync: SyncPolicy::None,
                clock,
                ..Default::default()
            },
            cut: CutPolicy {
                interval: Some(Duration::from_secs(1)),
                ..Default::default()
            },
        },
    )
    .unwrap()
}

/// A chain cuts once its piece is as old as the interval by the chain's clock, not a millisecond before.
#[test]
fn a_chain_cuts_when_its_clock_says_the_interval_is_up() {
    let hand = HandClock::default();
    let mut c = chain(ClockRef::new(hand.clone()));
    let (h, b) = chunk(0, 0);
    c.write_chunk(crate::definitions::base_defs(), &h, &column(&b))
        .unwrap();
    hand.advance(999);
    assert!(!c.cut_due(), "999 ms into a 1 s interval");
    hand.advance(1);
    assert!(c.cut_due(), "1 s into a 1 s interval");
    let (h, b) = chunk(0, 1);
    c.write_chunk(crate::definitions::base_defs(), &h, &column(&b))
        .unwrap();
    assert_eq!(c.seq(), 1, "the chunk went into a new piece");
    assert!(!c.cut_due(), "the new piece's interval starts at its cut");
}

/// Without a clock no interval is ever due: only the byte and chunk rules cut.
#[test]
fn without_a_clock_no_interval_is_due() {
    let mut c = chain(ClockRef::NONE);
    for k in 0..3 {
        let (h, b) = chunk(0, k);
        c.write_chunk(crate::definitions::base_defs(), &h, &column(&b))
            .unwrap();
        assert!(!c.cut_due());
    }
    assert_eq!(c.seq(), 0);
}

/// A writer places a checkpoint index at a tick once the index interval is up by its clock.
#[test]
fn a_checkpoint_is_placed_when_the_clock_says_the_interval_is_up() {
    let hand = HandClock::default();
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    w.set_clock(ClockRef::new(hand.clone()));
    w.set_index_policy(IndexPolicy {
        bytes: 0,
        interval: Some(Duration::from_secs(2)),
        ..IndexPolicy::default()
    })
    .unwrap();
    let (h, b) = chunk(0, 0);
    w.write_chunk(crate::definitions::base_defs(), &h, &column(&b))
        .unwrap();
    let indexes = |w: &TapeWriter<MemSink>| {
        let bytes = w.sink().tape().to_vec().unwrap();
        let mut rd = FileBlocks::with_recovery(&bytes[..], Recovery::Prefix);
        let mut n = 0;
        while let Some(Ok(b)) = rd.next_block() {
            n += usize::from(b.kind == BlockKind::Index);
        }
        n
    };
    hand.advance(1999);
    w.tick().unwrap();
    assert_eq!(indexes(&w), 0, "1999 ms into a 2 s interval");
    hand.advance(1);
    w.tick().unwrap();
    assert_eq!(indexes(&w), 1, "2 s into a 2 s interval");
}

/// Default options are equal: each takes the system clock, and two of them read the same time.
#[test]
fn default_options_are_equal() {
    assert_eq!(WriterOptions::default(), WriterOptions::default());
    assert_eq!(ClockRef::default(), ClockRef::default());
    let hand = ClockRef::new(HandClock::default());
    assert_eq!(hand, hand.clone());
    assert_ne!(hand, ClockRef::new(HandClock::default()));
    assert_ne!(hand, ClockRef::default());
}

/// A [MemSink] that counts its syncs.
struct Syncs(MemSink, Arc<AtomicU64>);

impl ww_tape::TapeSink for Syncs {
    type Error = ww_tape::MemError;
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), Self::Error> {
        self.0.write_all(bytes)
    }
    fn sync(&mut self) -> Result<(), Self::Error> {
        self.1.fetch_add(1, Ordering::SeqCst);
        self.0.sync()
    }
    fn close(&mut self) -> Result<(), Self::Error> {
        self.0.close()
    }
    fn size(&self) -> Result<u64, Self::Error> {
        self.0.size()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), Self::Error> {
        self.0.read_at(offset, buf)
    }
    fn truncate(&mut self, len: u64) -> Result<(), Self::Error> {
        self.0.truncate(len)
    }
    fn rebase(&mut self, base: u64) -> Result<(), Self::Error> {
        self.0.rebase(base)
    }
}

/// [SyncPolicy::Interval] syncs a block that waits when a later block or a tick finds the interval up, and a tick
/// with nothing waiting syncs nothing.
#[test]
fn a_tick_syncs_a_quiet_input_once_the_interval_is_up() {
    let (hand, syncs) = (HandClock::default(), Arc::new(AtomicU64::new(0)));
    let mut w = TapeWriter::create_on(
        Syncs(MemSink::new(), Arc::clone(&syncs)),
        SyncPolicy::Interval(Duration::from_secs(1)),
    );
    w.set_clock(ClockRef::new(hand.clone()));
    w.set_session(crate::TEST_SESSION).unwrap();
    let defs = crate::definitions::base_defs();
    let count = || syncs.load(Ordering::SeqCst);
    hand.advance(400);
    w.write_block(defs, &crate::pieces::records(w.next_id(), 0))
        .unwrap();
    assert_eq!(count(), 0, "400 ms into a 1 s interval: the block waits");
    hand.advance(599);
    w.tick().unwrap();
    assert_eq!(count(), 0, "999 ms");
    hand.advance(1);
    w.tick().unwrap();
    assert_eq!(count(), 1, "1 s: the waiting block is synced");
    hand.advance(5_000);
    w.tick().unwrap();
    assert_eq!(count(), 1, "nothing waits: no sync");
    w.write_block(defs, &crate::pieces::records(w.next_id(), 1))
        .unwrap();
    assert_eq!(
        count(),
        2,
        "a block 5 s after the last sync is synced at once"
    );
}
