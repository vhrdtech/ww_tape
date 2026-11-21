//! Where [TapeWriter::write_chunk], [TapeWriter::write_block], [TapeWriter::adopt_block] and
//! [PieceChain::write_chunk] say a block landed: the offset and length read back as that block with
//! [TapeReader::read_block].

use ww_tape::TrackNum;
use ww_tape::index::RECORDS_TRACK_ID;
use ww_tape::index::{IndexEntry, IndexPolicy};
use ww_tape::{
    BlockId, BlockKind, ChainOptions, CutPolicy, MemSink, OwnerId, SessionTag, SyncPolicy,
    TapeReader, TapeWriter, WriterOptions,
};
use ww_tape_fs::piece_name;

use crate::pieces::{chunk, column, records, scratch};

/// An entry naming only the offset and length [TapeReader::read_block] reads, the rest unused by it.
fn at(offset: u64, len: u32) -> IndexEntry {
    IndexEntry {
        offset,
        len,
        kind: BlockKind::Records,
        level: 0,
        tree: 0,
        track_id: RECORDS_TRACK_ID,
        number: 0,
        second: 0,
        first_item: ww_tape::ItemNum::new(0),
        count: 0,
        first_tick: 0,
        last_tick: 0,
        latest_end: 0,
        repeat: false,
        relation_break: false,
        unit: ww_tape::UnitHash::NONE,
        range: None,
    }
}

/// [TapeWriter::write_chunk] and [TapeWriter::write_block] say where the block they were asked to write landed,
/// with puts before it (the layout, the producer, the clock and the track; an explicit put of a state track too)
/// and a checkpoint due right after it (a small [IndexPolicy::bytes]).
#[test]
fn write_chunk_and_write_block_say_where_they_landed() {
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    w.set_index_policy(IndexPolicy {
        bytes: exact_units::Bytes::new(128),
        interval: None,
        ..IndexPolicy::default()
    })
    .expect("a policy with room for a piece head");
    let mut defs = crate::definitions::base_defs().clone();
    crate::definitions::hold(
        &mut defs,
        core::slice::from_ref(&crate::definitions::state(5)),
    )
    .unwrap();

    // the layout, the producer, the clock and the track go in as puts before this, the first call; a checkpoint is
    // due once its bytes pass the policy's, right after this block
    let (head, cols) = chunk(0, 0);
    let chunk_placed = w.write_chunk(&defs, &head, &column(&cols)).unwrap();

    // an explicit put lands right before the next block
    assert!(w.put(&defs, TrackNum::new(5), 1, b"state").unwrap());
    let block_placed = w
        .write_block(&defs, &records(w.next_id(), 7))
        .unwrap()
        .expect("a records block lands as itself");
    assert_ne!(
        chunk_placed.offset, block_placed.offset,
        "two different blocks"
    );

    let tape = w.sink().tape();
    w.close().unwrap();
    let bytes = tape.to_vec().unwrap();
    let r = TapeReader::open(&bytes[..]).unwrap();

    // the small index policy makes a checkpoint due once the chunk's bytes pass it: it lands in the gap before the
    // put and the records block that follow
    let chunk_end = chunk_placed.offset + u64::from(chunk_placed.len);
    assert!(
        block_placed.offset > chunk_end,
        "something sits between them"
    );
    let mut gap =
        ww_tape::FileBlocks::new(&bytes[chunk_end as usize..block_placed.offset as usize]);
    let mut kinds = Vec::new();
    while let Some(b) = gap.next_block() {
        kinds.push(b.unwrap().kind);
    }
    assert!(
        kinds.contains(&BlockKind::Index),
        "a checkpoint between them: {kinds:?}"
    );

    let mut buf = Vec::new();
    let b = r
        .read_block(0, &at(chunk_placed.offset, chunk_placed.len), &mut buf)
        .unwrap();
    let got = b.chunk().expect("a chunk").head();
    assert_eq!(
        (got.track_number, got.first_item),
        (head.track_number, head.first_item)
    );

    let mut buf = Vec::new();
    let b = r
        .read_block(0, &at(block_placed.offset, block_placed.len), &mut buf)
        .unwrap();
    let mut decoded = Vec::new();
    let d = b.decode(&mut decoded).unwrap();
    let rec = d.records().next().unwrap().unwrap();
    assert_eq!((rec.track.get(), rec.item), (9, [7u8; 30].as_slice()));
}

