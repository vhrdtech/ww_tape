//! Definitions written twice at once ([TrackDef::defined_twice]): in a piece too small for the copies spaced apart,
//! every definition such a track needs reads after any one damaged block, a crash anywhere included.

use ww_tape::index::IndexPolicy;
use ww_tape::writer::NearPolicy;
use ww_tape::{
    Block, ClockNum, Definitions, Describe, FLAG_WITH_NEXT, FileBlocks, Layout, MemSink, Recovery,
    Slot, SyncPolicy, TapeReader, TapeSink, TapeWriter, TrackDef, TrackNum, WriterOptions,
};

use crate::definitions::{CLOCK, clock, held, on_tracks, record_blocks, track, writes_of};
use crate::resume::head;

/// The track that asks for its definitions twice, and one beside it on the same clock that doesn't.
const TWICE: u32 = 9;
const ONCE: u32 = 10;

/// Bytes the test pieces stay under.
const PIECE_MAX: usize = 64 << 10;

/// The slots track [TWICE] needs: its clock and its own definition.
const NEEDED: [Slot; 2] = [
    Slot::Clock {
        number: ClockNum::new(CLOCK),
    },
    Slot::Track {
        number: TrackNum::new(TWICE),
    },
];

/// The clock and tracks [TWICE] (written twice at once when `twice`) and [ONCE].
fn batch(twice: bool) -> Vec<Describe<'static>> {
    let Describe::Track(t) = track(TWICE, CLOCK) else {
        unreachable!("track() gives a track")
    };
    vec![
        clock(CLOCK),
        track(ONCE, CLOCK),
        Describe::Track(TrackDef {
            defined_twice: twice,
            ..t
        }),
    ]
}

/// The default policies: track 0 repeated as [StatePolicy::DEFAULT](ww_tape::StatePolicy), copies 1 MiB apart, so
/// a piece under 1 MiB holds no copy spaced apart.
fn options() -> WriterOptions {
    WriterOptions {
        sync: SyncPolicy::None,
        clock: ww_tape::ClockRef::NONE,
        index: IndexPolicy::default(),
        layout: Layout::UNALIGNED,
        producer: "definitions twice".to_owned(),
        near: NearPolicy::default(),
        integrity: ww_tape::Integrity::Full,
    }
}

/// A first piece under [options], begun but given no call yet.
fn begin() -> TapeWriter<MemSink> {
    let o = options();
    let mut w = TapeWriter::create_on(MemSink::new(), o.sync);
    w.set_layout(o.layout).unwrap();
    w.set_producer(&o.producer);
    w.set_index_policy(o.index).unwrap();
    w.set_near_policy(o.near);
    w.begin_piece(&head(), &[]).unwrap();
    w
}

/// Write record blocks on [ONCE] then [TWICE], in turn, from block `from` on while the piece is under `until` bytes:
/// the clock is written first for [ONCE] alone.
fn write_from(w: &mut TapeWriter<MemSink>, defs: &Definitions, from: i64, until: u64) {
    let mut k = from;
    while w.sink().tape().held().unwrap() < until {
        let on = if k % 2 == 0 { ONCE } else { TWICE };
        let block = on_tracks(w.next_id(), &[(on, k, &[k as u8; 40])]);
        w.write_block(defs, &block).unwrap();
        k += 1;
    }
}

/// A closed piece of just under 64 KiB.
fn piece(twice: bool) -> Vec<u8> {
    let defs = held(&batch(twice));
    let mut w = begin();
    let tape = w.sink().tape();
    write_from(&mut w, &defs, 0, PIECE_MAX as u64 / 3);
    w.close().unwrap();
    let bytes = tape.to_vec().unwrap();
    assert!(bytes.len() <= PIECE_MAX, "{} bytes", bytes.len());
    bytes
}

/// The record block at `offset` of `bytes` damaged: a byte in its middle flipped.
fn damaged(bytes: &[u8], offset: u64) -> Vec<u8> {
    let b = record_blocks(bytes)
        .into_iter()
        .find(|b| b.offset == offset)
        .unwrap();
    let mut out = bytes.to_vec();
    out[offset as usize + b.bytes.len() / 2] ^= 0x5a;
    out
}

