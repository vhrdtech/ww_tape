//! Typed items through the mux (MUX-4): put on track 0 behind the units sent before, and put again in each piece a
//! later cut starts as their carry says: every piece, the pieces carrying a track of their owner, or none; until
//! released.

use shrink_wrap::prelude::*;
use ww_tape::{
    Block, DefinitionKind, Describe, FullVersion, ItemHash, MemPieces, OwnerId, RecordTrack, Slot,
    TapeReader, TypeRef, TypedItem, Version,
};
use ww_tape_mux::{
    CallError, HELD_ITEM_BYTES_MAX, HELD_ITEMS_MAX, ItemCarry, MuxError, MuxOptions, TapeMux,
};

use crate::follow::{OWNER, define, events_chunk, options, send};
use crate::mux_multi::pieces;

const RUN: OwnerId = OwnerId([0xa1; 32]);
const GONE: OwnerId = OwnerId([0xb2; 32]);

/// A typed item of value `v`.
pub(crate) fn item(v: &'static [u8]) -> TypedItem<'static> {
    TypedItem {
        ty: TypeRef {
            bundle: RefVec::Slice { slice: &[] },
            krate: FullVersion::new("tests", Version::new(0, 1, 0)),
            path: "RunDescribe",
        },
        value: RefVec::Slice { slice: v },
    }
}

/// Whether piece `p` holds the typed item of hash `h` on track 0.
fn holds(p: &[u8], h: ItemHash) -> bool {
    let r = TapeReader::open(p).unwrap();
    let defs = r.definitions_at(0, u64::MAX).unwrap();
    defs.items.contains_key(&Slot::Own {
        kind: DefinitionKind::Item,
        hash: h,
    })
}

/// How many records of piece `p`'s track 0 hold the item of hash `h`.
pub(crate) fn copies(p: &[u8], h: ItemHash) -> usize {
    let mut n = 0;
    let mut at = 0;
    while let Some(rest) = p.get(at..) {
        let Ok(len) = ww_tape::block_len(rest) else {
            break;
        };
        let b = Block::read(rest).unwrap();
        let mut buf = Vec::new();
        let b = b.decode(&mut buf).unwrap();
        n += b
            .records()
            .filter_map(Result::ok)
            .filter(|r| r.track == RecordTrack::Definitions && ItemHash::of(r.item) == h)
            .count();
        at += len;
    }
    n
}

/// A mux on `mem` whose pieces list at most `room` state entries, cutting on no count of chunks.
fn tight(mem: &MemPieces, room: u64) -> TapeMux<MemPieces> {
    let mut o = options(0);
    o.writer.index.state_entries_max = exact_units::Count::new(room);
    TapeMux::create(mem.clone(), OWNER, 0, &[], o, MuxOptions::default()).unwrap()
}

#[test]
fn a_cut_between_an_item_and_its_owners_next_chunk_carries_the_item() {
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
    let senders = define(&mux);
    // track 2 again, of the run's owner: its own number
    let Describe::Track(t) = crate::definitions::track(2, crate::definitions::CLOCK) else {
        unreachable!("track() gives a track")
    };
    let mut spec = t.spec();
    spec.owner = RUN.0;
    let run_track = mux.define_track(spec).unwrap();
    let run = mux.chunks(run_track).unwrap();
    let run_chunk = |k| {
        let (mut head, columns) = events_chunk(1, k);
        head.track_number = run_track;
        run.send(head, columns).unwrap();
    };
    run_chunk(0);
    send(&senders[0], 0, 0);

    let owned = mux.put_item(&item(b"run"), ItemCarry::Owner(RUN)).unwrap();
    let every = mux.put_item(&item(b"every"), ItemCarry::Every).unwrap();
    let once = mux.put_item(&item(b"once"), ItemCarry::Once).unwrap();
    let gone = mux
        .put_item(&item(b"gone"), ItemCarry::Owner(GONE))
        .unwrap();
    mux.cut().unwrap();
    run_chunk(1);
    send(&senders[0], 0, 1);

    assert!(mux.release_item(every).unwrap(), "held until released");
    assert!(
        !mux.release_item(once).unwrap(),
        "an item put once is not held"
    );
    mux.cut().unwrap();
    send(&senders[0], 0, 2);
    drop((senders, run));
    mux.close().unwrap();

    let p = pieces(&mem);
    assert_eq!(p.len(), 3);
    for h in [owned, every, once, gone] {
        assert!(holds(&p[0], h), "every item is in the piece it is put in");
    }
    assert!(
        holds(&p[1], owned),
        "the next piece carries the run's track, so its item"
    );
    assert!(holds(&p[1], every), "an item carried into every piece");
    assert!(!holds(&p[1], once), "an item put once stays in its piece");
    assert!(!holds(&p[1], gone), "no track of that owner is carried");
    assert!(holds(&p[2], owned), "the run's track is still carried");
    assert!(!holds(&p[2], every), "a released item is in no later piece");
}

#[test]
fn an_owners_track_first_written_after_a_cut_brings_its_item() {
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
    let senders = define(&mux);
    let Describe::Track(t) = crate::definitions::track(2, crate::definitions::CLOCK) else {
        unreachable!("track() gives a track")
    };
    let mut spec = t.spec();
    spec.owner = RUN.0;
    let run_track = mux.define_track(spec).unwrap();
    let run = mux.chunks(run_track).unwrap();
    send(&senders[0], 0, 0);
    let owned = mux.put_item(&item(b"run"), ItemCarry::Owner(RUN)).unwrap();
    // the run's track has no data yet: the next piece's head carries no track of the run
    mux.cut().unwrap();
    send(&senders[0], 0, 1);
    let (mut head, columns) = events_chunk(1, 0);
    head.track_number = run_track;
    run.send(head, columns).unwrap();
    mux.cut().unwrap();
    send(&senders[0], 0, 2);
    drop((senders, run));
    mux.close().unwrap();

    let p = pieces(&mem);
    assert_eq!(p.len(), 3);
    assert!(holds(&p[0], owned), "the piece it is put in");
    assert!(
        holds(&p[1], owned),
        "the piece where the run's track is first written"
    );
    assert!(
        holds(&p[2], owned),
        "the piece whose head carries the run's track"
    );
}

#[test]
fn a_put_that_cuts_carries_the_held_items_into_its_piece() {
    let mem = MemPieces::new();
    // the piece head, the clock and tracks, and five items
    let mux = tight(&mem, 9);
    let senders = define(&mux);
    send(&senders[0], 0, 0);
    let names: [&'static [u8]; 4] = [b"i0", b"i1", b"i2", b"i3"];
    let held: Vec<ItemHash> = names
        .iter()
        .map(|n| mux.put_item(&item(n), ItemCarry::Every).unwrap())
        .collect();
    let first = mux.put_item(&item(b"o1"), ItemCarry::Once).unwrap();
    // no room for a sixth: the put cuts before it
    let second = mux.put_item(&item(b"o2"), ItemCarry::Once).unwrap();
    drop(senders);
    mux.close().unwrap();

    let p = pieces(&mem);
    assert_eq!(p.len(), 2, "one cut, by the put");
    assert_eq!(copies(&p[0], first), 1);
    assert_eq!(copies(&p[0], second), 0, "the put cut before its item");
    assert_eq!(copies(&p[1], second), 1);
    assert_eq!(
        copies(&p[1], first),
        0,
        "an item put once stays in its piece"
    );
    for h in held {
        assert_eq!(copies(&p[0], h), 1);
        assert_eq!(
            copies(&p[1], h),
            1,
            "carried into the piece the put started"
        );
    }
}

#[test]
fn a_held_item_a_piece_has_no_room_for_waits_for_the_next() {
    let mem = MemPieces::new();
    // room for three of the four items after the piece head, the clock and tracks
    let mux = tight(&mem, 7);
    let senders = define(&mux);
    send(&senders[0], 0, 0);
    let names: [&'static [u8]; 4] = [b"i0", b"i1", b"i2", b"i3"];
    let held: Vec<ItemHash> = names
        .iter()
        .map(|n| mux.put_item(&item(n), ItemCarry::Every).unwrap())
        .collect();
    mux.cut().unwrap();
    send(&senders[0], 0, 1);
    mux.flush().unwrap();
    let refused = mux.stats().refused;
    drop(senders);
    mux.close().unwrap();

    let p = pieces(&mem);
    // i3 cut to piece 1, which carries i0 and i1 after it and has no room for i2; the cut carries i0, i1 and i2
    assert_eq!(
        p.len(),
        3,
        "a carry never cuts: no piece more than the puts and the cut start"
    );
    assert_eq!(
        refused, 2,
        "i2 in piece 1 and i3 in piece 2, each refused once"
    );
    let in_piece: Vec<Vec<usize>> = p
        .iter()
        .map(|p| held.iter().map(|h| copies(p, *h)).collect())
        .collect();
    assert_eq!(in_piece, [[1, 1, 1, 0], [1, 1, 0, 1], [1, 1, 1, 0]]);
}

/// A typed item of value `v`, owned.
fn owned_item(v: Vec<u8>) -> TypedItem<'static> {
    TypedItem {
        value: RefVec::Slice {
            slice: Vec::leak(v),
        },
        ..item(b"")
    }
}

#[test]
fn held_items_past_their_caps_are_refused_before_a_write() {
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
    let senders = define(&mux);
    send(&senders[0], 0, 0);
    let held: Vec<ItemHash> = (0..HELD_ITEMS_MAX)
        .map(|i| {
            let v = u32::try_from(i).unwrap().to_le_bytes().to_vec();
            mux.put_item(&owned_item(v), ItemCarry::Every).unwrap()
        })
        .collect();
    let one_more = owned_item(b"one more".to_vec());
    let r = mux.put_item(&one_more, ItemCarry::Every);
    assert!(
        matches!(r, Err(MuxError::Call(CallError::HeldItems { items, .. })) if items == HELD_ITEMS_MAX),
        "{r:?}"
    );
    let once = mux.put_item(&one_more, ItemCarry::Once).unwrap();
    // an item held already takes no more room
    mux.put_item(
        &owned_item(0_u32.to_le_bytes().to_vec()),
        ItemCarry::Owner(RUN),
    )
    .unwrap();
    for h in &held {
        assert!(mux.release_item(*h).unwrap());
    }
    // by bytes: items of 60,000 bytes until one is refused
    let size = 60_000;
    let big = |i: usize| {
        owned_item(
            [
                u32::try_from(i).unwrap().to_le_bytes().to_vec(),
                vec![7; size],
            ]
            .concat(),
        )
    };
    let mut n = 0;
    let r = loop {
        match mux.put_item(&big(n), ItemCarry::Every) {
            Ok(_) => n += 1,
            Err(e) => break e,
        }
    };
    assert!(
        matches!(r, MuxError::Call(CallError::HeldItems { items, .. }) if items == n),
        "{r:?}"
    );
    assert!(n * size <= HELD_ITEM_BYTES_MAX, "{n} items held");
    assert!(
        (n + 1) * (size + 64) > HELD_ITEM_BYTES_MAX,
        "refused at the cap: {n} items held"
    );
    let refused = ww_tape::open::encode(&Describe::Item(big(n))).unwrap();
    drop(senders);
    mux.close().unwrap();
    let p = pieces(&mem);
    assert_eq!(p.len(), 1);
    assert_eq!(copies(&p[0], once), 1, "an item put once is never held");
    assert_eq!(
        copies(&p[0], ItemHash::of(&refused)),
        0,
        "a refused item is not written"
    );
}

/// A clock the test moves.
#[derive(Clone, Default)]
struct TestClock(std::sync::Arc<std::sync::atomic::AtomicU64>);

impl ww_tape::Clock for TestClock {
    fn now_ms(&self) -> u64 {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[test]
fn cuts_by_count_and_by_age_carry_each_held_item_once() {
    let mem = MemPieces::new();
    let clock = TestClock::default();
    let mut o = options(2);
    o.cut.interval = Some(std::time::Duration::from_secs(10));
    o.writer.clock = ww_tape::ClockRef::new(clock.clone());
    let mux = TapeMux::create(
        mem.clone(),
        OWNER,
        0,
        &[],
        o,
        MuxOptions {
            tick: std::time::Duration::from_millis(1),
            ..MuxOptions::default()
        },
    )
    .unwrap();
    let senders = define(&mux);
    let every = mux.put_item(&item(b"every"), ItemCarry::Every).unwrap();
    let own = mux
        .put_item(&item(b"own"), ItemCarry::Owner(OWNER))
        .unwrap();
    // two chunks a piece: pieces 0 and 1 by count
    for k in 0..3 {
        send(&senders[0], 0, k);
    }
    mux.flush().unwrap();
    // piece 1 holds a chunk: once it is older than the interval, a tick cuts it
    clock
        .0
        .fetch_add(11_000, std::sync::atomic::Ordering::SeqCst);
    crate::mux::until(|| mem.get(&2).unwrap().is_some());
    send(&senders[0], 0, 3);
    drop(senders);
    mux.close().unwrap();

    let p = pieces(&mem);
    assert_eq!(p.len(), 3, "a cut by count, then one by age");
    for (i, p) in p.iter().enumerate() {
        assert_eq!(copies(p, every), 1, "piece {i}");
        assert_eq!(copies(p, own), 1, "piece {i}: the chain's own tracks");
    }
}
