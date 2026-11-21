//! A new piece carries the definitions of the tracks its head carries, quiet ones too (WRITE-9).

use ww_tape::index::IndexPolicy;
use ww_tape::{
    CutPolicy, Definitions, InputError, MemSink, MemTapes, OwnerId, PieceChain, SessionTag, Slot,
    SyncPolicy, TapeReader, TapeSink, TapeWriter, WriteError,
};

use crate::definitions::{CLOCK, base_defs};
use crate::pieces::{chunk, column};

/// A chain of three pieces in memory: tracks 1 and 2 give data in the first, then only track 2. The bytes of each
/// piece, oldest first.
fn quiet_chain(defs: &Definitions) -> Vec<Vec<u8>> {
    let mem = MemTapes::new();
    let mut chain = PieceChain::create_on(
        mem.clone(),
        OwnerId([0x51; 32]),
        0,
        &[],
        ww_tape::ChainOptions {
            tapes: ww_tape::PieceTapes::Each,
            writer: ww_tape::WriterOptions {
                sync: SyncPolicy::None,
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .unwrap();
    for (t, k) in [(0, 0), (1, 0)] {
        let (h, c) = chunk(t, k);
        chain.write_chunk(defs, &h, &column(&c)).unwrap();
    }
    for k in 1..3 {
        chain.cut_now(defs).unwrap();
        let (h, c) = chunk(1, k);
        chain.write_chunk(defs, &h, &column(&c)).unwrap();
    }
    chain
        .close()
        .unwrap()
        .iter()
        .map(|f| mem.get(&f.tape).unwrap().unwrap().to_vec().unwrap())
        .collect()
}

/// The definitions the first piece of `bytes` holds, by slot.
fn held(bytes: &[u8]) -> std::collections::BTreeMap<Slot, Vec<u8>> {
    let r = TapeReader::open(bytes).unwrap();
    let at = r.definitions_at(0, u64::MAX).unwrap();
    assert!(at.lost.is_empty() && !at.spent, "{at:?}");
    at.items.into_iter().map(|(s, i)| (s, i.item)).collect()
}

/// Each piece holds the definitions of every track its head carries, the quiet track 1 too, the same items `defs`
/// holds: dropping the pieces before it keeps track 1's kind, clock and label.
#[test]
fn a_new_piece_carries_its_quiet_tracks_definitions() {
    let defs = base_defs();
    let pieces = quiet_chain(defs);
    assert_eq!(pieces.len(), 3);
    for (i, p) in pieces.iter().enumerate() {
        let held = held(p);
        for slot in [
            Slot::Track { number: 1 },
            Slot::Track { number: 2 },
            Slot::Clock { number: CLOCK },
        ] {
            assert_eq!(
                held.get(&slot).map(Vec::as_slice),
                defs.get(&slot),
                "piece {i}: {slot:?}"
            );
        }
        // a track that gave no data in the chain has no head row and no definition
        assert!(!held.contains_key(&Slot::Track { number: 3 }), "piece {i}");
        let r = TapeReader::open(&p[..]).unwrap();
        assert!(r.verify(0).unwrap().holds, "piece {i}");
    }
    // the chain without its first piece reads as head cut and still knows track 1
    let rest = pieces[1..].concat();
    let r = TapeReader::open(&rest[..]).unwrap();
    assert!(!r.chain().is_whole());
    for i in 0..r.pieces().len() {
        let at = r.definitions_at(i, u64::MAX).unwrap();
        assert!(at.items.contains_key(&Slot::Track { number: 1 }), "{i}");
    }
}

/// The carried definitions lie right after the layout and the producer, before the first block of data: the chunk
/// of track 2 that follows puts nothing again.
#[test]
fn carried_definitions_follow_the_head_and_serve_the_data_after_them() {
    let defs = base_defs();
    let pieces = quiet_chain(defs);
    let blocks = crate::definitions::record_blocks(&pieces[2]);
    // layout and producer, then the carried definitions, all puts: the chunk follows them
    assert!(blocks.iter().all(|b| b.repeat), "only puts");
    let r = TapeReader::open(&pieces[2][..]).unwrap();
    let chunk_at = r.track_entries(0, crate::definitions::track_id(2)).unwrap()[0].offset;
    assert!(blocks.iter().all(|b| b.offset < chunk_at));
}

/// A chain written by the writer before WRITE-9 (`data/before-quiet-defs/`, the recipe of [quiet_chain] at main
/// 50bc1ad6): it reads and verifies as before, its later pieces with only the definitions of their data.
#[test]
fn a_chain_written_before_carried_definitions_reads() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/it/data/before-quiet-defs/quiet-track-chain.wwt");
    let bytes = std::fs::read(path).unwrap();
    let r = TapeReader::open(&bytes[..]).unwrap();
    assert_eq!(r.pieces().len(), 3);
    assert!(r.chain().is_whole(), "{:?}", r.chain());
    for i in 0..3 {
        assert!(r.verify(i).unwrap().holds, "piece {i}");
        let at = r.definitions_at(i, u64::MAX).unwrap();
        assert!(at.items.contains_key(&Slot::Track { number: 2 }), "{i}");
        assert_eq!(
            at.items.contains_key(&Slot::Track { number: 1 }),
            i == 0,
            "piece {i}"
        );
    }
    // the same recipe now differs only in the later pieces' definitions
    let now = quiet_chain(base_defs());
    assert!(bytes.starts_with(&now[0]), "the first piece is the same");
    assert_ne!(bytes, now.concat());
}

/// A piece cut by `w` after chunks of tracks 0, 1 and 2 (numbers 1 to 3), written with the index policy lowered to 7
/// state entries: past the layout and the producer, the next piece keeps 3 entries for the next data on track 3 (the
/// last written: its clock, its definition and a record), and has room for that track's clock and definition. Tracks
/// 1 and 2 are left out, their head rows marked; the cut's [Closed](ww_tape::writer::Closed) counts them.
fn lowered_cut() -> (ww_tape::writer::Closed, Vec<u8>) {
    let defs = base_defs();
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    w.set_session(SessionTag::writable(0x7175_6965)).unwrap();
    let owner = OwnerId([0x52; 32]);
    let head = ww_tape::PieceHead {
        ident: ww_tape::PieceIdent {
            id: ww_tape::piece::PieceId::first(&owner),
            owner,
            seq: 0,
        },
        prev: None,
        prev_session: SessionTag::NONE,
        integrity: ww_tape::Integrity::Full,
    };
    w.begin_piece(&head, &[]).unwrap();
    for t in 0..3 {
        let (h, c) = chunk(t, 0);
        w.write_chunk(defs, &h, &column(&c)).unwrap();
    }
    w.set_index_policy(IndexPolicy {
        state_entries_max: 7,
        ..IndexPolicy::default()
    })
    .unwrap();
    let (closed, next) = w.cut_on(defs, MemSink::new()).unwrap();
    let tape = next.sink().tape();
    next.close().unwrap();
    (closed, tape.to_vec().unwrap())
}

/// A cut is never refused for the definitions its next piece carries: with the index policy lowered so they don't
/// fit, it carries the clock and the tracks with the newest data, and marks the tracks left out in the next head.
/// A reader of that piece alone names them.
#[test]
fn a_cut_without_room_carries_the_newest_and_names_the_rest() {
    let (closed, bytes) = lowered_cut();
    assert_eq!(closed.not_carried(), 2);
    let left: Vec<u32> = closed
        .tracks
        .iter()
        .filter(|t| t.not_carried)
        .map(|t| t.track_number)
        .collect();
    assert_eq!(left, [1, 2]);
    let r = TapeReader::open(&bytes[..]).unwrap();
    assert!(r.verify(0).unwrap().holds);
    let at = r.definitions_at(0, u64::MAX).unwrap();
    assert!(at.items.contains_key(&Slot::Clock { number: CLOCK }));
    for number in 1..=2 {
        assert!(!at.items.contains_key(&Slot::Track { number }), "{number}");
    }
    assert!(at.items.contains_key(&Slot::Track { number: 3 }));
    let named: Vec<u32> = at.not_carried.iter().map(|t| t.track_number).collect();
    assert_eq!(named, [1, 2]);
    let head = &r.pieces()[0].head.as_ref().unwrap().1;
    assert_eq!(head.iter().filter(|t| t.not_carried).count(), 2);
}

/// The piece of [lowered_cut], two of its head rows marked not carried, pinned byte for byte.
#[test]
fn not_carried_golden() {
    let (_, bytes) = lowered_cut();
    let r = TapeReader::open(&bytes[..]).unwrap();
    let rows = &r.pieces()[0].head.as_ref().unwrap().1;
    let row = rows.iter().position(|t| t.not_carried).unwrap();
    // the head row's flags: bit 1, not carried
    assert_eq!(bytes[152 + row * 96 + 12..][..4], 2u32.to_le_bytes());
    golden(&bytes, "v1-not-carried.hex");
}

/// `bytes` as the golden file `name` holds them in hex, 64 bytes a line; `WW_TAPE_GOLDEN=update` rewrites it.
fn golden(bytes: &[u8], name: &str) {
    let mut hex = String::new();
    for line in bytes.chunks(64) {
        for byte in line {
            hex.push_str(&format!("{byte:02x}"));
        }
        hex.push('\n');
    }
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../ww_tape/golden")
        .join(name);
    if std::env::var("WW_TAPE_GOLDEN").as_deref() == Ok("update") {
        std::fs::write(&path, &hex).unwrap();
    }
    let want = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("golden/{name}: run with WW_TAPE_GOLDEN=update"));
    assert!(
        hex == want,
        "the format changed: bytes differ from golden/{name}"
    );
}

/// A chain whose index policy is lowered mid-piece cuts all the same and writes on.
#[test]
fn a_chain_with_its_policy_lowered_cuts_and_writes_on() {
    let defs = base_defs();
    let mut chain = PieceChain::create_on(
        MemTapes::new(),
        OwnerId([0x53; 32]),
        0,
        &[],
        ww_tape::ChainOptions::default(),
    )
    .unwrap();
    for t in 0..3 {
        let (h, c) = chunk(t, 0);
        chain.write_chunk(defs, &h, &column(&c)).unwrap();
    }
    chain
        .writer()
        .unwrap()
        .set_index_policy(IndexPolicy {
            state_entries_max: 7,
            ..IndexPolicy::default()
        })
        .unwrap();
    chain.cut_now(defs).unwrap();
    assert_eq!(chain.seq(), 1);
    let (h, c) = chunk(2, 1);
    chain.write_chunk(defs, &h, &column(&c)).unwrap();
    let pieces = chain.close().unwrap();
    assert_eq!(pieces.len(), 2);
    assert_eq!(pieces[0].piece.not_carried(), 2);
}

/// A crash right after a cut, before the next piece's first data: the resume keeps the piece head alone, and the
/// next call puts the layout, the producer and the carried definitions again: the piece's bytes are those of a cut
/// that never stopped.
#[test]
fn a_resume_after_a_cut_puts_the_carried_definitions_again() {
    let defs = base_defs();
    let options = ww_tape::WriterOptions {
        sync: SyncPolicy::None,
        ..Default::default()
    };
    let owner = OwnerId([0x54; 32]);
    let head = ww_tape::PieceHead {
        ident: ww_tape::PieceIdent {
            id: ww_tape::piece::PieceId::first(&owner),
            owner,
            seq: 0,
        },
        prev: None,
        prev_session: SessionTag::NONE,
        integrity: ww_tape::Integrity::Full,
    };
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    w.set_session(SessionTag::writable(0x7175_6966)).unwrap();
    w.begin_piece(&head, &[]).unwrap();
    for t in 0..3 {
        let (h, c) = chunk(t, 0);
        w.write_chunk(defs, &h, &column(&c)).unwrap();
    }
    let (_, mut next) = w.cut_on(defs, MemSink::new()).unwrap();
    let tape = next.sink().tape();
    let at_cut = tape.to_vec().unwrap();
    let (h, c) = chunk(1, 1);
    next.write_chunk(defs, &h, &column(&c)).unwrap();
    next.close().unwrap();
    let whole = tape.to_vec().unwrap();
    assert!(held(&whole).contains_key(&Slot::Track { number: 1 }));

    let mut sink = MemSink::new();
    sink.write_all(&at_cut).unwrap();
    let (mut again, resumed) = TapeWriter::resume_on(sink, options).unwrap();
    assert!(resumed.kept_bytes < u64::try_from(at_cut.len()).unwrap());
    let tape = again.sink().tape();
    again.write_chunk(defs, &h, &column(&c)).unwrap();
    again.close().unwrap();
    assert_eq!(tape.to_vec().unwrap(), whole);
}

/// A call whose definitions fit no piece under the index policy (here 3 entries: the layout, the producer, a clock
/// and a track need 4) is refused with [InputError::CarriedEntries], nothing written and no piece cut. A policy of 4
/// admits it.
#[test]
fn a_call_whose_definitions_fit_no_piece_is_refused_without_a_cut() {
    let defs = base_defs();
    for (max, admitted) in [(3, false), (4, true)] {
        let mut chain = PieceChain::create_on(
            MemTapes::new(),
            OwnerId([0x55; 32]),
            0,
            &[],
            chain_options(max, 0),
        )
        .unwrap();
        let bytes = chain.writer().unwrap().bytes();
        let (h, c) = chunk(0, 0);
        let r = chain.write_chunk(defs, &h, &column(&c));
        assert_eq!(r.is_ok(), admitted, "{max}: {r:?}");
        if !admitted {
            assert!(
                matches!(
                    r,
                    Err(WriteError::Input(InputError::CarriedEntries {
                        needed: 4,
                        max: 3
                    }))
                ),
                "{r:?}"
            );
            assert_eq!(chain.writer().unwrap().bytes(), bytes, "nothing written");
        }
        assert_eq!(chain.seq(), 0, "no cut");
    }
}

/// A new track in a full piece is checked for admission before the piece's room: one whose definitions fit no piece
/// (a clock, a map, a host clock and the track: 4, with the layout and the producer 6 at a policy of 5) is refused
/// with [InputError::CarriedEntries], and the chain doesn't cut for it. The piece stays, and writes on.
#[test]
fn a_refused_new_track_in_a_full_piece_cuts_no_piece() {
    let mut all = vec![
        keyed_clock(CLOCK),
        keyed_clock(9),
        keyed_clock(10),
        map(9, 10),
    ];
    all.extend((1..=2).map(|n| crate::definitions::track(n, CLOCK)));
    all.push(crate::definitions::track(3, 9));
    let mut defs = Definitions::new();
    defs.define_all(&all).unwrap();
    let mem = MemTapes::new();
    let mut chain = PieceChain::create_on(
        mem.clone(),
        OwnerId([0x56; 32]),
        0,
        &[],
        chain_options(5, 0),
    )
    .unwrap();
    // the layout, the producer, the clock and two tracks: the piece is full
    for t in 0..2 {
        let (h, c) = chunk(t, 0);
        chain.write_chunk(&defs, &h, &column(&c)).unwrap();
    }
    let (h, c) = chunk(2, 0);
    let r = chain.write_chunk(&defs, &h, &column(&c));
    assert!(
        matches!(
            r,
            Err(WriteError::Input(InputError::CarriedEntries {
                needed: 6,
                max: 5
            }))
        ),
        "{r:?}"
    );
    assert_eq!(chain.seq(), 0, "no cut for a refused call");
    let (h, c) = chunk(0, 1);
    chain.write_chunk(&defs, &h, &column(&c)).unwrap();
    let pieces = chain.close().unwrap();
    assert!(all_hold_data(&mem, &pieces));
}

/// A chain cutting by chunks admits a chunk before the cut its policy makes due: a chunk whose definitions fit no
/// piece is refused without a cut, as it is when no cut is due, and the piece goes on to hold the next chunk.
#[test]
fn a_refused_chunk_cuts_no_piece_when_a_cut_is_due() {
    let mut all = vec![
        keyed_clock(CLOCK),
        keyed_clock(9),
        keyed_clock(10),
        map(9, 10),
    ];
    all.extend((1..=2).map(|n| crate::definitions::track(n, CLOCK)));
    all.push(crate::definitions::track(3, 9));
    let mut defs = Definitions::new();
    defs.define_all(&all).unwrap();
    let mem = MemTapes::new();
    let mut chain = PieceChain::create_on(
        mem.clone(),
        OwnerId([0x57; 32]),
        0,
        &[],
        chain_options(5, 1),
    )
    .unwrap();
    let (h, c) = chunk(0, 0);
    chain.write_chunk(&defs, &h, &column(&c)).unwrap();
    assert!(chain.cut_due(), "one chunk: the policy's cut is due");
    let (h, c) = chunk(2, 0);
    let r = chain.write_chunk(&defs, &h, &column(&c));
    assert!(
        matches!(
            r,
            Err(WriteError::Input(InputError::CarriedEntries {
                needed: 6,
                max: 5
            }))
        ),
        "{r:?}"
    );
    assert_eq!(chain.seq(), 0, "no cut for a refused call, due or not");
    let (h, c) = chunk(1, 0);
    chain.write_chunk(&defs, &h, &column(&c)).unwrap();
    assert_eq!(chain.seq(), 1, "the due cut comes with the admitted chunk");
    let pieces = chain.close().unwrap();
    assert!(all_hold_data(&mem, &pieces));
}

/// Carried blocks are at most [CARRIED_BLOCK_LEN](ww_tape::CARRIED_BLOCK_LEN) and each holds the clock its tracks
/// count on: one damaged block costs only its own tracks, never another block's clock.
#[test]
fn a_damaged_carried_block_costs_only_its_own_tracks() {
    let mut defs = Definitions::new();
    let mut all = vec![crate::definitions::clock(CLOCK)];
    all.extend((1..=200).map(|n| crate::definitions::track(n, CLOCK)));
    defs.define_all(&all).unwrap();
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    w.set_session(SessionTag::writable(0x7175_6967)).unwrap();
    let owner = OwnerId([0x56; 32]);
    let head = ww_tape::PieceHead {
        ident: ww_tape::PieceIdent {
            id: ww_tape::piece::PieceId::first(&owner),
            owner,
            seq: 0,
        },
        prev: None,
        prev_session: SessionTag::NONE,
        integrity: ww_tape::Integrity::Full,
    };
    w.begin_piece(&head, &[]).unwrap();
    for t in 0..200 {
        let (h, c) = chunk(t, 0);
        w.write_chunk(&defs, &h, &column(&c)).unwrap();
    }
    let (_, mut next) = w.cut_on(&defs, MemSink::new()).unwrap();
    let tape = next.sink().tape();
    let (h, c) = chunk(0, 1);
    next.write_chunk(&defs, &h, &column(&c)).unwrap();
    next.close().unwrap();
    let bytes = tape.to_vec().unwrap();

    let carried: Vec<_> = crate::definitions::record_blocks(&bytes)
        .into_iter()
        .filter(|b| b.repeat && b.first_track == Some(0))
        .collect();
    // the layout and the producer, then the carried blocks: each but the first holds the clock
    let clock = Slot::Clock { number: CLOCK };
    let with_clock: Vec<_> = crate::definitions::puts(&bytes)
        .into_iter()
        .filter(|(_, slots)| slots.contains(&clock))
        .collect();
    assert!(with_clock.len() >= 4, "{} carried blocks", with_clock.len());
    for (_, slots) in &with_clock {
        assert_eq!(slots[0], clock);
    }
    for b in &carried {
        assert!(
            b.bytes.len() <= ww_tape::CARRIED_BLOCK_LEN,
            "{}",
            b.bytes.len()
        );
    }

    // damage the second carried block holding the clock
    let blocks: Vec<&Vec<u8>> = carried
        .iter()
        .map(|b| &b.bytes)
        .filter(|b| {
            crate::definitions::puts(b)
                .first()
                .is_some_and(|(_, s)| s.contains(&clock))
        })
        .collect();
    let second = blocks[1];
    let at = bytes
        .windows(second.len())
        .position(|w| w == &second[..])
        .unwrap();
    let mut damaged = bytes.clone();
    damaged[at + second.len() / 2] ^= 0xff;
    let r = TapeReader::open(&damaged[..]).unwrap();
    let defs_at = r.definitions_at(0, u64::MAX).unwrap();
    assert!(!defs_at.lost.is_empty());
    assert!(defs_at.items.contains_key(&clock));
    let tracks = defs_at
        .items
        .keys()
        .filter(|s| matches!(s, Slot::Track { .. }))
        .count();
    let lost = crate::definitions::puts(second)[0].1.len() - 1;
    assert_eq!(tracks, 200 - lost, "{lost} tracks in the damaged block");
}

/// Clock `number`, its key its own.
fn keyed_clock(number: u32) -> ww_tape::Describe<'static> {
    let ww_tape::Describe::Clock(c) = crate::definitions::clock(number) else {
        unreachable!("clock() gives a clock")
    };
    ww_tape::Describe::Clock(ww_tape::ClockDef {
        key: ww_tape::clock_key(0xad, &[], u64::from(number)),
        ..c
    })
}

