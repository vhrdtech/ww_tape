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
fn groups(blocks: &[(BlockKind, u16)]) -> usize {
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
        assert!(blocks[pair[1]].1 & FLAG_WITH_NEXT == 0);
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
