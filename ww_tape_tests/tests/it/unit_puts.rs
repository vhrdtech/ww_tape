//! Puts while a relayed unit is open (FMT-15): a put, a typed item and the definitions of a track without data go
//! into the unit's run, marked, and never split it; a definition written twice has its copy after the unit; a group,
//! whose last block is its own, is refused with nothing written.

use ww_tape::sink::MemSink;
use ww_tape::{
    Block, BlockKind, Definitions, GroupPut, GroupRefusal, InputError, TapeWriter, TrackNum,
    WriteError,
};

use crate::definitions::{CLOCK, hold, state, track};
use crate::follow::walk;
use crate::grids::{STEPS, head, samples_column, unit_of, unit_writer};
use crate::mux_items::item;

/// A track without data, beside the grid track.
pub(crate) const EMPTY: u32 = 2;
/// A state track.
const STATE: u32 = 3;

/// The definitions of the grid track, [EMPTY] (written twice at once when `twice`) and [STATE], and the blocks of
/// `units` units of the grid track, one run each: each unit's first chunk marked, its last not.
pub(crate) fn relayed_units(twice: bool, units: u64) -> (Definitions, Vec<[Vec<u8>; 2]>) {
    let (mut defs, mut src, id, tiles) = unit_writer();
    let samples = [0u8; 4 * STEPS as usize];
    let columns = samples_column(&samples, samples.len());
    let tape = src.sink().tape();
    let placed: Vec<_> = (0..units)
        .map(|run| {
            let heads: Vec<_> = tiles.iter().take(2).map(|t| head(id, run, *t)).collect();
            src.write_chunk_unit(&defs, &unit_of(&heads, &columns))
                .unwrap()
        })
        .collect();
    drop(src);
    let bytes = tape.to_vec().unwrap();
    let block = |p: &ww_tape::Placed| {
        let at = usize::try_from(p.offset).unwrap();
        bytes[at..at + p.len as usize].to_vec()
    };
    let blocks = placed
        .iter()
        .map(|u| [block(&u[0]), block(&u[1])])
        .collect();
    hold(&mut defs, &[empty_track(twice), state(STATE)]).unwrap();
    (defs, blocks)
}

/// The definition of [EMPTY], written twice at once when `twice`.
pub(crate) fn empty_track(twice: bool) -> ww_tape::Describe<'static> {
    let ww_tape::Describe::Track(mut empty) = track(EMPTY, CLOCK) else {
        unreachable!("track() gives a track")
    };
    empty.defined_twice = twice;
    ww_tape::Describe::Track(empty)
}

/// The kind and flags of each block of `bytes`.
pub(crate) fn kinds(bytes: &[u8]) -> Vec<(BlockKind, u16)> {
    walk(bytes)
        .into_iter()
        .map(|(_, b)| {
            let b = Block::read(&b).unwrap();
            (b.kind, b.flags)
        })
        .collect()
}

/// A relay writer given the unit's first chunk, then `puts`, then its last chunk; its bytes. The unit stays open
/// across the puts.
fn relay(
    twice: bool,
    puts: impl FnOnce(&mut TapeWriter<MemSink>, &Definitions),
) -> Vec<(BlockKind, u16)> {
    let (defs, units) = relayed_units(twice, 1);
    let mut w = crate::resume::begin(0);
    let tape = w.sink().tape();
    w.write_block(&defs, &units[0][0]).unwrap();
    assert!(w.unit_open(), "the unit's first chunk opens it");
    puts(&mut w, &defs);
    assert!(w.unit_open(), "a put inside the unit leaves it open");
    w.write_block(&defs, &units[0][1]).unwrap();
    assert!(!w.unit_open(), "the unit's last chunk ends it");
    drop(w);
    kinds(&tape.to_vec().unwrap())
}

/// Every block from the unit's first chunk to its last is marked [ww_tape::FLAG_WITH_NEXT], with at least one
/// between them; the records blocks after the last chunk, none of them marked.
pub(crate) fn whole(blocks: &[(BlockKind, u16)]) -> Vec<(BlockKind, u16)> {
    let first = blocks
        .iter()
        .position(|(k, _)| *k == BlockKind::Series)
        .unwrap();
    let last = blocks
        .iter()
        .rposition(|(k, _)| *k == BlockKind::Series)
        .unwrap();
    assert!(last > first + 1, "puts between the chunks: {blocks:x?}");
    for (k, f) in &blocks[first..last] {
        assert!(
            f & ww_tape::FLAG_WITH_NEXT != 0,
            "{k:?} {f:#x} inside the unit is marked: {blocks:x?}"
        );
    }
    let after: Vec<_> = blocks[last + 1..]
        .iter()
        .filter(|(k, _)| *k == BlockKind::Records)
        .copied()
        .collect();
    assert!(
        after.iter().all(|(_, f)| f & ww_tape::FLAG_WITH_NEXT == 0),
        "no run after the unit: {blocks:x?}"
    );
    after
}

#[test]
fn the_definitions_of_a_track_without_data_go_inside_an_open_unit() {
    let blocks = relay(false, |w, defs| {
        assert!(w.put_definitions(defs, &[TrackNum::new(EMPTY)]).unwrap());
    });
    assert!(whole(&blocks).is_empty(), "no puts after the unit");
}