/// A map from clock `from` to clock `to`, one to one.
fn map(from: u32, to: u32) -> ww_tape::Describe<'static> {
    use shrink_wrap::prelude::*;
    let seg: &'static [ww_tape::MapSegment] = Box::leak(
        vec![
            ww_tape::MapSegment::new(0, None, 0, ww_tape::Ratio64::new(1, 1).unwrap(), 0).unwrap(),
        ]
        .into_boxed_slice(),
    );
    ww_tape::Describe::Map(ww_tape::ClockMap {
        from: UNib32(from),
        to: UNib32(to),
        segments: RefVec::Slice { slice: seg },
    })
}

/// Chain options of state entries `max`, cut each `chunks` chunks (0: never).
fn chain_options(max: usize, chunks: u64) -> ww_tape::ChainOptions {
    ww_tape::ChainOptions {
        writer: ww_tape::WriterOptions {
            sync: SyncPolicy::None,
            index: IndexPolicy {
                state_entries_max: max,
                ..IndexPolicy::default()
            },
            ..Default::default()
        },
        cut: CutPolicy {
            chunks,
            ..CutPolicy::default()
        },
        ..Default::default()
    }
}

/// Write chunk 0 of tracks 0 to 199 into `chain`, which cuts by itself as pieces fill: 200 tracks counted in.
fn admit(chain: &mut PieceChain<MemTapes>, defs: &Definitions) -> u64 {
    for t in 0..200 {
        let (h, c) = chunk(t, 0);
        chain.write_chunk(defs, &h, &column(&c)).unwrap();
    }
    200
}

