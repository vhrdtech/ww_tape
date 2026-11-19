//! Where [TapeWriter::write_chunk], [TapeWriter::write_block], [TapeWriter::adopt_block] and
//! [PieceChain::write_chunk] say a block landed: the offset and length read back as that block with
//! [TapeReader::read_block].

use ww_tape::index::{IndexEntry, IndexPolicy};
use ww_tape::piece::piece_name;
use ww_tape::{
    BlockId, BlockKind, ChainOptions, CutPolicy, MemSink, OwnerId, SessionTag, SyncPolicy,
    TapeReader, TapeWriter, WriterOptions,
};

use crate::pieces::{chunk, column, records, scratch};

/// An entry naming only the offset and length [TapeReader::read_block] reads, the rest unused by it.
fn at(offset: u64, len: u32) -> IndexEntry {
    IndexEntry {
        offset,
        len,
        kind: BlockKind::Records,
        level: 0,
        tree: 0,
        track_id: 0,
        track_number: 0,
        second: 0,
        first_item: 0,
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
        bytes: 128,
        interval: None,
        ..IndexPolicy::default()
    })
    .expect("a policy with room for a piece head");
    let mut defs = crate::definitions::base_defs().clone();
    defs.define(&crate::definitions::state(5)).unwrap();

    // the layout, the producer, the clock and the track go in as puts before this, the first call; a checkpoint is
    // due once its bytes pass the policy's, right after this block
    let (head, cols) = chunk(0, 0);
    let chunk_placed = w.write_chunk(&defs, &head, &column(&cols)).unwrap();

    // an explicit put lands right before the next block
    assert!(w.put(&defs, 5, 1, b"state").unwrap());
    let block_placed = w
        .write_block(&defs, &records(w.next_id(), 7))
        .unwrap()
        .expect("a records block lands as itself");
    assert_ne!(
        chunk_placed.offset, block_placed.offset,
        "two different blocks"
    );

    let tape = w.sink().tape();
    w.finish().unwrap();
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
    assert_eq!((rec.track, rec.item), (9, [7u8; 30].as_slice()));
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
    w.finish().unwrap();
    let bytes = tape.to_vec().unwrap();
    let r = TapeReader::open(&bytes[..]).unwrap();
    let mut buf = Vec::new();
    let b = r
        .read_block(0, &at(placed.offset, placed.len), &mut buf)
        .unwrap();
    let mut decoded = Vec::new();
    let d = b.decode(&mut decoded).unwrap();
    let rec = d.records().next().unwrap().unwrap();
    assert_eq!((rec.track, rec.item), (9, [11u8; 30].as_slice()));
}

/// [PieceChain::write_chunk] says which piece a chunk landed in: a cut before the second chunk (its [CutPolicy]
/// cuts every chunk) moves it to the next piece's file.
#[test]
fn piece_chain_write_chunk_says_which_piece_it_landed_in() {
    let dir = scratch("placed");
    let mut chain = ww_tape::piece::fs::create(
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
                chunks: 1,
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

    let finished = chain.finish().unwrap();
    assert_eq!(finished.len(), 2);

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
