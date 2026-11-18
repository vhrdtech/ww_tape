//! Where a writer's puts go and what they cost: the state-entry cap at a cut and a finish, deferred blocks placed
//! with no puts, relayed puts with what they need, the work a block of data costs when nothing is written (writing
//! side review 3).

use std::path::Path;

use crate::definitions::{
    CLOCK, Rig, base, begin, labeled, on_tracks, options, puts, puts_of, record_blocks, state,
    track, write_on, writes_of,
};
use crate::pieces::{chunk, column, scratch};
use ww_tape::index::IndexPolicy;
use ww_tape::{
    ALIGNMENT, BlockId, BlockKind, BlockWriter, ChainOptions, ClockTime, CutPolicy, DEFER_WINDOW,
    DescribeError, HEAD_ENTRIES, IndexError, Layout, MemSink, OwnerId, PieceChain, SessionTag,
    Slot, SyncPolicy, TapeReader, TapeWriter, WriteError, WriterOptions,
};

/// An aligned writer on a memory sink with [base] held, no checkpoints, at most `max` state entries a piece.
fn aligned(max: usize) -> Rig {
    let mut w = Rig::new(TapeWriter::create_on(
        MemSink::new(Path::new("a")),
        SyncPolicy::None,
    ));
    w.set_index_policy(IndexPolicy {
        state_entries_max: max,
        ..IndexPolicy::NONE
    })
    .unwrap();
    w.set_alignment(ALIGNMENT).unwrap();
    w.define_all(&base(Layout::ALIGNED)).unwrap();
    w
}

/// Whether `e` is the cap's refusal.
fn is_cap(e: &WriteError) -> bool {
    matches!(e, WriteError::Index(IndexError::StateEntries { .. }))
}

/// The offset of the record block of data (not put) on `track` in `bytes`.
fn data_on(bytes: &[u8], track: u32) -> u64 {
    record_blocks(bytes)
        .into_iter()
        .find(|b| b.first_track == Some(track) && !b.repeat)
        .expect("the block of data is in the piece")
        .offset
}

/// A deferred block's definitions are put when it is deferred and none when it is placed: puts that fill the
/// piece's state entries place it on their way, and the piece still finishes, the block after its definition
/// (writing side review 3, point 1).
#[test]
fn a_full_cap_never_stops_a_finish() {
    let mut w = aligned(16);
    w.define(&state(12)).unwrap();
    let tape = w.sink().tape();
    w.defer_block(on_tracks(w.next_id(), &[(2, 1, b"deferred")]))
        .unwrap();
    let mut k = 0u8;
    let e = loop {
        match w.put(12, i64::from(k), &[k]) {
            Ok(_) => k += 1,
            Err(e) => break e,
        }
    };
    assert!(is_cap(&e), "{e:?}");
    assert_eq!(w.state_entries(), 16);
    w.finish().unwrap();
    let bytes = tape.to_vec().unwrap();
    let data = data_on(&bytes, 2);
    assert!(writes_of(&bytes, Slot::Track { number: 2 })[0] < data);
}

/// The same in a chain: a deferred block waits while puts fill the piece; the chain cuts, the block is placed in the
/// piece it was deferred in with its definition before it, and the chain finishes (writing side review 3, point 1).
#[test]
fn a_full_cap_never_stops_a_cut() {
    let dir = scratch("cap_cut");
    let mut chain = PieceChain::create(
        &dir,
        OwnerId([0x44; 32]),
        0,
        &[],
        ChainOptions {
            files: ww_tape::PieceFiles::Each,
            writer: WriterOptions {
                sync: SyncPolicy::None,
                index: IndexPolicy {
                    state_entries_max: 12,
                    ..IndexPolicy::NONE
                },
                layout: Layout::ALIGNED,
                ..WriterOptions::default()
            },
            cut: CutPolicy::default(),
        },
    )
    .unwrap();
    let mut defs = crate::definitions::base_defs().clone();
    defs.define(&state(12)).unwrap();
    let id = chain.writer().unwrap().next_id();
    chain
        .defer_block(&defs, on_tracks(id, &[(2, 1, b"deferred")]))
        .unwrap();
    for k in 0..12u8 {
        chain.put(&defs, 12, i64::from(k), &[k]).unwrap();
    }
    let pieces = chain.finish().unwrap();
    assert!(pieces.len() >= 2, "the chain cut");
    let bytes = std::fs::read(&pieces[0].path).unwrap();
    let data = data_on(&bytes, 2);
    assert!(writes_of(&bytes, Slot::Track { number: 2 })[0] < data);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A record block numbered `id` of one record of `len` bytes on `track`: longer than any gap before an aligned
/// chunk.
fn long_records(id: BlockId, track: u32, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len + 256];
    let mut b = BlockWriter::new(&mut buf, BlockKind::Records, id, &[CLOCK]).unwrap();
    b.push(track, ClockTime::new(CLOCK, 1), &[&vec![7u8; len]])
        .unwrap();
    let n = b.finish();
    buf.truncate(n);
    buf
}

/// A deferred block that no gap takes waits at most [DEFER_WINDOW] bytes: chunks carry the piece on, and it is
/// placed before one would take it further; it lies at most track 0's near plus the window after its definition
/// (writing side review 3, point 1).
#[test]
fn a_deferred_block_waits_at_most_the_window() {
    let mut w = aligned(IndexPolicy::NONE.state_entries_max);
    let tape = w.sink().tape();
    w.defer_block(long_records(w.next_id(), 2, 2 * ALIGNMENT as usize))
        .unwrap();
    let deferred_at = w.bytes();
    let mut k = 0;
    while w.bytes() < deferred_at + 2 * DEFER_WINDOW {
        let (h, c) = chunk(0, k);
        let defs = w.defs.clone();
        w.w.write_chunk(&defs, &h, &column(&c)).unwrap();
        k += 1;
    }
    w.finish().unwrap();
    let bytes = tape.to_vec().unwrap();
    let data = data_on(&bytes, 2);
    assert!(data > deferred_at, "it waited");
    assert!(data - deferred_at <= DEFER_WINDOW, "{}", data - deferred_at);
    let def = writes_of(&bytes, Slot::Track { number: 2 })[0];
    let near = Layout::ALIGNED.definitions.near.get();
    assert!(data - def <= u64::from(near) + DEFER_WINDOW);
}