/// A clock map defined after the tracks were counted in makes each block's chain longer: the cut drops the per-block
/// chain, carries what fits, and the chain writes on, cutting by chunks or by hand; every piece holds data.
#[test]
fn a_map_defined_after_the_tracks_keeps_the_chain_writing() {
    let mut all = vec![keyed_clock(CLOCK), keyed_clock(2)];
    all.extend((1..=200).map(|n| crate::definitions::track(n, CLOCK)));
    for chunks in [0, 4] {
        let mut defs = Definitions::new();
        defs.define_all(&all).unwrap();
        let mem = MemTapes::new();
        let mut chain = PieceChain::create_on(
            mem.clone(),
            OwnerId([0x71; 32]),
            0,
            &[],
            chain_options(105, chunks),
        )
        .unwrap();
        admit(&mut chain, &defs);
        defs.define(&map(CLOCK, 2)).unwrap();
        if chunks == 0 {
            chain.cut_now(&defs).unwrap();
        }
        for k in 1..=6 {
            let (h, c) = chunk(0, k);
            chain.write_chunk(&defs, &h, &column(&c)).unwrap();
        }
        assert!(chain.seq() >= 1, "chunks {chunks}");
        let pieces = chain.close().unwrap();
        assert!(all_hold_data(&mem, &pieces));
    }
}

