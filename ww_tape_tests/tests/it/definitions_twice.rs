//! Definitions written twice at once ([TrackDef::defined_twice]): in a piece too small for the copies spaced apart,
//! every definition such a track needs reads after any one damaged block, and a crash at any block boundary resumes
//! to the bytes written without it.

use ww_tape::index::IndexPolicy;
use ww_tape::writer::NearPolicy;
use ww_tape::{
    Block, ClockNum, Definitions, Describe, FLAG_TWICE, FLAG_WITH_NEXT, FileBlocks, InputError,
    Layout, MemSink, Recovery, Slot, SyncPolicy, TapeReader, TapeSink, TapeWriter, TrackDef,
    TrackNum, UnitChunk, WriteError, WriterOptions,
};

use crate::definitions::{CLOCK, clock, held, on_tracks, record_blocks, track, writes_of};
use crate::pieces::{chunk, column};
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

/// Where each of `slots` reads from in `bytes`; None where it doesn't read.
fn read_at(bytes: &[u8], slots: &[Slot]) -> Vec<Option<u64>> {
    let r = TapeReader::open(bytes).unwrap();
    let at = r.definitions_at(0, u64::MAX).unwrap();
    slots
        .iter()
        .map(|s| at.items.get(s).map(|i| i.offset))
        .collect()
}

/// Where each of [NEEDED] reads from in `bytes`.
fn needed_at(bytes: &[u8]) -> Vec<Option<u64>> {
    read_at(bytes, &NEEDED)
}

/// The blocks of puts in `bytes`, by offset.
fn puts(bytes: &[u8]) -> Vec<u64> {
    record_blocks(bytes)
        .into_iter()
        .filter(|b| b.repeat)
        .map(|b| b.offset)
        .collect()
}

/// Whether every one of `slots` reads from `bytes` with any one of its blocks of puts damaged.
fn survives(bytes: &[u8], slots: &[Slot]) -> bool {
    puts(bytes).into_iter().all(|at| {
        read_at(&damaged(bytes, at), slots)
            .iter()
            .all(Option::is_some)
    })
}

/// [survives] for [NEEDED].
fn survives_any_one(bytes: &[u8]) -> bool {
    survives(bytes, &NEEDED)
}

/// The header flags of the record block at `offset` of `bytes`.
fn flags_at(bytes: &[u8], offset: u64) -> u16 {
    let b = record_blocks(bytes)
        .into_iter()
        .find(|b| b.offset == offset)
        .unwrap();
    Block::read(&b.bytes).unwrap().flags
}

/// Each definition the track needs is written twice at once: its first record in a block marked [FLAG_TWICE], its
/// copy after the run that block belongs to, marked the same but never [FLAG_WITH_NEXT]. The track beside it on the
/// same clock has its own definition once.
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
    let (first, copy) = (flags_at(&bytes, track[0]), flags_at(&bytes, track[1]));
    assert_ne!(first & FLAG_TWICE, 0);
    assert_ne!(
        first & FLAG_WITH_NEXT,
        0,
        "the first record serves the data after it"
    );
    assert_eq!(
        copy & (FLAG_TWICE | FLAG_WITH_NEXT),
        FLAG_TWICE,
        "the copy belongs with no block"
    );
    // the run in between: the first record's block, then the block of data it serves
    let blocks: Vec<u64> = record_blocks(&bytes).iter().map(|b| b.offset).collect();
    let i = blocks.iter().position(|o| *o == track[0]).unwrap();
    assert_eq!(
        blocks[i + 2],
        track[1],
        "the copy follows the run's data block"
    );
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
    let (first, copy) = (at[0], at[1]);
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

/// A state track written twice at once, beside [TWICE] and [ONCE].
const STATE: u32 = 12;

/// [batch], track 1 of the chunks and the state track [STATE], both written twice at once.
fn calls_batch() -> Vec<Describe<'static>> {
    let mut out = batch(true);
    for d in [track(1, CLOCK), crate::definitions::state(STATE)] {
        let Describe::Track(t) = d else {
            unreachable!("a track")
        };
        out.push(Describe::Track(TrackDef {
            defined_twice: true,
            ..t
        }));
    }
    out
}

/// A call of the producer, given again after a resume from where [TapeWriter::next_seq] says.
#[derive(Clone, Copy, Debug)]
enum Call {
    /// A record block on a track, at a tick
    Records(u32, i64),
    /// A put on [STATE] at a tick
    Put(i64),
    /// A unit of two chunks of track 1, from chunk k
    Unit(u64),
    /// A chunk of track `t + 1`, chunk k
    Chunk(u64, u64),
}

