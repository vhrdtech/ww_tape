//! Definitions put through the mux without data (MUX-5): a track that never gets an item is in the piece it is put
//! in and in each piece a later cut starts, once each; a track defined twice gets its copy; a crashed mux resumed
//! and asked again holds it once in every piece, also after its next cut.

use ww_tape::{Describe, ItemHash, MemPieces, Slot, TapeReader, TrackNum};
use ww_tape_mux::{ChunkSender, MuxOptions, TapeMux};

use crate::definitions::{CLOCK, track, track_id};
use crate::follow::{OWNER, define, events_chunk, options, send};
use crate::mux_items::copies;
use crate::mux_multi::{pieces, plant};
use crate::resume::cut_points;

/// The number [track] `n` gets after [define]'s nine tracks: its own.
const EMPTY: u32 = 10;
/// A track defined twice at once ([ww_tape::TrackSpec::defined_twice]).
const TWICE: u32 = 11;

/// Define track `n` of [track] on `mux`, written twice at once when `twice`.
fn define_empty(mux: &TapeMux<MemPieces>, n: u32, twice: bool) -> TrackNum {
    let Describe::Track(t) = track(n, CLOCK) else {
        unreachable!("track() gives a track")
    };
    let mut spec = t.spec();
    spec.defined_twice = twice;
    let number = mux.define_track(spec).unwrap();
    assert_eq!(number, TrackNum::new(n), "the number after the base tracks");
    number
}

/// The hash of track `n`'s definition as piece `p` holds it; None when the piece doesn't define it.
pub(crate) fn track_def(p: &[u8], n: TrackNum) -> Option<ItemHash> {
    let r = TapeReader::open(p).unwrap();
    let defs = r.definitions_at(0, u64::MAX).unwrap();
    defs.items
        .get(&Slot::Track { number: n })
        .map(|s| ItemHash::of(&s.item))
}

/// How many records of piece `p` hold track `n`'s definition: 0 when it defines none.
pub(crate) fn def_records(p: &[u8], n: TrackNum) -> usize {
    track_def(p, n).map_or(0, |h| copies(p, h))
}

/// Chunk `k` of track [EMPTY] once it gets data.
fn send_empty(s: &ChunkSender<MemPieces>, k: u64) {
    let (mut head, columns) = events_chunk(0, k);
    head.track_number = TrackNum::new(EMPTY);
    head.track_id = track_id(EMPTY);
    s.send(head, columns).unwrap();
}

#[test]
fn an_empty_tracks_definitions_are_in_every_piece_once() {
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
    // --8<-- [start:put_definitions]
    let empty = define_empty(&mux, EMPTY, false);
    let twice = define_empty(&mux, TWICE, true);
    send(&senders[0], 0, 0);
    mux.put_definitions(&[empty, twice]).unwrap();
    send(&senders[0], 0, 1);
    mux.put_definitions(&[empty]).unwrap();
    // --8<-- [end:put_definitions]
    mux.cut().unwrap();
    send(&senders[0], 0, 2);
    mux.cut().unwrap();
    send(&senders[0], 0, 3);
    drop(senders);
    mux.close().unwrap();

    let p = pieces(&mem);
    assert_eq!(p.len(), 3);
    for (i, piece) in p.iter().enumerate() {
        assert_eq!(
            def_records(piece, empty),
            1,
            "piece {i} holds the empty track's definition once"
        );
        assert_eq!(
            def_records(piece, twice),
            2,
            "piece {i} holds the definition written twice and its copy"
        );
    }
}

#[test]
fn a_later_first_item_does_not_write_the_definition_again() {
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
    let empty = define_empty(&mux, EMPTY, false);
    send(&senders[0], 0, 0);
    mux.put_definitions(&[empty]).unwrap();
    send(&senders[0], 0, 1);
    let late = mux.chunks(empty).unwrap();
    send_empty(&late, 0);
    mux.cut().unwrap();
    send_empty(&late, 1);
    drop((senders, late));
    mux.close().unwrap();

    let p = pieces(&mem);
    assert_eq!(p.len(), 2);
    for (i, piece) in p.iter().enumerate() {
        assert_eq!(
            def_records(piece, empty),
            1,
            "piece {i}: the definition near behind serves the track's data"
        );
    }
}

#[test]
fn an_unknown_track_is_refused() {
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
    mux.flush().unwrap();
    let before = mux.watch().get().piece_bytes;
    let r = mux.put_definitions(&[TrackNum::new(1), TrackNum::new(EMPTY)]);
    assert!(
        matches!(
            r,
            Err(ww_tape_mux::MuxError::Refused(ww_tape::WriteError::Describe(
                ww_tape::DescribeError::UnknownTrack { track }
            ))) if track == TrackNum::new(EMPTY)
        ),
        "{r:?}"
    );
    assert_eq!(mux.watch().get().piece_bytes, before, "nothing written");
}

/// Write the run [a_crashed_mux_resumed_holds_the_definition_in_every_piece] crashes: data on track 1, the empty
/// track put, a cut, data again.
fn whole_run(mem: &MemPieces) {
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
    let empty = define_empty(&mux, EMPTY, false);
    send(&senders[0], 0, 0);
    mux.put_definitions(&[empty]).unwrap();
    send(&senders[0], 0, 1);
    mux.cut().unwrap();
    send(&senders[0], 0, 2);
    drop(senders);
    mux.close().unwrap();
}

#[test]
fn a_crashed_mux_resumed_holds_the_definition_in_every_piece() {
    let mem = MemPieces::new();
    whole_run(&mem);
    let want = pieces(&mem);
    assert_eq!(want.len(), 2);
    let empty = TrackNum::new(EMPTY);
    let mut crashes = 0;
    for (p, piece) in want.iter().enumerate() {
        let (done, _) = want.split_at(p);
        for at in cut_points(piece, 0) {
            if p + 1 == want.len() && at == piece.len() {
                continue;
            }
            let mem = plant(done, &piece[..at]);
            let (mux, _) =
                TapeMux::resume(mem.clone(), OWNER, 0, options(0), MuxOptions::default()).unwrap();
            let senders = define(&mux);
            define_empty(&mux, EMPTY, false);
            mux.put_definitions(&[empty]).unwrap();
            // past every chunk kept: a gap in track 1's items, no overlap
            mux.cut().unwrap();
            send(&senders[0], 0, 10);
            drop(senders);
            mux.close().unwrap();
            let got = pieces(&mem);
            assert_eq!(
                got.len(),
                p + 2,
                "the resumed piece and the one after its cut"
            );
            for (i, got) in got.iter().enumerate() {
                assert_eq!(
                    def_records(got, empty),
                    1,
                    "a crash in piece {p} at {at}: piece {i} holds the definition once"
                );
            }
            crashes += 1;
        }
    }
    assert!(crashes > 10, "{crashes} crashes");
}