/// A relayed put gets what it needs as a put does: after a cut, a producer's block holding only a put on track 7
/// puts track 7's definition in the new piece before it (writing side review 3, point 2).
#[test]
fn a_relayed_put_takes_its_definitions_into_each_piece() {
    let mut w = begin(options(u64::MAX));
    w.define(&state(7)).unwrap();
    let producer = BlockId::new(SessionTag::random(), 3);
    w.write_block(&puts_of(producer, &[], &[(7, 1, b"A")]))
        .unwrap();
    let Rig { w: first, defs } = w;
    let (_, next) = first.cut_on(MemSink::new(Path::new("next"))).unwrap();
    let mut w = Rig { w: next, defs };
    let tape = w.sink().tape();
    w.write_block(&puts_of(producer.next(), &[], &[(7, 2, b"B")]))
        .unwrap();
    w.finish().unwrap();
    let bytes = tape.to_vec().unwrap();
    let state_at = writes_of(&bytes, Slot::State { track: 7 });
    let def_at = writes_of(&bytes, Slot::Track { number: 7 });
    assert!(!def_at.is_empty() && def_at[0] <= state_at[0]);
    let r = TapeReader::open(&bytes[..]).unwrap();
    let found = r.state_at(0, Slot::State { track: 7 }, u64::MAX).unwrap();
    assert_eq!(found.item.unwrap().item, b"B");
}

/// A relayed block of many definitions is put in as few blocks as they fit, as a put of them is (writing side
/// review 3, point 4).
#[test]
fn a_relayed_block_of_definitions_is_put_in_few_blocks() {
    let mut w = begin(options(u64::MAX));
    let tape = w.sink().tape();
    let before = puts(&tape.to_vec().unwrap()).len();
    let tracks: Vec<_> = (100..1100).map(|n| track(n, CLOCK)).collect();
    let block = puts_of(BlockId::new(SessionTag::random(), 1), &tracks, &[]);
    let mut defs = w.defs.clone();
    w.w.relay_block(&mut defs, &block).unwrap();
    let bytes = tape.to_vec().unwrap();
    let blocks = puts(&bytes).len() - before;
    assert!(blocks <= 2, "{blocks} blocks for 1000 definitions");
    assert_eq!(defs.len(), w.defs.len() + 1000);
}

/// A block of data whose definitions lie close behind costs no hashing and no copying, however long they are: each
/// definition's hash is kept with it (writing side review 3, point 3).
#[test]
fn a_block_whose_definitions_lie_behind_costs_no_work() {
    let mut w = begin(options(u64::MAX));
    let label: &'static str = Box::leak("x".repeat(60 << 10).into_boxed_str());
    w.define(&labeled(2, CLOCK, label)).unwrap();
    write_on(&mut w, 2, 0, 1);
    let first = w.put_work();
    assert!(first.copied.get() >= 60 << 10, "the definition was put");
    write_on(&mut w, 2, 1, 10);
    assert_eq!(w.put_work(), first, "nothing hashed or copied");
}

/// A chunk refused leaves nothing of its own: an events chunk on a state track is checked before the definitions it
/// needs are put (writing side review 3, point 5).
#[test]
fn a_refused_chunk_leaves_no_puts() {
    let mut w = aligned(IndexPolicy::NONE.state_entries_max);
    w.define(&state(1)).unwrap();
    let tape = w.sink().tape();
    write_on(&mut w, 2, 0, 1);
    let len = tape.to_vec().unwrap().len();
    let (h, c) = chunk(0, 0);
    let defs = w.defs.clone();
    let e = w.w.write_chunk(&defs, &h, &column(&c)).unwrap_err();
    assert!(
        matches!(
            e,
            WriteError::Describe(DescribeError::StateChunk { track: 1 })
        ),
        "{e:?}"
    );
    assert_eq!(tape.to_vec().unwrap().len(), len, "nothing written");
}

/// A cap without room for a piece head's layout and producer is refused, on a writer and in a chain's options
/// (writing side review 3, point 7).
#[test]
fn a_cap_below_a_piece_head_is_refused() {
    let mut w = TapeWriter::create_on(MemSink::new(Path::new("c")), SyncPolicy::None);
    let small = IndexPolicy {
        state_entries_max: HEAD_ENTRIES - 1,
        ..IndexPolicy::NONE
    };
    assert!(matches!(
        w.set_index_policy(small),
        Err(WriteError::Index(IndexError::StateEntries { .. }))
    ));
    w.set_index_policy(IndexPolicy {
        state_entries_max: HEAD_ENTRIES,
        ..IndexPolicy::NONE
    })
    .unwrap();
    let dir = scratch("cap_small");
    let chain = PieceChain::create(
        &dir,
        OwnerId([0x45; 32]),
        0,
        &[],
        ChainOptions {
            files: ww_tape::PieceFiles::Each,
            writer: WriterOptions {
                index: small,
                ..WriterOptions::default()
            },
            cut: CutPolicy::default(),
        },
    );
    assert!(chain.is_err());
    std::fs::remove_dir_all(&dir).unwrap();
}
