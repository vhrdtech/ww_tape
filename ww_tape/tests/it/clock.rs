//! Time reaches a writer only through its clock: intervals are due when the clock says so, never without one.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use ww_tape::index::IndexPolicy;
use ww_tape::{
    BlockKind, ChainOptions, Clock, ClockRef, CutPolicy, FileBlocks, MemSink, MemTapes, OwnerId,
    PieceChain, PieceFiles, Recovery, SyncPolicy, TapeWriter, WriterOptions,
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
        Path::new("c"),
        OwnerId([7; 32]),
        0,
        &[],
        ChainOptions {
            files: PieceFiles::Each,
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
    let mut w = TapeWriter::create_on(MemSink::new(Path::new("p")), SyncPolicy::None);
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