/// 300 tracks each on a clock of its own, carried in by the chain's first head, then a map from each clock to a host
/// clock, at 1000 state entries: the full layout (1204 entries) no longer fits, each definition once (905) does. The
/// cut carries every track and the chain writes on. At the default policy 300 000 tracks do the same.
#[test]
fn maps_added_to_tracks_on_their_own_clocks_keep_the_chain_writing() {
    let n = 300u32;
    let host = 9_000_000;
    let mut all = vec![keyed_clock(CLOCK), keyed_clock(host)];
    let mut rows = Vec::new();
    for k in 0..n {
        let (number, c) = (100 + k, 1000 + k);
        all.push(keyed_clock(c));
        let d = crate::definitions::track(number, c);
        let ww_tape::Describe::Track(t) = &d else {
            unreachable!("track() gives a track")
        };
        rows.push(ww_tape::piece::TrackStart {
            track_id: t.id(),
            track_number: number,
            next_item: 1,
            from_tick: 0,
            owner: OwnerId(t.owner),
            hash: ww_tape::TrackHash::START,
            incomplete: false,
            not_carried: false,
        });
        all.push(d);
    }
    all.push(crate::definitions::track(1, CLOCK));
    let mut defs = Definitions::new();
    defs.define_all(&all).unwrap();
    let mut chain = PieceChain::create_on(
        MemTapes::new(),
        OwnerId([0x73; 32]),
        0,
        &rows,
        chain_options(1000, 2),
    )
    .unwrap();
    for k in 0..2 {
        let (h, c) = chunk(0, k);
        chain.write_chunk(&defs, &h, &column(&c)).unwrap();
    }
    let maps: Vec<_> = (0..n).map(|k| map(1000 + k, host)).collect();
    defs.define_all(&maps).unwrap();
    for k in 2..5 {
        let (h, c) = chunk(0, k);
        chain.write_chunk(&defs, &h, &column(&c)).unwrap();
    }
    assert!(chain.seq() >= 1);
    let pieces = chain.close().unwrap();
    assert!(pieces.iter().all(|p| p.piece.not_carried() == 0));
}

