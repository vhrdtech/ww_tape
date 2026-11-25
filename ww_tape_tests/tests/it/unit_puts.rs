//! Puts while a relayed unit is open (FMT-15): a put, a typed item and the definitions of a track without data go
//! into the unit's run, marked, and never split it; a definition written twice has its copy after the unit; a group,
//! whose last block is its own, is refused with nothing written.

use ww_tape::sink::MemSink;
use ww_tape::{Block, BlockKind, Definitions, GroupPut, TapeWriter, TrackNum};

use crate::definitions::{CLOCK, hold, state, track};
use crate::follow::walk;
use crate::grids::{STEPS, head, samples_column, unit_of, unit_writer};
use crate::mux_items::item;

/// A track without data, beside the grid track.
pub(crate) const EMPTY: u32 = 2;
/// A state track.
const STATE: u32 = 3;
/// Another state track.
const OTHER: u32 = 4;

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
    hold(&mut defs, &[empty_track(twice), state(STATE), state(OTHER)]).unwrap();
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

/// The item piece `bytes` holds in force on state track `track` at its end.
fn in_force(bytes: &[u8], track: u32) -> Option<Vec<u8>> {
    let r = ww_tape::TapeReader::open(bytes).unwrap();
    let slot = ww_tape::Slot::State {
        track: TrackNum::new(track),
    };
    r.state_at(0, slot, u64::MAX).unwrap().item.map(|i| i.item)
}

/// A group put on `track`: `item` at tick 0.
fn group_of(track: u32, item: &'static [u8]) -> [GroupPut<'static>; 1] {
    [GroupPut {
        track: TrackNum::new(track),
        ticks: 0,
        item: ww_tape::GroupItem::State(item),
    }]
}

#[test]
fn a_group_inside_an_open_unit_waits_whole_for_its_end() {
    let (defs, units) = relayed_units(false, 1);
    let mut w = crate::resume::begin(0);
    let tape = w.sink().tape();
    w.write_block(&defs, &units[0][0]).unwrap();
    let before = w.bytes();
    assert_eq!(
        w.put_group(&defs, &group_of(STATE, b"on")).unwrap(),
        vec![false],
        "held, not yet written"
    );
    assert_eq!(w.bytes(), before, "nothing written inside the unit");
    assert!(w.unit_open());
    w.write_block(&defs, &units[0][1]).unwrap();
    let bytes = tape.to_vec().unwrap();
    let blocks = kinds(&bytes);
    let last = blocks
        .iter()
        .rposition(|(k, _)| *k == BlockKind::Series)
        .unwrap();
    assert_eq!(
        blocks[last - 1].0,
        BlockKind::Series,
        "the unit whole: {blocks:x?}"
    );
    assert_eq!(
        crate::put_definitions::groups(&blocks[last..]),
        1,
        "the group after it"
    );
    assert_eq!(in_force(&bytes, STATE).as_deref(), Some(&b"on"[..]));
}

/// The blocks of puts of another producer's two groups, on [STATE] and [OTHER], as a relay receives them.
fn producer_groups(defs: &Definitions) -> Vec<Vec<u8>> {
    let mut w = crate::resume::begin(0);
    let tape = w.sink().tape();
    w.put_group(defs, &group_of(STATE, b"one")).unwrap();
    w.put_group(defs, &group_of(OTHER, b"two")).unwrap();
    drop(w);
    walk(&tape.to_vec().unwrap())
        .into_iter()
        .map(|(_, b)| b)
        .filter(|b| Block::read(b).unwrap().kind == BlockKind::Records)
        .collect()
}

