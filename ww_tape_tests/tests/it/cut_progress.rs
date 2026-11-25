//! A cut makes progress (WRITE-1): a piece's cut budget counts from the end of its carry, so a chain or a mux whose
//! carry alone reaches the budget cuts by its data, never before every put.

use exact_units::Bytes;
use ww_tape::index::IndexPolicy;
use ww_tape::{
    ChainOptions, CutPolicy, Definitions, Describe, GroupItem, GroupPut, MemPieces, OwnerId,
    PieceChain, PieceTapes, SyncPolicy, TrackDef, TrackNum, TrackRepeat, WriterOptions,
};
use ww_tape_mux::{MuxOptions, TapeMux};

use crate::definitions::{CLOCK, clock, hold, labeled};

/// Tracks whose definitions and state the pieces carry: their carry alone is past [BUDGET].
const TRACKS: u32 = 10_000;
/// The cut budget.
const BUDGET: u64 = 4 << 20;
/// What a piece may hold past its carry and [BUDGET]: the put over it, the close and the tail.
const SLACK: u64 = 64 << 10;

/// A label of `repeat` sentences: at 20, [TRACKS] definitions take 3 budgets, and a piece cut after the first few
/// thousand tracks carries more than [BUDGET].
fn label(repeat: usize) -> &'static str {
    "a long label naming the slow series of a large project. "
        .repeat(repeat)
        .leak()
}