/// Definitions that grow after their tracks were counted in (labels to 2000 bytes): one track per block no longer
/// fits; the cut carries what fits once each, and the chain writes on.
#[test]
fn definitions_grown_after_admission_keep_the_chain_writing() {
    let mut all = vec![keyed_clock(CLOCK)];
    all.extend((1..=200).map(|n| crate::definitions::track(n, CLOCK)));
    let mut defs = Definitions::new();
    defs.define_all(&all).unwrap();
    let mem = MemTapes::new();
    let mut chain = PieceChain::create_on(
        mem.clone(),
        OwnerId([0x75; 32]),
        0,
        &[],
        chain_options(105, 0),
    )
    .unwrap();
    let admitted = admit(&mut chain, &defs);
    let long: &'static str = Box::leak("y".repeat(2000).into_boxed_str());
    for n in 1..=u32::try_from(admitted).unwrap() {
        defs.define(&crate::definitions::labeled(n, CLOCK, long))
            .unwrap();
    }
    let before = chain.seq();
    chain.cut_now(&defs).unwrap();
    let (h, c) = chunk(0, 1);
    chain.write_chunk(&defs, &h, &column(&c)).unwrap();
    let pieces = chain.close().unwrap();
    assert_eq!(pieces.len(), usize::try_from(before).unwrap() + 2);
    assert!(all_hold_data(&mem, &pieces));
}

/// Tracks on clocks of their own, then maps to a host clock, at a policy where not even each definition once fits:
/// the cut carries the clocks and maps, then the tracks with the newest data, and leaves the rest out. The chain
/// writes on, and a reader of the later pieces alone names each track left out.
#[test]
fn a_cut_short_of_room_leaves_out_the_oldest_and_a_reader_names_them() {
    let n = 300u32;
    let host = 9_000;
    let mut all = vec![keyed_clock(host)];
    for k in 1..=n {
        all.push(keyed_clock(1000 + k));
        all.push(crate::definitions::track(k, 1000 + k));
    }
    let mut defs = Definitions::new();
    defs.define_all(&all).unwrap();
    let mem = MemTapes::new();
    let mut chain = PieceChain::create_on(
        mem.clone(),
        OwnerId([0x76; 32]),
        0,
        &[],
        chain_options(700, 0),
    )
    .unwrap();
    for t in 0..u64::from(n) {
        let (h, c) = chunk(t, 0);
        chain.write_chunk(&defs, &h, &column(&c)).unwrap();
    }
    let maps: Vec<_> = (1..=n).map(|k| map(1000 + k, host)).collect();
    defs.define_all(&maps).unwrap();
    chain.cut_now(&defs).unwrap();
    let (h, c) = chunk(u64::from(n) - 1, 1);
    chain.write_chunk(&defs, &h, &column(&c)).unwrap();
    chain.cut_now(&defs).unwrap();
    let pieces = chain.close().unwrap();
    assert_eq!(pieces.len(), 3);
    let left = pieces[0].piece.not_carried();
    assert!(left > 0 && left < n as usize, "{left}");
    // the newest data's track is carried, the oldest's isn't
    let marked = |p: &ww_tape::ChainPiece<_>, number: u32| {
        p.piece
            .tracks
            .iter()
            .find(|t| t.track_number == number)
            .unwrap()
            .not_carried
    };
    assert!(!marked(&pieces[0], n));
    assert!(marked(&pieces[0], 1));
    let bytes = mem.get(&pieces[1].tape).unwrap().unwrap().to_vec().unwrap();
    let r = TapeReader::open(&bytes[..]).unwrap();
    let at = r.definitions_at(0, u64::MAX).unwrap();
    assert_eq!(at.not_carried.len(), left);
    for t in &at.not_carried {
        assert!(!at.items.contains_key(&Slot::Track {
            number: t.track_number
        }));
    }
    assert!(at.items.contains_key(&Slot::Track { number: n }));
}