/// [TapeWriter::adopt_block] says where a producer's block landed, once renumbered into the writer's own session.
#[test]
fn adopt_block_says_where_it_landed() {
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    let defs = crate::definitions::base_defs();
    let foreign = BlockId::new(SessionTag::writable(0x1234_5678), 3);
    let placed = w
        .adopt_block(defs, &records(foreign, 11))
        .unwrap()
        .expect("a records block lands as itself");

    let tape = w.sink().tape();
    w.close().unwrap();
    let bytes = tape.to_vec().unwrap();
    let r = TapeReader::open(&bytes[..]).unwrap();
    let mut buf = Vec::new();
    let b = r
        .read_block(0, &at(placed.offset, placed.len), &mut buf)
        .unwrap();
    let mut decoded = Vec::new();
    let d = b.decode(&mut decoded).unwrap();
    let rec = d.records().next().unwrap().unwrap();
    assert_eq!((rec.track.get(), rec.item), (9, [11u8; 30].as_slice()));
}

/// [PieceChain::write_chunk] says which piece a chunk landed in: a cut before the second chunk (its [CutPolicy]
/// cuts every chunk) moves it to the next piece's file.
#[test]
fn piece_chain_write_chunk_says_which_piece_it_landed_in() {
    let dir = scratch("placed");
    let mut chain = ww_tape_fs::chain::create(
        &dir,
        OwnerId([0x45; 32]),
        0,
        &[],
        ChainOptions {
            tapes: ww_tape::PieceTapes::Each,
            writer: WriterOptions {
                sync: SyncPolicy::None,
                ..Default::default()
            },
            cut: CutPolicy {
                chunks: exact_units::Count::new(1),
                ..Default::default()
            },
        },
    )
    .unwrap();
    let defs = crate::definitions::base_defs();
    let (head0, cols0) = chunk(0, 0);
    let first = chain.write_chunk(defs, &head0, &column(&cols0)).unwrap();
    let (head1, cols1) = chunk(0, 1);
    let second = chain.write_chunk(defs, &head1, &column(&cols1)).unwrap();
    assert_eq!((first.seq, second.seq), (0, 1), "a cut before the second");

    let closed = chain.close().unwrap();
    assert_eq!(closed.len(), 2);

    for (placed, head) in [(first, head0), (second, head1)] {
        let bytes = std::fs::read(dir.join(piece_name(placed.seq))).unwrap();
        let r = TapeReader::open(&bytes[..]).unwrap();
        let mut buf = Vec::new();
        let b = r
            .read_block(0, &at(placed.offset, placed.len), &mut buf)
            .unwrap();
        let got = b.chunk().expect("a chunk").head();
        assert_eq!(
            (got.track_number, got.first_item),
            (head.track_number, head.first_item)
        );
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// [Placed::unit](ww_tape::Placed::unit) and [ChunkPlaced::unit](ww_tape::ChunkPlaced::unit) are the unit hash a
/// reader takes from the index entry of the same chunk: in one tape of several pieces and in a tape per piece,
/// before and after cuts; a records block states none.
#[test]
fn a_chunk_placed_states_the_unit_of_its_index_entry() {
    for tapes in [ww_tape::PieceTapes::One, ww_tape::PieceTapes::Each] {
        let mut chain = ww_tape::PieceChain::create_on(
            ww_tape::MemTapes::new(),
            OwnerId([0x46; 32]),
            0,
            &[],
            ChainOptions {
                tapes,
                writer: WriterOptions {
                    sync: SyncPolicy::None,
                    ..Default::default()
                },
                cut: CutPolicy {
                    chunks: exact_units::Count::new(3),
                    ..Default::default()
                },
            },
        )
        .unwrap();
        let defs = crate::definitions::base_defs();
        let mut placed = Vec::new();
        for n in 0..10u64 {
            let (head, cols) = chunk(n % 2, n / 2);
            placed.push(chain.write_chunk(defs, &head, &column(&cols)).unwrap());
            if n == 4 {
                let w = chain.writer().unwrap();
                let id = w.next_id();
                let block = w.write_block(defs, &records(id, 3)).unwrap().unwrap();
                assert!(block.unit.is_none(), "{tapes:?}: a records block");
            }
        }
        assert_eq!(placed.last().unwrap().seq, 3, "{tapes:?}: three cuts");
        let mem = chain.tapes().clone();
        chain.close().unwrap();

        for p in &placed {
            let (key, piece) = match tapes {
                ww_tape::PieceTapes::One => (0, p.seq as usize),
                ww_tape::PieceTapes::Each => (p.seq, 0),
            };
            let bytes = mem.get(&key).unwrap().unwrap().to_vec().unwrap();
            let r = TapeReader::open(&bytes[..]).unwrap();
            let entry = r
                .entries(piece)
                .unwrap()
                .into_iter()
                .find(|e| e.offset == p.offset)
                .expect("an index entry for the chunk");
            assert_eq!(entry.len, p.len);
            assert!(!p.unit.is_none(), "{tapes:?}: a chunk states its unit");
            assert_eq!(
                entry.unit, p.unit,
                "{tapes:?}: piece {} at {}",
                p.seq, p.offset
            );
        }
    }
}

/// A chunk with an empty payload (one column of no bytes: the payload is its padding alone) states the unit hash its
/// index entry carries, the hash of the chunk read back.
#[test]
fn an_empty_chunk_placed_states_the_unit_of_its_index_entry() {
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    let defs = crate::definitions::base_defs();
    let (head, _) = chunk(0, 0);
    let placed = w.write_chunk(defs, &head, &column(&[])).unwrap();
    assert!(!placed.unit.is_none(), "a chunk states its unit");
    let tape = w.sink().tape();
    w.close().unwrap();
    let bytes = tape.to_vec().unwrap();
    let r = TapeReader::open(&bytes[..]).unwrap();
    let entry = r
        .entries(0)
        .unwrap()
        .into_iter()
        .find(|e| e.offset == placed.offset)
        .expect("an index entry for the chunk");
    assert_eq!(entry.unit, placed.unit);
    let mut buf = Vec::new();
    let b = r.read_block(0, &entry, &mut buf).unwrap();
    let c = b.chunk().expect("a chunk");
    assert_eq!(
        c.payload(),
        &[0u8; ww_tape::chunk::PAYLOAD_PADDING],
        "no column bytes"
    );
    assert_eq!(
        ww_tape::TrackHash::chunk_unit(&c, &ww_tape::piece::payload_hash(c.payload())),
        placed.unit
    );
}

/// [TapeWriter::write_chunk_hashed] with the payload hash a producer computed writes the same bytes, the same content
/// hash and the same track hashes as [TapeWriter::write_chunk] (WRITE-13).
#[test]
fn a_chunk_with_its_payload_hash_given_writes_the_same_piece() {
    let defs = crate::definitions::base_defs().clone();
    let write = |hashed: bool| {
        let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
        w.set_session(crate::TEST_SESSION).unwrap();
        for k in 0..4 {
            let (head, cols) = chunk(0, k);
            let columns = column(&cols);
            if hashed {
                let h = ww_tape::piece::columns_payload_hash(columns.iter().map(|(_, b)| *b));
                w.write_chunk_hashed(&defs, &head, &columns, h).unwrap();
            } else {
                w.write_chunk(&defs, &head, &columns).unwrap();
            }
        }
        let tape = w.sink().tape();
        let closed = w.close().unwrap();
        (tape.to_vec().unwrap(), closed.hash, closed.tracks)
    };
    // compared whole: a difference prints both pieces, so only a bool is asserted
    assert!(write(true) == write(false), "the same piece either way");
}