/// Make `call` on `w`.
fn make(w: &mut TapeWriter<MemSink>, defs: &Definitions, call: Call) {
    match call {
        Call::Records(on, k) => {
            let block = on_tracks(w.next_id(), &[(on, k, &[k as u8; 40])]);
            w.write_block(defs, &block).unwrap();
        }
        Call::Put(k) => {
            w.put(defs, TrackNum::new(STATE), k, &[k as u8; 8]).unwrap();
        }
        Call::Unit(k) => {
            let (h0, c0) = chunk(0, k);
            let (h1, c1) = chunk(0, k + 1);
            let (col0, col1) = (column(&c0), column(&c1));
            let unit = [
                UnitChunk {
                    head: &h0,
                    columns: &col0,
                    payload_hash: None,
                },
                UnitChunk {
                    head: &h1,
                    columns: &col1,
                    payload_hash: None,
                },
            ];
            w.write_chunk_unit(defs, &unit).unwrap();
        }
        Call::Chunk(t, k) => {
            let (h, c) = chunk(t, k);
            w.write_chunk(defs, &h, &column(&c)).unwrap();
        }
    }
}

/// Where a call was made: the seq it started at and the bytes of the piece after it.
#[derive(Clone, Copy, Debug)]
struct Made {
    seq: u64,
    end: u64,
}

/// Make `calls` on `w`; where each was made.
fn make_all(w: &mut TapeWriter<MemSink>, defs: &Definitions, calls: &[Call]) -> Vec<Made> {
    calls
        .iter()
        .map(|c| {
            let seq = w.next_seq();
            make(w, defs, *c);
            let end = w.sink().tape().held().unwrap();
            Made { seq, end }
        })
        .collect()
}

/// Whether a resume whose kept bytes end at `kept` and whose next seq is `next` is given `call`, made as `made`
/// before the crash: a call of data from the next seq on; a put whose bytes the crash cut, as a put of an item that
/// changed since is not given again.
fn given_again(call: Call, made: Made, next: u64, kept: u64) -> bool {
    match call {
        Call::Put(_) => made.seq >= next && made.end > kept,
        _ => made.seq >= next,
    }
}

/// Each block of `bytes`: where it starts, its kind, flags and seq, and the slots of its records.
fn blocks_of(bytes: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut rd = FileBlocks::with_recovery(bytes, Recovery::Prefix);
    while let Some(Ok(b)) = rd.next_block() {
        let slots: Vec<Option<Slot>> = match b.kind {
            ww_tape::BlockKind::Records => Block::read(b.bytes())
                .unwrap()
                .records()
                .filter_map(Result::ok)
                .map(|r| Slot::of(r.track, r.item))
                .collect(),
            _ => Vec::new(),
        };
        let line = format!("{:?} {:#x} seq {} {slots:?}", b.kind, b.flags, b.seq);
        out.push(format!("{} {line}", rd.offset()));
    }
    out
}

/// Where each block of `bytes` ends, past `from`.
fn block_ends(bytes: &[u8], from: u64) -> Vec<u64> {
    let mut out = Vec::new();
    let mut rd = FileBlocks::with_recovery(bytes, Recovery::Prefix);
    while let Some(Ok(b)) = rd.next_block() {
        let len = b.bytes().len() as u64;
        let end = rd.offset() + len;
        if end > from {
            out.push(end);
        }
    }
    out
}

/// `whole`, a piece written by `calls` (each made as `made` says) and closed, cut at every block boundary
/// past `from` as a crash leaves it, resumed, given again the calls from [TapeWriter::next_seq] on and closed: the
/// bytes of `whole` each time.
fn same_bytes_after_any_crash(
    whole: &[u8],
    from: u64,
    defs: &Definitions,
    calls: &[Call],
    made: &[Made],
) {
    let ends = block_ends(whole, from);
    assert!(ends.len() > 8, "{ends:?}");
    for end in ends {
        let mut sink = MemSink::new();
        sink.write_all(&whole[..end as usize]).unwrap();
        let (mut w, resumed) = TapeWriter::resume_on(sink, options()).unwrap();
        let tape = w.sink().tape();
        let next = w.next_seq();
        for (call, m) in calls.iter().zip(made) {
            if given_again(*call, *m, next, resumed.kept_bytes) {
                make(&mut w, defs, *call);
            }
        }
        w.close().unwrap();
        let again = tape.to_vec().unwrap();
        assert_eq!(blocks_of(&again), blocks_of(whole), "crash at {end}");
        assert!(again == whole, "crash at {end} of {}", whole.len());
    }
}