/// State track `number` on [CLOCK], labeled `label`.
fn state(number: u32, label: &'static str) -> Describe<'static> {
    let Describe::Track(t) = labeled(number, CLOCK, label) else {
        unreachable!("labeled() gives a track")
    };
    Describe::Track(TrackDef {
        repeat: TrackRepeat::State(ww_tape::StatePolicy::DEFAULT),
        ..t
    })
}

fn options() -> ChainOptions {
    ChainOptions {
        cut: CutPolicy {
            bytes: Bytes::new(BUDGET),
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

fn pieces(mem: &MemPieces) -> Vec<Vec<u8>> {
    crate::mux_multi::pieces(mem)
}

/// The test makes the case it names: a piece of `p` holds more than [BUDGET] and [SLACK], which only a carry left
/// out of the budget makes.
fn carry_past_budget(p: &[Vec<u8>]) {
    let longest = p.iter().map(Vec::len).max().unwrap_or(0) as u64;
    assert!(
        longest > BUDGET + SLACK,
        "a carry past the budget: the longest piece holds {longest} bytes"
    );
}

/// The owner of the tests' chains.
const OWNER: OwnerId = OwnerId([0x63; 32]);

/// The definitions of [CLOCK] and `tracks` state tracks labeled [label] (`repeat`).
fn defs(tracks: u32, repeat: usize) -> Definitions {
    let label = label(repeat);
    let mut all = vec![clock(CLOCK)];
    all.extend((1..=tracks).map(|n| state(n, label)));
    let mut defs = Definitions::new();
    hold(&mut defs, &all).unwrap();
    defs
}

/// Put an item on each of state tracks 1 to `tracks`, in groups of 500.
fn put_each(chain: &mut PieceChain<MemPieces>, defs: &Definitions, tracks: u32) {
    let values: Vec<[u8; 4]> = (1..=tracks).map(u32::to_le_bytes).collect();
    for group in (1..=tracks).collect::<Vec<_>>().chunks(500) {
        let puts: Vec<GroupPut<'_>> = group
            .iter()
            .map(|&n| GroupPut {
                track: TrackNum::new(n),
                ticks: 0,
                item: GroupItem::State(&values[n as usize - 1]),
            })
            .collect();
        chain.put_group(defs, &puts).unwrap();
    }
}

/// A piece resumed after a crash finds its carry again, the head and the blocks after it that belong with the next:
/// a carry past the budget and a put, resumed, take more puts without a cut. Fewer tracks with longer labels: a
/// resume reads each track's definitions.
#[test]
fn a_resumed_piece_finds_its_carry() {
    let defs = defs(1000, 200);
    let mem = MemPieces::new();
    let mut chain = PieceChain::create_on(mem.clone(), OWNER, 0, &[], options()).unwrap();
    put_each(&mut chain, &defs, 1000);
    chain.cut_now(&defs).unwrap();
    let carry = chain.writer().unwrap().carried_bytes();
    assert!(carry > BUDGET, "the case: a carry of {carry} bytes");
    chain.put(&defs, TrackNum::new(1), 1, &[1]).unwrap();
    let seq = chain.seq();
    drop(chain);
    let (mut chain, _) = PieceChain::resume_on(mem.clone(), OWNER, 0, options()).unwrap();
    assert_eq!(chain.seq(), seq);
    assert_eq!(chain.writer().unwrap().carried_bytes(), carry);
    for i in 0..64_u8 {
        chain
            .put(&defs, TrackNum::new(1), i64::from(i) + 2, &[i])
            .unwrap();
    }
    assert_eq!(chain.seq(), seq, "no cut: the data is far below the budget");
}

/// A chain of [TRACKS] state tracks, each put once, then puts on one of them of 3 budgets of data: the pieces
/// follow the data, one more for each budget, never one a put. Each piece holds at most its carry and the budget.
#[test]
fn a_chain_whose_carry_passes_the_budget_cuts_by_its_data() {
    let defs = defs(TRACKS, 20);
    let mem = MemPieces::new();
    let mut chain = PieceChain::create_on(mem.clone(), OWNER, 0, &[], options()).unwrap();
    put_each(&mut chain, &defs, TRACKS);
    let after_first = chain.seq();
    let mut carry = chain.writer().unwrap().carried_bytes();
    let item = vec![0x5a_u8; 1024];
    let puts = 3 * BUDGET / item.len() as u64;
    for i in 0..puts {
        chain
            .put(&defs, TrackNum::new(1), i as i64 + 1, &item)
            .unwrap();
        carry = carry.max(chain.writer().unwrap().carried_bytes());
        let data = (i + 1) * item.len() as u64;
        assert!(
            chain.seq() <= after_first + 1 + data / BUDGET,
            "put {i}: {} pieces for {data} bytes of puts",
            chain.seq()
        );
    }
    chain.close().unwrap();
    let p = pieces(&mem);
    carry_past_budget(&p);
    assert!(carry > BUDGET, "the case: a carry of {carry} bytes");
    for (i, piece) in p.iter().enumerate() {
        assert!(
            piece.len() as u64 <= carry + BUDGET + SLACK,
            "piece {i} of {} bytes past the carry and the budget",
            piece.len()
        );
    }
    assert!(p.len() >= 3, "3 budgets of data cut: {} pieces", p.len());
}

/// The same through the mux: [TRACKS] state tracks put once, 3 budgets of definitions, then 64 small puts: a
/// piece for each budget, the last one holding the puts, never a cut before each.
#[test]
fn a_mux_whose_carry_passes_the_budget_cuts_by_its_data() {
    let label = label(20);
    let mem = MemPieces::new();
    let mux = TapeMux::create(
        mem.clone(),
        OwnerId([0x64; 32]),
        0,
        &[],
        options(),
        MuxOptions::default(),
    )
    .unwrap();
    let Describe::Clock(c) = clock(CLOCK) else {
        unreachable!("clock() gives a clock")
    };
    mux.define_clock(c.spec()).unwrap();
    let mut handles = Vec::new();
    for n in 1..=TRACKS {
        let Describe::Track(t) = state(n, label) else {
            unreachable!("a track")
        };
        let track = mux.define_track(t.spec()).unwrap();
        let h = mux.state(track).unwrap();
        h.put(0, &n.to_le_bytes()).unwrap();
        handles.push(h);
    }
    for i in 0..64_i64 {
        handles[0].put(i + 1, &i.to_le_bytes()).unwrap();
    }
    drop(handles);
    mux.close().unwrap();
    let p = pieces(&mem);
    carry_past_budget(&p);
    // the data: each definition and item once, the puts, a record head each; the carries come on top
    let data = defs(TRACKS, 20).bytes() as u64 + u64::from(TRACKS + 64) * 32;
    assert!(
        p.len() as u64 <= 2 + data / BUDGET,
        "{} pieces for {data} bytes of data",
        p.len()
    );
}
