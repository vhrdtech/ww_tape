//! A gap in a track's item numbers between two chunks reads as items lost, not as damage: the writer takes the later
//! chunk, the index lists both, the close counts the range, the next piece goes on after the later chunk, and a
//! resume keeps it all.

use ww_tape::{
    ChainOptions, ItemNum, MemTapes, OwnerId, PieceChain, PieceTapes, SyncPolicy, TapeReader,
    TrackHash, WriterOptions,
};

use crate::definitions::base_defs;
use crate::pieces::{chunk, chunk_id, column};

fn options() -> ChainOptions {
    ChainOptions {
        tapes: PieceTapes::Each,
        writer: WriterOptions {
            sync: SyncPolicy::None,
            ..WriterOptions::default()
        },
        ..ChainOptions::default()
    }
}

/// Chunks 0 and 2 of track 0 (items 0..50 and 100..150), chunk 1 lost: then a cut and chunk 3 (150..200).
#[test]
fn an_item_number_gap_between_chunks_reads_as_lost_items() {
    let defs = base_defs();
    let owner = OwnerId([0x9a; 32]);
    let mem = MemTapes::new();
    let mut chain = PieceChain::create_on(mem.clone(), owner, 0, &[], options()).unwrap();
    for k in [0, 2] {
        let (h, c) = chunk(0, k);
        chain.write_chunk(defs, &h, &column(&c)).unwrap();
    }
    chain.cut_now(defs).unwrap();
    let (h, c) = chunk(0, 3);
    chain.write_chunk(defs, &h, &column(&c)).unwrap();
    let pieces = chain.close().unwrap();
    assert_eq!(pieces.len(), 2);

    // the cut's close: the range spans the gap, and the next piece goes on after the later chunk
    let first = &pieces[0].piece;
    let start = first
        .tracks
        .iter()
        .find(|t| t.track_id == chunk_id(0))
        .unwrap();
    assert_eq!(start.next_item, ItemNum::new(150));

    let bytes: Vec<Vec<u8>> = pieces
        .iter()
        .map(|p| mem.get(&p.tape).unwrap().unwrap().to_vec().unwrap())
        .collect();
    let r = TapeReader::open(&bytes[0][..]).unwrap();
    assert!(r.verify(0).unwrap().holds, "the gap is no damage");
    let entries = r.track_entries(0, chunk_id(0)).unwrap();
    let items: Vec<_> = entries
        .iter()
        .map(|e| (e.first_item.get(), e.count))
        .collect();
    assert_eq!(items, [(0, 50), (100, 50)]);
    let (_, ranges) = r.pieces()[0].close.as_ref().unwrap();
    assert_eq!(
        (ranges[0].first_item.get(), ranges[0].end_item().get()),
        (0, 150)
    );

    // the lost items fold no chunk; the items around them fold as when written alone
    let whole = bytes.concat();
    let r_all = TapeReader::open(&whole[..]).unwrap();
    let lost = r_all
        .range_hash(
            chunk_id(0),
            OwnerId::NONE,
            ItemNum::new(50)..ItemNum::new(100),
            TrackHash::START,
        )
        .unwrap();
    assert_eq!(lost.chunks, 0);
    let all = r_all
        .range_hash(
            chunk_id(0),
            OwnerId::NONE,
            ItemNum::new(0)..ItemNum::new(200),
            TrackHash::START,
        )
        .unwrap();
    assert_eq!(all.chunks, 3);

    // a resume of the closed chain writes on after the later chunk
    let (mut chain, _) = PieceChain::resume_on(mem.clone(), owner, 0, options()).unwrap();
    let (h, c) = chunk(0, 5);
    chain.write_chunk(defs, &h, &column(&c)).unwrap();
    chain.close().unwrap();
}