#[test]
fn a_definition_written_twice_inside_an_open_unit_has_its_copy_after_it() {
    let blocks = relay(true, |w, defs| {
        assert!(w.put_definitions(defs, &[TrackNum::new(EMPTY)]).unwrap());
    });
    let twice = blocks
        .iter()
        .filter(|(_, f)| f & ww_tape::FLAG_TWICE != 0)
        .count();
    assert!(twice > 0, "a block marked twice in the unit: {blocks:x?}");
    let after = whole(&blocks);
    assert!(
        !after.is_empty() && after.iter().all(|(_, f)| f & ww_tape::FLAG_TWICE != 0),
        "the copies follow the unit: {after:x?}"
    );
}

#[test]
fn a_put_and_a_typed_item_go_inside_an_open_unit() {
    let blocks = relay(false, |w, defs| {
        assert!(w.put(defs, TrackNum::new(STATE), 0, b"on").unwrap());
        assert!(w.put_item(defs, &item(b"run")).unwrap());
    });
    assert!(whole(&blocks).is_empty(), "no puts after the unit");
}

#[test]
fn a_group_inside_an_open_unit_is_refused_with_nothing_written() {
    let (defs, units) = relayed_units(false, 1);
    let mut w = crate::resume::begin(0);
    w.write_block(&defs, &units[0][0]).unwrap();
    let before = w.bytes();
    let puts = [GroupPut {
        track: TrackNum::new(STATE),
        ticks: 0,
        item: b"on",
    }];
    let r = w.put_group(&defs, &puts);
    assert!(
        matches!(
            r,
            Err(WriteError::Input(InputError::Group(GroupRefusal::UnitOpen)))
        ),
        "{r:?}"
    );
    assert_eq!(w.bytes(), before, "nothing written");
    assert!(w.unit_open());
    w.write_block(&defs, &units[0][1]).unwrap();
    assert_eq!(w.put_group(&defs, &puts).unwrap(), vec![true]);
}

/// A mux cutting every `chunks` chunks, with [CLOCK], the grid track and [EMPTY] defined; a relay handle.
fn relay_mux(
    mem: &ww_tape::MemPieces,
    chunks: u64,
) -> (
    ww_tape_mux::TapeMux<ww_tape::MemPieces>,
    ww_tape_mux::RelayHandle<ww_tape::MemPieces>,
) {
    let mux = ww_tape_mux::TapeMux::create(
        mem.clone(),
        crate::follow::OWNER,
        0,
        &[],
        crate::follow::options(chunks),
        ww_tape_mux::MuxOptions::default(),
    )
    .unwrap();
    let ww_tape::Describe::Clock(c) = crate::definitions::clock(CLOCK) else {
        unreachable!("clock() gives a clock")
    };
    mux.define_clock(c.spec()).unwrap();
    let axes = crate::grids::axes(&crate::grids::EXTENTS);
    let grid = mux
        .define_track(crate::grids::grid_track(&axes).spec())
        .unwrap();
    assert_eq!(grid, TrackNum::new(1), "the grid track's own number");
    let ww_tape::Describe::Track(empty) = empty_track(false) else {
        unreachable!("empty_track() gives a track")
    };
    assert_eq!(
        mux.define_track(empty.spec()).unwrap(),
        TrackNum::new(EMPTY)
    );
    let relay = mux.relay(ww_tape_mux::RelayMode::Relay);
    (mux, relay)
}

#[test]
fn the_mux_puts_definitions_inside_a_relayed_unit_without_splitting_it() {
    let (_, units) = relayed_units(false, 1);
    let mem = ww_tape::MemPieces::new();
    let (mux, relay) = relay_mux(&mem, 0);
    relay.send(units[0][0].clone()).unwrap();
    mux.put_definitions(&[TrackNum::new(EMPTY)]).unwrap();
    relay.send(units[0][1].clone()).unwrap();
    drop(relay);
    mux.close().unwrap();
    let p = crate::mux_multi::pieces(&mem);
    assert_eq!(p.len(), 1);
    assert!(whole(&kinds(&p[0])).is_empty(), "no puts after the unit");
    assert_eq!(
        crate::mux_definitions::def_records(&p[0], TrackNum::new(EMPTY)),
        1
    );
}

/// A cut by the policy before a relayed unit's first chunk: the definitions the mux carries into the new piece go
/// inside the unit.
#[test]
fn definitions_carried_after_a_cut_into_a_relayed_unit_go_inside_it() {
    let (_, units) = relayed_units(false, 3);
    let mem = ww_tape::MemPieces::new();
    // a cut once a piece holds 3 chunks: before the third unit, never inside the second
    let (mux, relay) = relay_mux(&mem, 3);
    relay.send(units[0][0].clone()).unwrap();
    relay.send(units[0][1].clone()).unwrap();
    mux.put_definitions(&[TrackNum::new(EMPTY)]).unwrap();
    for u in &units[1..] {
        relay.send(u[0].clone()).unwrap();
        relay.send(u[1].clone()).unwrap();
    }
    drop(relay);
    mux.close().unwrap();
    let p = crate::mux_multi::pieces(&mem);
    assert_eq!(p.len(), 2, "a cut before the third unit");
    let first = kinds(&p[0]);
    assert!(
        first
            .windows(2)
            .any(|w| w[0].0 == BlockKind::Series && w[1].0 == BlockKind::Series),
        "the first unit whole, the put after it: {first:x?}"
    );
    assert!(whole(&kinds(&p[1])).is_empty(), "the carry inside the unit");
    for piece in &p {
        assert_eq!(
            crate::mux_definitions::def_records(piece, TrackNum::new(EMPTY)),
            1
        );
    }
}