/// A head row marked not carried whose definition the piece holds all the same (a hostile or careless writer): the
/// definition is read as any other, and the reader doesn't name the track.
#[test]
fn a_marked_row_whose_definition_the_piece_holds_is_not_named() {
    let defs = base_defs();
    let row = |number: u32| ww_tape::piece::TrackStart {
        track_id: crate::definitions::track_id(number),
        track_number: number,
        next_item: 0,
        from_tick: i64::MIN,
        owner: OwnerId::NONE,
        hash: ww_tape::TrackHash::START,
        incomplete: false,
        not_carried: true,
    };
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    w.set_session(SessionTag::writable(0x7175_6968)).unwrap();
    let owner = OwnerId([0x57; 32]);
    let head = ww_tape::PieceHead {
        ident: ww_tape::PieceIdent {
            id: ww_tape::piece::PieceId::first(&owner),
            owner,
            seq: 0,
        },
        prev: None,
        prev_session: SessionTag::NONE,
        integrity: ww_tape::Integrity::Full,
    };
    w.begin_piece(&head, &[row(1), row(2)]).unwrap();
    // track 1's data puts its definition; track 2 stays quiet
    let (h, c) = chunk(0, 0);
    w.write_chunk(defs, &h, &column(&c)).unwrap();
    let tape = w.sink().tape();
    w.close().unwrap();
    let bytes = tape.to_vec().unwrap();
    let r = TapeReader::open(&bytes[..]).unwrap();
    assert!(r.verify(0).unwrap().holds);
    let at = r.definitions_at(0, u64::MAX).unwrap();
    assert!(at.items.contains_key(&Slot::Track { number: 1 }));
    let named: Vec<u32> = at.not_carried.iter().map(|t| t.track_number).collect();
    assert_eq!(named, [2]);
}

/// Whether every piece of `pieces` (tapes in `mem`) holds data: a chunk or a record of some track.
fn all_hold_data(mem: &MemTapes, pieces: &[ww_tape::ChainPiece<u64>]) -> bool {
    pieces.iter().all(|p| {
        let bytes = mem.get(&p.tape).unwrap().unwrap().to_vec().unwrap();
        let r = TapeReader::open(&bytes[..]).unwrap();
        r.pieces().iter().any(|i| {
            i.close
                .as_ref()
                .is_some_and(|(_, rows)| rows.iter().any(|t| t.chunks > 0 || t.records > 0))
        })
    })
}

/// The chain of [a_cut_short_of_room_leaves_out_the_oldest_and_a_reader_names_them] cut after the maps came: the
/// chain, its tapes and the numbers of the tracks the cut left out.
fn left_out_chain(policy: usize) -> (PieceChain<MemTapes>, MemTapes, Definitions, Vec<u32>) {
    let n = 300u32;
    let host = 9_000;
    let mut all = vec![keyed_clock(host)];
    for k in 1..=n {
        all.push(keyed_clock(1000 + k));
        all.push(crate::definitions::track(k, 1000 + k));
    }
    let mut defs = Definitions::new();
    defs.define_all(&all).unwrap();
    let mem = MemTapes::new();
    let mut chain = PieceChain::create_on(
        mem.clone(),
        OwnerId([0x77; 32]),
        0,
        &[],
        chain_options(policy, 0),
    )
    .unwrap();
    for t in 0..u64::from(n) {
        let (h, c) = chunk(t, 0);
        chain.write_chunk(&defs, &h, &column(&c)).unwrap();
    }
    let maps: Vec<_> = (1..=n).map(|k| map(1000 + k, host)).collect();
    defs.define_all(&maps).unwrap();
    chain.cut_now(&defs).unwrap();
    let left: Vec<u32> = chain
        .closed()
        .last()
        .unwrap()
        .piece
        .tracks
        .iter()
        .filter(|t| t.not_carried)
        .map(|t| t.track_number)
        .collect();
    (chain, mem, defs, left)
}

/// Data on a track the cut left out is never refused: a write that finds the piece full cuts to one carrying the
/// track first, once. Writes on the left-out tracks in turn keep the chain writing, and every piece holds data: at a
/// policy where some tracks fit (700), where the clocks and maps alone fill the room (604), and far below it (300).
#[test]
fn data_on_tracks_left_out_writes_on_and_no_piece_is_empty() {
    for policy in [700, 604, 300] {
        let (mut chain, mem, defs, left) = left_out_chain(policy);
        assert!(left.len() > 100, "{policy}: {}", left.len());
        let seq = chain.seq();
        for (k, number) in left.iter().take(40).enumerate() {
            let before = chain.seq();
            let (h, c) = chunk(u64::from(*number) - 1, 1 + k as u64);
            chain.write_chunk(&defs, &h, &column(&c)).unwrap();
            assert!(
                chain.seq() <= before + 1,
                "{policy}: at most one cut per write"
            );
        }
        assert!(chain.seq() > seq, "{policy}: full pieces were cut");
        let pieces = chain.close().unwrap();
        assert!(all_hold_data(&mem, &pieces), "{policy}");
    }
}

