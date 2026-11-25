//! [TapeWriter::put_definitions] (MUX-5): refused before a write for an unknown track, for definitions no piece
//! holds and in a full piece the writer cannot cut; written whole beside a group and beside a unit, through the
//! writer and the mux.

use exact_units::Count;
use ww_tape::index::IndexError;
use ww_tape::sink::MemSink;
use ww_tape::{
    BlockKind, Definitions, DescribeError, FLAG_GROUP, FLAG_GROUP_START, FLAG_WITH_NEXT, GroupPut,
    InputError, MemPieces, TapeWriter, TrackNum, WriteError,
};
use ww_tape_mux::{MuxError, MuxOptions, TapeMux};

use crate::definitions::{CLOCK, clock, hold, state, track};
use crate::unit_puts::{EMPTY, kinds, relayed_units};

const STATE: TrackNum = TrackNum::new(3);

/// A writer whose pieces list at most `room` state entries, and the definitions of [CLOCK], tracks 1 and 2 and the
/// state track [STATE].
fn writer(room: u64) -> (TapeWriter<MemSink>, Definitions) {
    let mut w = crate::resume::begin(0);
    let mut index = crate::resume::options(0).index;
    index.state_entries_max = Count::new(room);
    w.set_index_policy(index).unwrap();
    let mut defs = Definitions::new();
    hold(
        &mut defs,
        &[clock(CLOCK), track(1, CLOCK), track(2, CLOCK), state(3)],
    )
    .unwrap();
    (w, defs)
}

#[test]
fn an_unknown_track_is_refused_with_nothing_written() {
    let (mut w, defs) = writer(64);
    let before = w.bytes();
    let r = w.put_definitions(&defs, &[TrackNum::new(1), TrackNum::new(9)]);
    assert!(
        matches!(
            r,
            Err(WriteError::Describe(DescribeError::UnknownTrack { track })) if track == TrackNum::new(9)
        ),
        "{r:?}"
    );
    assert_eq!(w.bytes(), before, "nothing written");
}

#[test]
fn definitions_no_piece_holds_are_refused_with_nothing_written() {
    // the layout and the producer take 2; the clock and two tracks 3 more
    let (mut w, defs) = writer(4);
    let before = w.bytes();
    let r = w.put_definitions(&defs, &[TrackNum::new(1), TrackNum::new(2)]);
    assert!(
        matches!(
            r,
            Err(WriteError::Input(InputError::CarriedEntries {
                needed: 5,
                max: 4
            }))
        ),
        "{r:?}"
    );
    assert_eq!(w.bytes(), before, "nothing written");
}

#[test]
fn a_full_piece_the_writer_cannot_cut_refuses_with_nothing_written() {
    let (mut w, defs) = writer(4);
    assert!(w.put_definitions(&defs, &[TrackNum::new(1)]).unwrap());
    let before = w.bytes();
    let r = w.put_definitions(&defs, &[TrackNum::new(2)]);
    assert!(
        matches!(r, Err(WriteError::Index(IndexError::StateEntries { .. }))),
        "{r:?}"
    );
    assert_eq!(w.bytes(), before, "nothing written");
}

/// The blocks of each group in `blocks`: from one marked [FLAG_GROUP_START] to its last, the last not marked
/// [FLAG_WITH_NEXT], the others marked.
pub(crate) fn groups(blocks: &[(BlockKind, u16)]) -> usize {
    let mut n = 0;
    let mut open = false;
    for (k, f) in blocks {
        if f & FLAG_GROUP == 0 {
            assert!(!open, "{k:?} {f:#x} inside a group: {blocks:x?}");
            continue;
        }
        if f & FLAG_GROUP_START != 0 {
            assert!(!open, "a group starts inside another: {blocks:x?}");
            n += 1;
        }
        open = f & FLAG_WITH_NEXT != 0;
    }
    assert!(!open, "a group left open: {blocks:x?}");
    n
}

#[test]
fn definitions_put_between_groups_leave_each_group_whole() {
    let (mut w, defs) = writer(64);
    let tape = w.sink().tape();
    let put = |item: &'static [u8]| {
        [GroupPut {
            track: STATE,
            ticks: 0,
            item,
        }]
    };
    w.put_group(&defs, &put(b"one")).unwrap();
    assert!(w.put_definitions(&defs, &[TrackNum::new(2)]).unwrap());
    w.put_group(&defs, &put(b"two")).unwrap();
    let blocks = kinds(&tape.to_vec().unwrap());
    assert_eq!(groups(&blocks), 2, "{blocks:x?}");
}

