//! Typed items through the mux (MUX-4): put on track 0 behind the units sent before, and put again in each piece a
//! later cut starts as their carry says: every piece, the pieces carrying a track of their owner, or none; until
//! released.

use shrink_wrap::prelude::*;
use ww_tape::{
    DefinitionKind, Describe, FullVersion, ItemHash, MemPieces, OwnerId, Slot, TapeReader, TypeRef,
    TypedItem, Version,
};
use ww_tape_mux::{ItemCarry, MuxOptions, TapeMux};

use crate::follow::{OWNER, define, events_chunk, options, send};
use crate::mux_multi::pieces;

const RUN: OwnerId = OwnerId([0xa1; 32]);
const GONE: OwnerId = OwnerId([0xb2; 32]);

/// A typed item of value `v`.
fn item(v: &'static [u8]) -> TypedItem<'static> {
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