/// The same with the policy lowered mid-piece to 50 entries over 120 tracks on one clock: data on a track left out
/// writes on.
#[test]
fn data_on_a_track_left_out_by_a_lowered_policy_writes_on() {
    let mut all = vec![keyed_clock(CLOCK)];
    all.extend((1..=120).map(|n| crate::definitions::track(n, CLOCK)));
    let mut defs = Definitions::new();
    defs.define_all(&all).unwrap();
    let mem = MemTapes::new();
    let mut chain = PieceChain::create_on(
        mem.clone(),
        OwnerId([0x78; 32]),
        0,
        &[],
        chain_options(ww_tape::index::STATE_ENTRIES_MAX, 0),
    )
    .unwrap();
    for t in 0..120 {
        let (h, c) = chunk(t, 0);
        chain.write_chunk(&defs, &h, &column(&c)).unwrap();
    }
    chain
        .writer()
        .unwrap()
        .set_index_policy(IndexPolicy {
            state_entries_max: 50,
            ..IndexPolicy::default()
        })
        .unwrap();
    chain.cut_now(&defs).unwrap();
    let left: Vec<u32> = chain.closed()[0]
        .piece
        .tracks
        .iter()
        .filter(|t| t.not_carried)
        .map(|t| t.track_number)
        .collect();
    assert!(left.contains(&5), "{left:?}");
    for (k, number) in left.iter().take(30).enumerate() {
        let (h, c) = chunk(u64::from(*number) - 1, 1 + k as u64);
        chain.write_chunk(&defs, &h, &column(&c)).unwrap();
    }
    let pieces = chain.close().unwrap();
    assert!(pieces.len() > 2);
    assert!(all_hold_data(&mem, &pieces));
}

/// A marked row whose number the piece defines for another track (another id): the marked track's definition is
/// still missing, and the reader names it.
#[test]
fn a_marked_row_whose_number_another_track_took_is_named() {
    let defs = base_defs();
    let ww_tape::Describe::Track(t) = crate::definitions::track(1, CLOCK) else {
        unreachable!("track() gives a track")
    };
    let other = ww_tape::TrackDef {
        key: ww_tape::track_key(0xad, &[ww_tape::Segment::Index(999)]),
        ..t
    };
    let mut renumbered = defs.clone();
    renumbered
        .define(&ww_tape::Describe::Track(other.clone()))
        .unwrap();
    let row = ww_tape::piece::TrackStart {
        track_id: crate::definitions::track_id(1),
        track_number: 1,
        next_item: 0,
        from_tick: i64::MIN,
        owner: OwnerId::NONE,
        hash: ww_tape::TrackHash::START,
        incomplete: false,
        not_carried: true,
    };
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    w.set_session(SessionTag::writable(0x7175_6969)).unwrap();
    let owner = OwnerId([0x58; 32]);
    let head = ww_tape::PieceHead {
        ident: ww_tape::PieceIdent {
            id: ww_tape::piece::PieceId::first(&owner),
            owner,
            seq: 0,
        },
        prev: None,
        prev_session: SessionTag::NONE,
        integrity: ww_tape::Integrity::Full,
    };
    w.begin_piece(&head, &[row]).unwrap();
    // number 1 now names the other track, which gives data
    let (mut h, c) = chunk(0, 0);
    h.track_id = other.id();
    w.write_chunk(&renumbered, &h, &column(&c)).unwrap();
    let tape = w.sink().tape();
    w.close().unwrap();
    let bytes = tape.to_vec().unwrap();
    let r = TapeReader::open(&bytes[..]).unwrap();
    let at = r.definitions_at(0, u64::MAX).unwrap();
    assert!(at.items.contains_key(&Slot::Track { number: 1 }));
    let named: Vec<u64> = at.not_carried.iter().map(|t| t.track_id).collect();
    assert_eq!(named, [crate::definitions::track_id(1)]);
}