#[test]
fn a_relayed_group_ending_inside_another_producers_unit_waits_for_its_end() {
    let (mut defs, units) = relayed_units(false, 1);
    let groups = producer_groups(&defs);
    let mut w = crate::resume::begin(0);
    let tape = w.sink().tape();
    w.relay_block(&mut defs, &units[0][0]).unwrap();
    // the first group's blocks, its last among them, while the unit is open
    let first_end = groups
        .iter()
        .position(|b| {
            let f = Block::read(b).unwrap().flags;
            f & ww_tape::FLAG_GROUP != 0 && f & ww_tape::FLAG_WITH_NEXT == 0
        })
        .unwrap();
    for b in &groups[..=first_end] {
        w.relay_block(&mut defs, b).unwrap();
    }
    assert!(w.unit_open(), "the group waits: the unit stays open");
    w.relay_block(&mut defs, &units[0][1]).unwrap();
    for b in &groups[first_end + 1..] {
        w.relay_block(&mut defs, b).unwrap();
    }
    let bytes = tape.to_vec().unwrap();
    let blocks = kinds(&bytes);
    let chunks: Vec<usize> = (0..blocks.len())
        .filter(|i| blocks[*i].0 == BlockKind::Series)
        .collect();
    assert_eq!(chunks.len(), 2);
    assert!(
        blocks[chunks[0]..chunks[1]]
            .iter()
            .all(|(_, f)| f & ww_tape::FLAG_WITH_NEXT != 0),
        "the unit whole: {blocks:x?}"
    );
    assert_eq!(
        crate::put_definitions::groups(&blocks),
        2,
        "both groups whole"
    );
    assert_eq!(in_force(&bytes, STATE).as_deref(), Some(&b"one"[..]));
    assert_eq!(in_force(&bytes, OTHER).as_deref(), Some(&b"two"[..]));
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
    for n in [STATE, OTHER] {
        let ww_tape::Describe::Track(t) = state(n) else {
            unreachable!("state() gives a track")
        };
        assert_eq!(mux.define_track(t.spec()).unwrap(), TrackNum::new(n));
    }
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

/// The unit's chunks lie together in `blocks`, and every group is whole.
fn unit_and_groups(blocks: &[(BlockKind, u16)]) -> usize {
    let chunks: Vec<usize> = (0..blocks.len())
        .filter(|i| blocks[*i].0 == BlockKind::Series)
        .collect();
    assert_eq!(chunks.len(), 2);
    assert!(
        blocks[chunks[0]..chunks[1]]
            .iter()
            .all(|(_, f)| f & ww_tape::FLAG_WITH_NEXT != 0),
        "the unit whole: {blocks:x?}"
    );
    crate::put_definitions::groups(blocks)
}

#[test]
fn the_mux_holds_a_group_put_during_a_relayed_unit_until_its_end() {
    let (_, units) = relayed_units(false, 1);
    let mem = ww_tape::MemPieces::new();
    let (mux, relay) = relay_mux(&mem, 0);
    let group = mux.group(&[TrackNum::new(STATE)]).unwrap();
    relay.send(units[0][0].clone()).unwrap();
    relay.flush().unwrap();
    group.put_group(&group_of(STATE, b"on")).unwrap();
    group.flush().unwrap();
    relay.send(units[0][1].clone()).unwrap();
    relay.flush().unwrap();
    assert_eq!(mux.stats().refused, 0, "nothing refused");
    drop((relay, group));
    mux.close().unwrap();
    let p = crate::mux_multi::pieces(&mem);
    assert_eq!(p.len(), 1);
    assert_eq!(unit_and_groups(&kinds(&p[0])), 1);
    assert_eq!(in_force(&p[0], STATE).as_deref(), Some(&b"on"[..]));
}

#[test]
fn the_mux_holds_a_relayed_group_ending_during_another_producers_unit() {
    let (defs, units) = relayed_units(false, 1);
    let groups = producer_groups(&defs);
    let mem = ww_tape::MemPieces::new();
    let (mux, relay) = relay_mux(&mem, 0);
    let other = mux.relay(ww_tape_mux::RelayMode::Relay);
    relay.send(units[0][0].clone()).unwrap();
    relay.flush().unwrap();
    for b in &groups {
        other.send(b.clone()).unwrap();
    }
    other.flush().unwrap();
    relay.send(units[0][1].clone()).unwrap();
    relay.flush().unwrap();
    assert_eq!(mux.stats().refused, 0, "nothing refused");
    drop((relay, other));
    mux.close().unwrap();
    let p = crate::mux_multi::pieces(&mem);
    assert_eq!(p.len(), 1);
    assert_eq!(unit_and_groups(&kinds(&p[0])), 2);
    assert_eq!(in_force(&p[0], STATE).as_deref(), Some(&b"one"[..]));
    assert_eq!(in_force(&p[0], OTHER).as_deref(), Some(&b"two"[..]));
}

/// A put after a group, both while the unit is open: the put waits behind the group, so its item is in force after.
#[test]
fn a_put_behind_a_waiting_group_keeps_its_order() {
    let (defs, units) = relayed_units(false, 1);
    let mut w = crate::resume::begin(0);
    let tape = w.sink().tape();
    w.write_block(&defs, &units[0][0]).unwrap();
    w.put_group(&defs, &group_of(STATE, b"old")).unwrap();
    assert!(!w.put(&defs, TrackNum::new(STATE), 1, b"new").unwrap());
    assert!(!w.put_item(&defs, &item(b"run")).unwrap());
    assert_eq!(
        w.puts_waiting(),
        3,
        "the group, then the put and the item behind it"
    );
    w.write_block(&defs, &units[0][1]).unwrap();
    assert_eq!(w.puts_waiting(), 0);
    let bytes = tape.to_vec().unwrap();
    assert_eq!(unit_and_groups(&kinds(&bytes)), 1);
    assert_eq!(in_force(&bytes, STATE).as_deref(), Some(&b"new"[..]));
}

/// No series chunk lies in `bytes` by a scan, nor by the index of the piece they hold.
fn no_chunk(bytes: &[u8]) {
    let blocks = kinds(bytes);
    assert!(
        blocks.iter().all(|(k, _)| *k != BlockKind::Series),
        "the torn unit dropped by a scan: {blocks:x?}"
    );
    let r = ww_tape::TapeReader::open(bytes).unwrap();
    assert!(
        r.entries(0)
            .unwrap()
            .iter()
            .all(|e| e.kind != BlockKind::Series),
        "the torn unit in no index entry"
    );
}

/// A producer dies inside its unit while a group and a put wait for the unit's end: the close tears the unit as a
/// crash would and writes them after its place, in order.
#[test]
fn a_close_inside_an_open_unit_tears_it_and_writes_what_waits() {
    let (defs, units) = relayed_units(false, 1);
    let mut w = crate::resume::begin(0);
    let tape = w.sink().tape();
    w.write_block(&defs, &units[0][0]).unwrap();
    w.put_group(&defs, &group_of(STATE, b"old")).unwrap();
    w.put(&defs, TrackNum::new(OTHER), 1, b"later").unwrap();
    w.close().unwrap();
    let bytes = tape.to_vec().unwrap();
    no_chunk(&bytes);
    assert_eq!(crate::put_definitions::groups(&kinds(&bytes)), 1);
    assert_eq!(in_force(&bytes, STATE).as_deref(), Some(&b"old"[..]));
    assert_eq!(in_force(&bytes, OTHER).as_deref(), Some(&b"later"[..]));
}

/// Through the mux on files: a relay's producer dies mid-unit with a group waiting; the mux closes, and a scan,
/// the index and a follower each see the group and the put after it, and none of the torn unit.
#[test]
fn the_mux_closes_after_a_relayed_unit_that_never_ends() {
    let (_, units) = relayed_units(false, 1);
    let dir = std::env::temp_dir().join(format!("ww_tape_unit_puts_torn_{}", std::process::id()));
    _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mux = ww_tape_fs::mux::create(
        &dir,
        crate::follow::OWNER,
        0,
        &[],
        crate::follow::options(0),
        ww_tape_mux::MuxOptions::default(),
    )
    .unwrap();
    let ww_tape::Describe::Clock(c) = crate::definitions::clock(CLOCK) else {
        unreachable!("clock() gives a clock")
    };
    mux.define_clock(c.spec()).unwrap();
    let axes = crate::grids::axes(&crate::grids::EXTENTS);
    mux.define_track(crate::grids::grid_track(&axes).spec())
        .unwrap();
    for d in [empty_track(false), state(STATE), state(OTHER)] {
        let ww_tape::Describe::Track(t) = d else {
            unreachable!("a track")
        };
        mux.define_track(t.spec()).unwrap();
    }
    let relay = mux.relay(ww_tape_mux::RelayMode::Relay);
    let group = mux.group(&[TrackNum::new(STATE)]).unwrap();
    let other = mux.state(TrackNum::new(OTHER)).unwrap();
    relay.send(units[0][0].clone()).unwrap();
    relay.flush().unwrap();
    group.put_group(&group_of(STATE, b"old")).unwrap();
    other.put(1, b"later").unwrap();
    other.flush().unwrap();
    // the producer dies: its unit's last chunk never comes
    drop((relay, group, other));
    mux.close().unwrap();
    let bytes = std::fs::read(dir.join(ww_tape_fs::chain::piece_name(0))).unwrap();
    no_chunk(&bytes);
    assert_eq!(crate::put_definitions::groups(&kinds(&bytes)), 1);
    assert_eq!(in_force(&bytes, STATE).as_deref(), Some(&b"old"[..]));
    assert_eq!(in_force(&bytes, OTHER).as_deref(), Some(&b"later"[..]));
    let mut f =
        ww_tape_fs::follow::ChainFollower::new(&dir, ww_tape_fs::follow::FollowOptions::default())
            .unwrap();
    let mut followed = Vec::new();
    while let Some(b) = f.try_next().unwrap() {
        followed.push(b.block.block().kind);
    }
    assert!(f.is_closed(), "the follower reads to the close");
    assert!(
        !followed.contains(&BlockKind::Series),
        "the follower hands out none of the torn unit: {followed:?}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Through the mux: a state put behind a group put while a relayed unit is open is in force after the unit.
#[test]
fn the_mux_keeps_a_put_behind_a_waiting_group() {
    let (_, units) = relayed_units(false, 1);
    let mem = ww_tape::MemPieces::new();
    let (mux, relay) = relay_mux(&mem, 0);
    let group = mux.group(&[TrackNum::new(STATE)]).unwrap();
    let state = mux.state(TrackNum::new(STATE)).unwrap();
    relay.send(units[0][0].clone()).unwrap();
    relay.flush().unwrap();
    group.put_group(&group_of(STATE, b"old")).unwrap();
    state.put(1, b"new").unwrap();
    state.flush().unwrap();
    relay.send(units[0][1].clone()).unwrap();
    relay.flush().unwrap();
    assert_eq!(mux.stats().refused, 0, "nothing refused");
    drop((relay, group, state));
    mux.close().unwrap();
    let p = crate::mux_multi::pieces(&mem);
    assert_eq!(p.len(), 1);
    assert_eq!(unit_and_groups(&kinds(&p[0])), 1);
    assert_eq!(in_force(&p[0], STATE).as_deref(), Some(&b"new"[..]));
}