/// Where each of [NEEDED] reads from in `bytes`; None where it doesn't read.
fn needed_at(bytes: &[u8]) -> Vec<Option<u64>> {
    let r = TapeReader::open(bytes).unwrap();
    let at = r.definitions_at(0, u64::MAX).unwrap();
    NEEDED
        .iter()
        .map(|s| at.items.get(s).map(|i| i.offset))
        .collect()
}

/// The blocks of puts in `bytes`, by offset.
fn puts(bytes: &[u8]) -> Vec<u64> {
    record_blocks(bytes)
        .into_iter()
        .filter(|b| b.repeat)
        .map(|b| b.offset)
        .collect()
}

/// Whether every one of [NEEDED] reads from `bytes` with any one of its blocks of puts damaged.
fn survives_any_one(bytes: &[u8]) -> bool {
    puts(bytes)
        .into_iter()
        .all(|at| needed_at(&damaged(bytes, at)).iter().all(Option::is_some))
}

/// Each definition the track needs is written twice at once, in two blocks one after the other; the copy belongs with
/// no other block. The track beside it on the same clock has its own definition once.
#[test]
fn the_definitions_are_written_in_two_blocks() {
    let bytes = piece(true);
    for slot in NEEDED {
        let at = writes_of(&bytes, slot);
        assert!(at.len() >= 2, "{slot:?}: {at:?}");
    }
    let track = writes_of(
        &bytes,
        Slot::Track {
            number: TrackNum::new(TWICE),
        },
    );
    assert_eq!(track.len(), 2, "{track:?}");
    let copy = record_blocks(&bytes)
        .into_iter()
        .find(|b| b.offset == track[0])
        .unwrap();
    let flags = Block::read(&copy.bytes).unwrap().flags;
    assert_eq!(flags & FLAG_WITH_NEXT, 0, "the copy belongs with no block");
    let once = writes_of(
        &bytes,
        Slot::Track {
            number: TrackNum::new(ONCE),
        },
    );
    assert_eq!(once.len(), 1, "{once:?}");
    assert!(
        TapeReader::open(&bytes[..])
            .unwrap()
            .verify(0)
            .unwrap()
            .holds
    );
}

/// With the copy damaged the first record reads, with the first record damaged the copy reads.
#[test]
fn either_record_reads_when_the_other_is_damaged() {
    let bytes = piece(true);
    let slot = Slot::Track {
        number: TrackNum::new(TWICE),
    };
    let at = writes_of(&bytes, slot);
    let (copy, first) = (at[0], at[1]);
    assert_eq!(needed_at(&damaged(&bytes, copy))[1], Some(first));
    assert_eq!(needed_at(&damaged(&bytes, first))[1], Some(copy));
}

/// In a 64 KiB piece every definition of a track that asked for it reads after any one damaged block; without the
/// request one damaged block loses the track.
#[test]
fn every_definition_reads_after_one_damaged_block() {
    assert!(survives_any_one(&piece(true)));
    assert!(!survives_any_one(&piece(false)));
}

/// A crash at any block boundary, then a resume that writes on: the piece still holds each definition twice.
#[test]
fn a_crash_anywhere_keeps_the_definitions_twice() {
    let whole = piece(true);
    let mut ends = Vec::new();
    let mut rd = FileBlocks::with_recovery(&whole[..], Recovery::Prefix);
    while let Some(Ok(b)) = rd.next_block() {
        let len = b.bytes().len() as u64;
        let end = rd.offset() + len;
        if end > 2048 {
            break;
        }
        ends.push(end);
    }
    assert!(ends.len() > 6, "{ends:?}");
    for end in ends {
        let mut sink = MemSink::new();
        sink.write_all(&whole[..end as usize]).unwrap();
        let (mut w, _) = TapeWriter::resume_on(sink, options()).unwrap();
        let tape = w.sink().tape();
        let defs = held(&batch(true));
        write_from(&mut w, &defs, 1000, 8192);
        w.close().unwrap();
        let bytes = tape.to_vec().unwrap();
        assert!(survives_any_one(&bytes), "crash at {end}");
    }
}