/// The calls of the tests: records on both tracks, puts, a unit and chunks.
const CALLS: [Call; 10] = [
    Call::Records(ONCE, 0),
    Call::Records(TWICE, 1),
    Call::Put(2),
    Call::Put(3),
    Call::Unit(0),
    Call::Records(TWICE, 4),
    Call::Put(5),
    Call::Chunk(0, 2),
    Call::Records(ONCE, 6),
    Call::Put(7),
];

/// A first piece: a crash at any block boundary and a resume give the bytes of the piece written without the crash,
/// the copies due at the crash written by the resume. Every definition written twice reads after any one damaged
/// block, a copy after a unit or a put among them.
#[test]
fn a_crash_at_any_block_boundary_gives_the_same_bytes() {
    let defs = held(&calls_batch());
    let mut w = begin();
    let tape = w.sink().tape();
    let made = make_all(&mut w, &defs, &CALLS);
    w.close().unwrap();
    let whole = tape.to_vec().unwrap();
    let mut slots = NEEDED.to_vec();
    for t in [1, STATE] {
        slots.push(Slot::Track {
            number: TrackNum::new(t),
        });
    }
    assert!(survives(&whole, &slots));
    for slot in &slots {
        for at in writes_of(&whole, *slot).into_iter().skip(1).take(1) {
            assert_eq!(
                flags_at(&whole, at) & FLAG_WITH_NEXT,
                0,
                "{slot:?}: a copy is in no run"
            );
        }
    }
    same_bytes_after_any_crash(&whole, 0, &defs, &CALLS, &made);
}

/// A new piece carrying tracks written twice: their carried definitions have copies after the run the piece head's
/// puts start. A crash at any block boundary of the new piece and a resume give its bytes; every carried definition
/// reads after any one damaged block of puts, a copy among them.
#[test]
fn a_piece_carrying_tracks_written_twice_resumes_to_the_same_bytes() {
    let defs = held(&calls_batch());
    let mut w = begin();
    // no state in the first piece: a writer resumed alone has no newest state to carry beside its definition
    let first: Vec<Call> = CALLS
        .into_iter()
        .filter(|c| !matches!(c, Call::Put(_)))
        .collect();
    make_all(&mut w, &defs, &first);
    let (_, mut next) = w.cut_on(&defs, MemSink::new()).unwrap();
    let tape = next.sink().tape();
    let calls = [
        Call::Records(ONCE, 20),
        Call::Put(21),
        Call::Chunk(0, 9),
        Call::Records(TWICE, 22),
    ];
    let made = make_all(&mut next, &defs, &calls);
    next.close().unwrap();
    let whole = tape.to_vec().unwrap();
    let carried: Vec<Slot> = [TWICE, 1]
        .into_iter()
        .map(|t| Slot::Track {
            number: TrackNum::new(t),
        })
        .chain([Slot::Clock {
            number: ClockNum::new(CLOCK),
        }])
        .collect();
    assert!(survives(&whole, &carried));
    same_bytes_after_any_crash(&whole, 0, &defs, &calls, &made);
}

/// A put is admitted by the state entries its definitions take with their copies: at a policy of 6, the layout,
/// the producer, the clock and [STATE] (each twice) and the item need 7, so the put is refused with
/// [InputError::CarriedEntries] before anything is written.
#[test]
fn a_put_is_admitted_by_the_entries_its_copies_take() {
    let defs = held(&calls_batch());
    let mut w = begin();
    w.set_index_policy(IndexPolicy {
        state_entries_max: exact_units::Count::new(6),
        ..IndexPolicy::default()
    })
    .unwrap();
    let held = w.sink().tape().held().unwrap();
    let r = w.put(&defs, TrackNum::new(STATE), 0, &[0; 8]);
    assert!(
        matches!(
            r,
            Err(WriteError::Input(InputError::CarriedEntries {
                needed: 7,
                max: 6
            }))
        ),
        "{r:?}"
    );
    assert_eq!(w.sink().tape().held().unwrap(), held, "nothing written");
}
