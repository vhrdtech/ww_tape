//! Memory tapes: pieces in segments, and the oldest pieces of a tape of several dropped ([MemTapes::drop_front]).

use ww_tape::{
    ByteSource, ItemNum, MemError, MemTapes, OwnerId, PieceTapes, ReadError, TapeReader, TrackHash,
};

use crate::range_hash::write;

/// A chain of several pieces in one memory tape, the same tape copied: (tapes, key, bytes).
fn one_tape() -> (MemTapes, u64, Vec<u8>) {
    let (_, bytes) = write(PieceTapes::One, 0xd20f);
    let tapes = MemTapes::new();
    let mut sink = tapes.create_whole(&0).unwrap();
    ww_tape::TapeSink::write_all(&mut sink, &bytes[0]).unwrap();
    ww_tape::TapeSink::close(&mut sink).unwrap();
    (tapes, 0, bytes[0].clone())
}

/// Dropping the front keeps the offsets: a reader opened afterwards finds the pieces kept where they were, as a
/// head cut, and the bytes held shrink by the pieces dropped.
#[test]
fn dropped_pieces_leave_the_rest_where_it_was() {
    let (_, bytes) = write(PieceTapes::One, 0xd20f);
    let full = TapeReader::open(&bytes[0][..]).unwrap();
    assert!(full.pieces().len() >= 5, "{} pieces", full.pieces().len());
    let tapes = MemTapes::new();
    let mut sink = tapes.create_whole(&0).unwrap();
    // each piece written as its own: the sink seals a segment per piece
    for p in full.pieces() {
        ww_tape::TapeSink::rebase(&mut sink, p.start).unwrap();
        ww_tape::TapeSink::write_all(&mut sink, &bytes[0][p.start as usize..p.end as usize])
            .unwrap();
    }
    let tape = tapes.get(&0).unwrap().unwrap();
    assert_eq!(tape.to_vec().unwrap(), bytes[0]);

    // inside piece 2: pieces 0 and 1 go
    let p2 = &full.pieces()[2];
    let front = tapes.drop_front(&0, p2.start + 1).unwrap().unwrap();
    assert_eq!(front, p2.start);
    assert_eq!(tape.start().unwrap(), p2.start);
    assert_eq!(tape.size().unwrap(), bytes[0].len() as u64);
    assert_eq!(tape.held().unwrap(), bytes[0].len() as u64 - p2.start);
    assert_eq!(tape.to_vec().unwrap(), &bytes[0][p2.start as usize..]);
    assert_eq!(
        tape.read_at(0, &mut [0; 4]),
        Err(MemError::Dropped { front })
    );

    let r = TapeReader::open(tape.clone()).unwrap();
    assert_eq!(r.pieces(), &full.pieces()[2..]);
    let chain = r.chain();
    assert!(chain.head_cut, "{chain:?}");
    // the kept pieces read as before: their range hashes are those of the bytes from piece 2 on
    let rest = TapeReader::open(&bytes[0][p2.start as usize..]).unwrap();
    range_hashes_agree(&r, &rest);

    // the reader opened after the first drop fails once its pieces are dropped too
    let last = full.pieces().last().unwrap();
    // the last piece is still being written: it stays until the sink closes
    assert_eq!(tapes.drop_front(&0, u64::MAX).unwrap(), Some(last.start));
    ww_tape::TapeSink::close(&mut sink).unwrap();
    assert_eq!(tapes.drop_front(&0, u64::MAX).unwrap(), Some(last.end));
    assert_eq!(tape.held().unwrap(), 0);
    assert!(matches!(
        r.range_hash(
            crate::range_hash::id_of(0),
            OwnerId::NONE,
            ItemNum::new(0)..ItemNum::new(u64::MAX),
            TrackHash::START
        ),
        Err(ReadError::Source(MemError::Dropped { .. }))
    ));
    assert_eq!(tapes.drop_front(&1, 0).unwrap(), None);
}

/// Each track's range hash over all its items is the same from `r` as from `want`.
fn range_hashes_agree<S: ByteSource, W: ByteSource>(r: &TapeReader<S>, want: &TapeReader<W>)
where
    S::Error: std::fmt::Debug,
    W::Error: std::fmt::Debug,
{
    let all = ItemNum::new(0)..ItemNum::new(u64::MAX);
    for t in 0..3 {
        let id = crate::range_hash::id_of(t);
        let got = r.range_hash(id, OwnerId::NONE, all.clone(), TrackHash::START);
        let want = want.range_hash(id, OwnerId::NONE, all.clone(), TrackHash::START);
        let (got, want) = (got.unwrap(), want.unwrap());
        assert_eq!(got.hash, want.hash, "track {t}");
        assert_eq!(got.items, want.items, "track {t}");
    }
}

/// The piece being written is never dropped, and the writer goes on after a drop.
#[test]
fn a_writer_goes_on_after_a_drop() {
    let (tapes, key, bytes) = one_tape();
    let mut sink = tapes.open_whole(&key).unwrap().unwrap();
    let len = bytes.len() as u64;
    ww_tape::TapeSink::rebase(&mut sink, len).unwrap();
    ww_tape::TapeSink::write_all(&mut sink, b"open").unwrap();
    assert_eq!(tapes.drop_front(&key, len + 4).unwrap(), Some(len));
    ww_tape::TapeSink::write_all(&mut sink, b" piece").unwrap();
    let mut buf = [0; 10];
    ww_tape::TapeSink::read_at(&sink, 0, &mut buf).unwrap();
    assert_eq!(&buf, b"open piece");
    assert_eq!(
        ww_tape::TapeSink::rebase(&mut sink, 0),
        Err(MemError::Dropped { front: len })
    );
}