#[test]
fn definitions_put_between_units_leave_each_unit_whole() {
    let (defs, units) = relayed_units(false, 2);
    let mut w = crate::resume::begin(0);
    let tape = w.sink().tape();
    assert!(w.put_definitions(&defs, &[TrackNum::new(EMPTY)]).unwrap());
    for u in &units {
        w.write_block(&defs, &u[0]).unwrap();
        w.write_block(&defs, &u[1]).unwrap();
        w.put_definitions(&defs, &[TrackNum::new(EMPTY)]).unwrap();
    }
    let blocks = kinds(&tape.to_vec().unwrap());
    let chunks: Vec<usize> = (0..blocks.len())
        .filter(|i| blocks[*i].0 == BlockKind::Series)
        .collect();
    assert_eq!(chunks.len(), 4);
    for pair in chunks.chunks(2) {
        assert_eq!(
            pair[1],
            pair[0] + 1,
            "each unit's chunks together: {blocks:x?}"
        );
        assert!(blocks[pair[0]].1 & FLAG_WITH_NEXT != 0);
        assert_eq!(blocks[pair[1]].1 & FLAG_WITH_NEXT, 0);
    }
}

#[test]
fn the_mux_puts_definitions_between_groups_and_refuses_an_unknown_track() {
    let mem = MemPieces::new();
    let mux = TapeMux::create(
        mem.clone(),
        crate::follow::OWNER,
        0,
        &[],
        crate::follow::options(0),
        MuxOptions::default(),
    )
    .unwrap();
    let ww_tape::Describe::Clock(c) = clock(CLOCK) else {
        unreachable!("clock() gives a clock")
    };
    mux.define_clock(c.spec()).unwrap();
    for d in [track(1, CLOCK), track(2, CLOCK), state(3)] {
        let ww_tape::Describe::Track(t) = d else {
            unreachable!("a track")
        };
        mux.define_track(t.spec()).unwrap();
    }
    let group = mux.group(&[STATE]).unwrap();
    let put = |item: &'static [u8]| {
        [GroupPut {
            track: STATE,
            ticks: 0,
            item,
        }]
    };
    group.put_group(&put(b"one")).unwrap();
    mux.put_definitions(&[TrackNum::new(2)]).unwrap();
    group.put_group(&put(b"two")).unwrap();
    group.flush().unwrap();
    let r = mux.put_definitions(&[TrackNum::new(9)]);
    assert!(
        matches!(
            r,
            Err(MuxError::Refused(WriteError::Describe(
                DescribeError::UnknownTrack { .. }
            )))
        ),
        "{r:?}"
    );
    drop(group);
    mux.close().unwrap();
    let p = crate::mux_multi::pieces(&mem);
    assert_eq!(p.len(), 1);
    assert_eq!(groups(&kinds(&p[0])), 2);
    assert_eq!(
        crate::mux_definitions::def_records(&p[0], TrackNum::new(2)),
        1
    );
}

/// Puts the state-only test makes.
const STATE_PUTS: u64 = 300_000;
/// The cut budget of the state-only test.
const BUDGET: u64 = 1 << 20;
/// What a piece in memory may hold past [BUDGET]: its close and tail, padded to a page.
const PAGE: u64 = 4096;

