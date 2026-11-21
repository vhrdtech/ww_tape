//! File order is call order: what a writer writes for a call (a definition, a put, a deferred block) lands in the
//! file in the order of the calls, whatever waits for an aligned chunk's gap.

use shrink_wrap::prelude::*;
use ww_tape::TrackNum;
use ww_tape::{ClockNum, RecordTrack};

use crate::definitions::{CLOCK, Rig, base, clock, on_tracks, record_blocks, track};
use ww_tape::{
    ALIGNMENT, DescribeOwned, Layout, MemSink, Slot, SyncPolicy, TapeReader, TapeWriter,
};

/// An aligned writer on a memory sink with [base] and clock 2 held.
fn aligned() -> Rig {
    let mut w = Rig::new(TapeWriter::create_on(MemSink::new(), SyncPolicy::None));
    w.set_alignment(ALIGNMENT).unwrap();
    let mut defs = base(Layout::ALIGNED);
    defs.push(clock(2));
    w.define_all(&defs).unwrap();
    w
}

/// The clock of track `number`'s definition in force at `offset` of `bytes`.
fn clock_at(bytes: &[u8], number: u32, offset: u64) -> u32 {
    let r = TapeReader::open(bytes).unwrap();
    let found = r
        .state_at(
            0,
            Slot::Track {
                number: TrackNum::new(number),
            },
            offset,
        )
        .unwrap()
        .item
        .unwrap();
    match DescribeOwned::from_ww_bytes_owned(&found.item).unwrap() {
        DescribeOwned::Track(t) => t.clock.get(),
        other => panic!("not a track: {other:?}"),
    }
}

/// A deferred block of track 5 waits for a chunk's gap; a later change of track 5 to another clock in the
/// definitions doesn't reach it: it is placed with the definitions it was made under, so the reader takes it under
/// those (reading side review 2, point 2; writing side point 3).
#[test]
fn a_deferred_block_keeps_the_definitions_it_was_made_under() {
    let mut w = aligned();
    let tape = w.sink().tape();
    w.defer_block(on_tracks(w.next_id(), &[(5, 1, b"deferred")]))
        .unwrap();
    w.define(&track(5, 2)).unwrap();
    w.close_piece().unwrap();
    let bytes = tape.to_vec().unwrap();
    let data = record_blocks(&bytes)
        .into_iter()
        .find(|b| b.first_track == Some(5) && !b.repeat)
        .unwrap();
    assert_eq!(clock_at(&bytes, 5, data.offset), CLOCK);
}

/// A producer's block of puts given to defer_block is put as write_block puts it: not counted, its records under the
/// writer's own near rule (writing side review 2, point 4).
#[test]
fn a_deferred_block_of_puts_is_put() {
    let mut w = aligned();
    w.define(&crate::definitions::state(6)).unwrap();
    let tape = w.sink().tape();
    let producer = ww_tape::BlockId::new(ww_tape::SessionTag::random(), 9);
    let mut out = Vec::new();
    ww_tape::open::write_records(
        &[ww_tape::RecordRef {
            track: RecordTrack::from_u32(6),
            at: Some(ww_tape::ClockTime::new(ClockNum::new(CLOCK), 3)),
            item: b"A",
        }],
        4096,
        ww_tape::FLAG_REPEAT,
        || producer,
        |b| {
            out.push(b.to_vec());
            Ok::<(), ww_tape::DescribeError>(())
        },
    )
    .unwrap();
    let seq = w.next_seq();
    w.defer_block(out[0].clone()).unwrap();
    assert_eq!(w.next_seq(), seq, "not counted");
    w.defer_block(out[0].clone()).unwrap();
    w.close_piece().unwrap();
    let bytes = tape.to_vec().unwrap();
    // the record goes in one block with the track's definition before it
    let state: Vec<_> = record_blocks(&bytes)
        .into_iter()
        .filter(|b| {
            let block = ww_tape::Block::read(&b.bytes).unwrap();
            block
                .records()
                .any(|r| r.unwrap().track == RecordTrack::from_u32(6))
        })
        .collect();
    assert_eq!(state.len(), 1, "put once");
    let b = ww_tape::Block::read(&state[0].bytes).unwrap();
    assert!(b.is_repeat());
    assert_ne!(b.session, producer.session);
}