/// A xorshift generator for the random schedule.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// The invariant of admission, cuts and carried definitions, over random schedules: tracks on shared or own clocks,
/// maps and longer labels defined on the way, the index policy lowered on the way (never below what one track's
/// definitions need with the layout and the producer: 6 entries, so every call is admitted), cuts by the chain for
/// a full piece or by its policy, and by hand, each followed by data on the track written last (the one the cut
/// carries first). Asserted: no write is refused, no piece is empty, and a reader of each piece alone names exactly
/// the tracks the cut marked that got no data in the piece.
#[test]
fn random_schedules_refuse_no_counted_in_track_and_leave_no_piece_empty() {
    let host = 9_000u32;
    for seed in 1..=40u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let tracks = 8 + u32::try_from(rng.below(48)).unwrap();
        let mut all = vec![keyed_clock(CLOCK), keyed_clock(host)];
        let mut own_clock = Vec::new();
        for k in 1..=tracks {
            let own = rng.below(2) == 0;
            let clock = if own { 1000 + k } else { CLOCK };
            if own {
                all.push(keyed_clock(clock));
                own_clock.push(k);
            }
            all.push(crate::definitions::track(k, clock));
        }
        let mut defs = Definitions::new();
        defs.define_all(&all).unwrap();
        let mem = MemTapes::new();
        let policy = 12 + usize::try_from(rng.below(60)).unwrap();
        let chunks = if rng.below(2) == 0 {
            0
        } else {
            2 + rng.below(8)
        };
        let mut chain = PieceChain::create_on(
            mem.clone(),
            OwnerId([0x79; 32]),
            0,
            &[],
            chain_options(policy, chunks),
        )
        .unwrap();
        let mut next_chunk = vec![0u64; usize::try_from(tracks).unwrap() + 1];
        // the tracks with data in each piece, by seq
        let mut data_in: Vec<std::collections::BTreeSet<u64>> = vec![Default::default()];
        let mut has_data = false;
        let mut maps_left = own_clock.clone();
        let mut policy = policy;
        let mut last = 1u32;
        let mut write = |chain: &mut PieceChain<MemTapes>,
                         defs: &Definitions,
                         k: u32,
                         data_in: &mut Vec<std::collections::BTreeSet<u64>>,
                         step: u64| {
            let (h, c) = chunk(u64::from(k) - 1, next_chunk[usize::try_from(k).unwrap()]);
            next_chunk[usize::try_from(k).unwrap()] += 1;
            chain
                .write_chunk(defs, &h, &column(&c))
                .unwrap_or_else(|e| panic!("seed {seed} step {step}: {e:?}"));
            while data_in.len() <= usize::try_from(chain.seq()).unwrap() {
                data_in.push(Default::default());
            }
            data_in[usize::try_from(chain.seq()).unwrap()].insert(h.track_id);
        };
        for step in 0..160u64 {
            match rng.below(10) {
                0 if !maps_left.is_empty() => {
                    let k = maps_left
                        .swap_remove(usize::try_from(rng.below(maps_left.len() as u64)).unwrap());
                    defs.define(&map(1000 + k, host)).unwrap();
                }
                1 => {
                    let k = 1 + u32::try_from(rng.below(u64::from(tracks))).unwrap();
                    let clock = if own_clock.contains(&k) {
                        1000 + k
                    } else {
                        CLOCK
                    };
                    let long: &'static str = Box::leak(
                        "z".repeat(200 + usize::try_from(step).unwrap() * 9)
                            .into_boxed_str(),
                    );
                    defs.define(&crate::definitions::labeled(k, clock, long))
                        .unwrap();
                }
                2 if policy > 6 => {
                    policy = 6 + usize::try_from(rng.below((policy - 6) as u64)).unwrap();
                    chain
                        .writer()
                        .unwrap()
                        .set_index_policy(IndexPolicy {
                            state_entries_max: policy,
                            ..IndexPolicy::default()
                        })
                        .unwrap();
                }
                3 if has_data => {
                    // a cut by hand carries the last call's tracks first: data on them follows
                    chain.cut_now(&defs).unwrap();
                    write(&mut chain, &defs, last, &mut data_in, step);
                }
                _ => {
                    let k = 1 + u32::try_from(rng.below(u64::from(tracks))).unwrap();
                    write(&mut chain, &defs, k, &mut data_in, step);
                    last = k;
                    has_data = true;
                }
            }
        }
        if !has_data {
            write(&mut chain, &defs, 1, &mut data_in, 160);
        }
        let pieces = chain.close().unwrap();
        assert!(all_hold_data(&mem, &pieces), "seed {seed}");
        for (i, p) in pieces.iter().enumerate().skip(1) {
            // the writer's marks for this piece's head, less the tracks whose data put their definition here
            let mut want: Vec<u64> = pieces[i - 1]
                .piece
                .tracks
                .iter()
                .filter(|t| t.not_carried && !data_in[i].contains(&t.track_id))
                .map(|t| t.track_id)
                .collect();
            want.sort_unstable();
            let bytes = mem.get(&p.tape).unwrap().unwrap().to_vec().unwrap();
            let r = TapeReader::open(&bytes[..]).unwrap();
            let at = r.definitions_at(0, u64::MAX).unwrap();
            assert!(at.is_whole(), "seed {seed} piece {i}");
            let mut named: Vec<u64> = at.not_carried.iter().map(|t| t.track_id).collect();
            named.sort_unstable();
            assert_eq!(named, want, "seed {seed} piece {i}");
        }
    }
}

/// The base definitions and state track 10, whose item the tests put once.
fn defs_with_state() -> Definitions {
    let mut defs = base_defs().clone();
    defs.define_all(&[crate::definitions::state(10)]).unwrap();
    defs
}

/// A chain of two pieces in memory: track 10's state put once, then a chunk of track 1 in each piece. The bytes of
/// each piece, oldest first.
fn state_chain(defs: &Definitions) -> Vec<Vec<u8>> {
    let mem = MemTapes::new();
    let mut chain = PieceChain::create_on(
        mem.clone(),
        OwnerId([0x53; 32]),
        0,
        &[],
        ww_tape::ChainOptions {
            tapes: ww_tape::PieceTapes::Each,
            writer: ww_tape::WriterOptions {
                sync: SyncPolicy::None,
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .unwrap();
    assert!(chain.put(defs, 10, 5, b"run").unwrap());
    let (h, c) = chunk(0, 0);
    chain.write_chunk(defs, &h, &column(&c)).unwrap();
    chain.cut_now(defs).unwrap();
    let (h, c) = chunk(0, 1);
    chain.write_chunk(defs, &h, &column(&c)).unwrap();
    chain
        .close()
        .unwrap()
        .iter()
        .map(|f| mem.get(&f.tape).unwrap().unwrap().to_vec().unwrap())
        .collect()
}

/// A state put once and never again: the piece after the cut alone holds the state track's definition and its
/// newest item, so dropping the first piece loses neither.
#[test]
fn a_new_piece_carries_a_quiet_state_and_its_definition() {
    let defs = defs_with_state();
    let pieces = state_chain(&defs);
    assert_eq!(pieces.len(), 2);
    let r = TapeReader::open(&pieces[1][..]).unwrap();
    assert!(r.verify(0).unwrap().holds);
    let at = r.definitions_at(0, u64::MAX).unwrap();
    assert_eq!(
        at.items
            .get(&Slot::Track { number: 10 })
            .map(|i| i.item.as_slice()),
        defs.get(&Slot::Track { number: 10 }),
    );
    let found = r
        .state_at(0, Slot::State { track: 10 }, u64::MAX)
        .unwrap()
        .item
        .expect("the newest state of track 10");
    assert_eq!(found.item, b"run");
}

/// The piece after the cut of [state_chain], carrying track 10's definition and state, pinned byte for byte.
#[test]
fn carried_state_golden() {
    let pieces = state_chain(&defs_with_state());
    golden(&pieces[1], "v1-carried-state.hex");
}