/// A chain of puts alone is cut by its policy: 300 000 puts on one state track under a 1 MiB budget fill pieces of
/// at most the budget and a page, each naming the track and starting with the state the piece before ended in.
#[test]
fn a_state_only_chain_is_cut_by_its_policy() {
    let mem = MemPieces::new();
    let mut o = crate::follow::options(0);
    o.cut.bytes = exact_units::Bytes::new(BUDGET);
    let mux = TapeMux::create(
        mem.clone(),
        crate::follow::OWNER,
        0,
        &[],
        o,
        MuxOptions::default(),
    )
    .unwrap();
    let ww_tape::Describe::Clock(c) = clock(CLOCK) else {
        unreachable!("clock() gives a clock")
    };
    mux.define_clock(c.spec()).unwrap();
    let ww_tape::Describe::Track(t) = state(1) else {
        unreachable!("a track")
    };
    let track = mux.define_track(t.spec()).unwrap();
    let h = mux.state(track).unwrap();
    for i in 0..STATE_PUTS {
        h.put(i as i64, &i.to_le_bytes()).unwrap();
    }
    drop(h);
    mux.close().unwrap();
    let p = crate::mux_multi::pieces(&mem);
    let lens: Vec<usize> = p.iter().map(Vec::len).collect();
    assert!(p.len() > 2, "{lens:?}");
    let slot = ww_tape::Slot::State { track };
    let mut last: Option<Vec<u8>> = None;
    for (i, piece) in p.iter().enumerate() {
        assert!(
            piece.len() as u64 <= BUDGET + PAGE,
            "piece {i} of {} bytes",
            piece.len()
        );
        let r = ww_tape::TapeReader::open(&piece[..]).unwrap();
        let defs = r.definitions_at(0, u64::MAX).unwrap();
        assert!(
            defs.items
                .contains_key(&ww_tape::Slot::Track { number: track }),
            "piece {i} names the track"
        );
        let first = r
            .state_entries(0)
            .unwrap()
            .iter()
            .filter(|(_, s)| s.slot == slot)
            .map(|(e, _)| e.offset)
            .min()
            .unwrap();
        let at_start = r.state_at(0, slot, first + 1).unwrap().item.unwrap().item;
        if let Some(prev) = &last {
            assert_eq!(
                &at_start, prev,
                "piece {i} starts with the state the piece before ended in"
            );
        }
        last = Some(r.state_at(0, slot, u64::MAX).unwrap().item.unwrap().item);
    }
    assert_eq!(
        last,
        Some((STATE_PUTS - 1).to_le_bytes().to_vec()),
        "the last put"
    );
}

/// Pieces the ring of [a_ring_of_state_only_pieces_trims_as_it_goes] keeps: the one written and those before it.
const RING: u64 = 3;

/// A ring of pieces in memory, the oldest removed as the chain moves on, holds a state-only chain within its bound:
/// each piece kept reads alone, naming the track, with the state the run had put by its end.
#[test]
fn a_ring_of_state_only_pieces_trims_as_it_goes() {
    let mem = MemPieces::new();
    let mut o = crate::follow::options(0);
    o.cut.bytes = exact_units::Bytes::new(BUDGET);
    let mux = TapeMux::create(
        mem.clone(),
        crate::follow::OWNER,
        0,
        &[],
        o,
        MuxOptions::default(),
    )
    .unwrap();
    let ww_tape::Describe::Clock(c) = clock(CLOCK) else {
        unreachable!("clock() gives a clock")
    };
    mux.define_clock(c.spec()).unwrap();
    let ww_tape::Describe::Track(t) = state(1) else {
        unreachable!("a track")
    };
    let track = mux.define_track(t.spec()).unwrap();
    let h = mux.state(track).unwrap();
    let mut removed = 0;
    let mut held_max = 0;
    for i in 0..STATE_PUTS {
        h.put(i as i64, &i.to_le_bytes()).unwrap();
        if i % 10_000 == 9_999 {
            h.flush().unwrap();
            let now = mux.watch().get().piece;
            while removed + RING <= now {
                assert!(
                    mem.remove(&removed).unwrap(),
                    "piece {removed} there to trim"
                );
                removed += 1;
            }
            held_max = held_max.max(now + 1 - removed);
        }
    }
    drop(h);
    mux.close().unwrap();
    assert!(removed > 10, "{removed} pieces trimmed");
    assert!(held_max <= RING, "{held_max} pieces held at once");
    let slot = ww_tape::Slot::State { track };
    let kept: Vec<Vec<u8>> = (removed..)
        .map_while(|seq| mem.get(&seq).unwrap())
        .map(|t| t.to_vec().unwrap())
        .collect();
    assert!(!kept.is_empty() && kept.len() as u64 <= RING);
    for (i, piece) in kept.iter().enumerate() {
        let r = ww_tape::TapeReader::open(&piece[..]).unwrap();
        let defs = r.definitions_at(0, u64::MAX).unwrap();
        assert!(
            defs.items
                .contains_key(&ww_tape::Slot::Track { number: track })
        );
        assert!(
            r.state_at(0, slot, u64::MAX).unwrap().item.is_some(),
            "piece {i}"
        );
    }
    let last = ww_tape::TapeReader::open(&kept[kept.len() - 1][..]).unwrap();
    assert_eq!(
        last.state_at(0, slot, u64::MAX).unwrap().item.unwrap().item,
        (STATE_PUTS - 1).to_le_bytes(),
        "the last put"
    );
}
