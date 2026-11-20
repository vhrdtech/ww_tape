//! A new piece carries the definitions of the tracks its head carries, quiet ones too (WRITE-9).

use ww_tape::index::IndexPolicy;
use ww_tape::{
    Definitions, IndexError, MemSink, MemTapes, OwnerId, PieceChain, SessionTag, Slot, SyncPolicy,
    TapeReader, TapeSink, TapeWriter, WriteError,
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

/// A cut whose next piece has no room for the state entries of its carried definitions is refused before the piece
/// ends; the writer comes back and closes as before.
#[test]
fn a_cut_without_room_for_the_carried_definitions_keeps_the_writer() {
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
    let len = w.bytes();
    // the next piece needs the layout, the producer, the clock and three tracks: 6
    w.set_index_policy(IndexPolicy {
        state_entries_max: 5,
        ..IndexPolicy::default()
    })
    .unwrap();
    let refused = w.cut_on(defs, MemSink::new()).err().unwrap();
    assert!(
        matches!(
            refused.error,
            WriteError::Index(IndexError::StateEntries { max: 5, used: 6 })
        ),
        "{:?}",
        refused.error
    );
    let mut w = *refused.writer.unwrap();
    assert_eq!(w.bytes(), len);
    w.set_index_policy(IndexPolicy::default()).unwrap();
    let (_, next) = w.cut_on(defs, MemSink::new()).unwrap();
    let tape = next.sink().tape();
    next.close().unwrap();
    let held = held(&tape.to_vec().unwrap());
    for number in 1..=3 {
        assert!(held.contains_key(&Slot::Track { number }), "{number}");
    }
}

/// A chain's cut refused the same way keeps the piece being written.
#[test]
fn a_chain_cut_without_room_keeps_its_piece() {
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
            state_entries_max: 5,
            ..IndexPolicy::default()
        })
        .unwrap();
    assert!(matches!(
        chain.cut_now(defs),
        Err(WriteError::Index(IndexError::StateEntries { .. }))
    ));
    assert_eq!(chain.seq(), 0);
    chain
        .writer()
        .unwrap()
        .set_index_policy(IndexPolicy::default())
        .unwrap();
    let (h, c) = chunk(0, 1);
    chain.write_chunk(defs, &h, &column(&c)).unwrap();
    assert_eq!(chain.close().unwrap().len(), 1);
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
