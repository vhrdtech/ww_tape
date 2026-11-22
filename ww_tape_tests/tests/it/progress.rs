//! The mux's progress watch: a reader in the writer's process waits on it instead of polling (READ-6).

use std::time::Duration;

use ww_tape::{ItemNum, MemTapes, OwnerId, TrackNum};
use ww_tape_mux::{MuxOptions, TapeMux};

use crate::follow::{define, options, send, walk};

const OWNER: OwnerId = OwnerId([0x6e; 32]);

/// The mux's progress watch wakes a reader in the writer's process when chunks are placed, says what is placed and
/// synced, and reports the end.
#[test]
fn the_progress_watch_wakes_a_reader_in_the_writers_process() {
    let mem = MemTapes::new();
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
    let watch = mux.watch();
    let reader = std::thread::spawn(move || {
        let mut p = watch.get();
        while p.tracks.get(&TrackNum::new(1)).map(|t| t.next_item) != Some(ItemNum::new(200)) {
            p = watch.wait_past(p.version);
        }
        p
    });
    for k in 0..4 {
        send(&senders[0], 0, k);
    }
    let p = reader.join().unwrap();
    assert_eq!(p.tracks[&TrackNum::new(1)].last_tick, 1990);
    assert!(!p.ended);
    let bytes = mem.get(&0).unwrap().unwrap().to_vec().unwrap();
    assert!(bytes.len() as u64 >= p.piece_bytes);
    let placed = walk(&bytes[..p.piece_bytes as usize])
        .iter()
        .filter(|(_, b)| ww_tape::Block::read(b).unwrap().chunk().is_some())
        .count();
    assert_eq!(
        placed, 4,
        "every chunk the watch counts lies in the bytes it names"
    );

    mux.sync().unwrap();
    let watch = mux.watch();
    let p = watch.get();
    assert_eq!(p.synced_bytes, p.piece_bytes);
    drop(senders);
    mux.close().unwrap();
    let p = watch.wait_past(p.version);
    assert!(p.ended);
    assert!(watch.wait_past_for(p.version, Duration::ZERO).is_some());
}

/// A sync, a flush and a cut return only after the watch shows what they did: a reader in the writer's process
/// that asks the watch right after one sees the synced bytes, the placed chunk and the new piece.
#[test]
fn the_watch_shows_a_sync_flush_and_cut_before_they_return() {
    let mem = MemTapes::new();
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
    let watch = mux.watch();
    for k in 0..400 {
        send(&senders[0], 0, k);
        mux.sync().unwrap();
        let p = watch.get();
        assert_eq!(
            p.tracks[&TrackNum::new(1)].next_item,
            ItemNum::new((k + 1) * 50),
            "round {k}: the chunk sent before the sync is placed"
        );
        assert_eq!(
            p.synced_bytes, p.piece_bytes,
            "round {k}: the sync's bytes are published before it returns"
        );
        if k % 50 == 49 {
            mux.cut().unwrap();
            assert_eq!(
                watch.get().piece,
                (k + 1) / 50,
                "round {k}: the cut's piece is published before it returns"
            );
        }
    }
    drop(senders);
    mux.close().unwrap();
}
