//! The positional reader over crafted bytes: offsets from the file never panic or loop.

use ww_tape::ItemNum;

use crate::id;
use crate::range_hash::{crc32c, id_of, rewrite_final_entries, write};
use ww_tape::TrackId;
use ww_tape::index::IndexEntry;
use ww_tape::piece::{
    ChainEnd, Close, CloseReason, ContentHash, TAIL_LEN, Tail, close_len, write_close,
};
use ww_tape::reader::ReadError;
use ww_tape::{BlockKind, PieceTapes, TapeReader};

fn tail(piece_len: u64, close_offset: u64, close_len: u32) -> [u8; TAIL_LEN] {
    let t = Tail {
        piece_len,
        close_offset,
        close_len,
        reason: CloseReason::Closed,
        hash: ContentHash([0; 32]),
        ident: None,
        next: None,
    };
    let mut b = [0u8; TAIL_LEN];
    t.write(&mut b, id(0));
    b
}

#[test]
fn tail_with_wrapping_offsets_is_no_tail() {
    let bytes = tail(TAIL_LEN as u64, u64::MAX - 343, 180);
    let r = TapeReader::open(&bytes[..]).unwrap();
    assert!(r.pieces().iter().all(|p| p.end <= TAIL_LEN as u64));
}

/// A closed piece of a close record naming `final_index` and its tail, nothing else.
fn close_only(final_index: u64) -> Vec<u8> {
    let close = Close {
        reason: CloseReason::Closed,
        hash: ContentHash([0; 32]),
        checkpoint: None,
        final_index: Some(final_index),
        prev: None,
        summary: None,
        ident: None,
        next: None,
        unlisted_keys_unknown: false,
    };
    let mut bytes = vec![0u8; close_len(0, 0)];
    let n = write_close(&mut bytes, id(0), &close, (&[], &[]), 0).unwrap();
    bytes.truncate(n);
    bytes.extend(tail((n + TAIL_LEN) as u64, 0, n as u32));
    bytes
}

#[test]
fn final_index_past_the_piece_is_an_error() {
    for at in [u64::MAX - 10, u64::MAX - 40, 1 << 40, 8] {
        let bytes = close_only(at);
        let r = TapeReader::open(&bytes[..]).unwrap();
        assert_eq!(r.pieces().len(), 1, "final index at {at}");
        assert!(r.pieces()[0].close.is_some());
        assert!(r.entries(0).is_err(), "final index at {at}");
        assert!(
            r.track_entries(0, TrackId::new(1)).is_err(),
            "final index at {at}"
        );
    }
}

#[test]
fn entry_past_the_piece_is_an_error() {
    let bytes = close_only(0);
    let r = TapeReader::open(&bytes[..]).unwrap();
    let mut buf = Vec::new();
    for (offset, len) in [(u64::MAX - 10, 64), (u64::MAX, 1), (0, u32::MAX), (8, 400)] {
        let entry = IndexEntry {
            offset,
            len,
            kind: BlockKind::Series,
            level: 0,
            tree: 0,
            track_id: TrackId::new(1),
            number: 1,
            second: 0,
            first_item: ItemNum::new(0),
            count: 0,
            first_tick: 0,
            last_tick: 0,
            latest_end: 0,
            repeat: false,
            relation_break: false,
            grid_start: false,
            unhashed: false,
            unit: ww_tape::UnitHash::NONE,
            range: None,
            tile: None,
            key: None,
            deleted: false,
        };
        assert!(
            matches!(
                r.read_block(0, &entry, &mut buf),
                Err(ReadError::Block { .. })
            ),
            "entry at {offset}, {len} bytes"
        );
    }
}

#[test]
fn a_piece_number_past_the_pieces_is_an_error() {
    let bytes = close_only(0);
    let r = TapeReader::open(&bytes[..]).unwrap();
    let n = r.pieces().len();
    assert!(matches!(r.entries(n), Err(ReadError::NoPiece(i)) if i == n));
    assert!(matches!(
        r.track_entries(n, TrackId::new(0)),
        Err(ReadError::NoPiece(_))
    ));
    assert!(matches!(
        r.definitions_at(usize::MAX, 0),
        Err(ReadError::NoPiece(_))
    ));
    assert!(matches!(
        r.verify(n).map(|v| v.holds),
        Err(ReadError::NoPiece(_))
    ));
}

#[test]
fn an_entry_longer_than_any_block_is_refused_before_reading() {
    let bytes = close_only(0);
    let r = TapeReader::open(&bytes[..]).unwrap();
    let entry = IndexEntry {
        offset: 0,
        len: u32::MAX,
        kind: BlockKind::Series,
        level: 0,
        tree: 0,
        track_id: TrackId::new(1),
        number: 1,
        second: 0,
        first_item: ItemNum::new(0),
        count: 0,
        first_tick: 0,
        last_tick: 0,
        latest_end: 0,
        repeat: false,
        relation_break: false,
        grid_start: false,
        unhashed: false,
        unit: ww_tape::UnitHash::NONE,
        range: None,
        tile: None,
        key: None,
        deleted: false,
    };
    let mut buf = Vec::new();
    let e = r.read_block(0, &entry, &mut buf);
    assert!(matches!(
        e,
        Err(ReadError::Block {
            error: ww_tape::BlockError::BadLength(u32::MAX),
            ..
        })
    ));
    assert!(buf.capacity() < 1 << 20);
}

/// The block at `at` with its block CRC made good again.
fn reseal_block(bytes: &mut [u8], at: usize) {
    let len = ww_tape::block_len(&bytes[at..]).unwrap();
    let crc = crc32c(&[&bytes[at..at + len - 4]]);
    bytes[at + len - 4..at + len].copy_from_slice(&crc.to_le_bytes());
}

/// A close this build refuses (rows of another length, or flags it doesn't know), its CRC good, is kept as the
/// piece's `close_error`: the tail behind it ends no chain, so the chain is open, not whole.
#[test]
fn a_refused_close_leaves_the_chain_open() {
    let (_, files) = write(PieceTapes::One, 0x5eed);
    let good = TapeReader::open(&files[0][..]).unwrap();
    assert!(good.chain().is_whole());
    let i = good.pieces().len() - 1;
    let p = &good.pieces()[i];
    let fin = (p.start + p.close_offset.unwrap()) as usize;
    let rows = p.close.as_ref().unwrap().1.len();
    // the close's row length at +34, rows of 136 bytes from +176, each row's flags at +128
    let mut short_rows = files[0].clone();
    short_rows[fin + 34..fin + 36].copy_from_slice(&128u16.to_le_bytes());
    reseal_block(&mut short_rows, fin);
    let mut flags = files[0].clone();
    for k in 0..rows {
        let row = fin + 192 + k * 136;
        flags[row + 128..row + 132].copy_from_slice(&u32::MAX.to_le_bytes());
    }
    reseal_block(&mut flags, fin);
    for (name, bytes) in [("rows of 128", short_rows), ("flags", flags)] {
        let r = TapeReader::open(&bytes[..]).unwrap();
        assert_eq!(r.pieces().len(), good.pieces().len(), "{name}");
        let p = &r.pieces()[i];
        assert!(p.close.is_none(), "{name}");
        assert!(p.close_error.is_some(), "{name}");
        let c = r.chain();
        assert_eq!(c.end, ChainEnd::Unclosed, "{name}");
        assert!(!c.is_whole(), "{name}");
    }
}

/// A final index with two entries of a track swapped, its CRCs good, is refused by every path alike: the track's
/// list and the whole index, never sorted into a good order first.
#[test]
fn a_final_index_out_of_order_is_refused_by_every_path() {
    use ww_tape::index::{ENTRY_LEN, IndexError};
    let (_, files) = write(PieceTapes::One, 0x5eed);
    let good = TapeReader::open(&files[0][..]).unwrap();
    let i = 1;
    let t = (0..3)
        .find(|t| good.track_entries(i, id_of(*t)).unwrap().len() >= 2)
        .unwrap();
    let mut bytes = files[0].clone();
    let (mut first, mut second) = ([0u8; ENTRY_LEN], [0u8; ENTRY_LEN]);
    rewrite_final_entries(&mut bytes, i, t, |j, e| match j {
        0 => first.copy_from_slice(e),
        1 => second.copy_from_slice(e),
        _ => {}
    });
    rewrite_final_entries(&mut bytes, i, t, |j, e| match j {
        0 => e.copy_from_slice(&second),
        1 => e.copy_from_slice(&first),
        _ => {}
    });
    let r = TapeReader::open(&bytes[..]).unwrap();
    let bad = |e| matches!(e, Err(ReadError::Index(IndexError::BadEntries)));
    assert!(bad(r.track_entries(i, id_of(t)).map(|_| ())));
    assert!(bad(r.entries(i).map(|_| ())));
}

/// A piece head of an integrity level this build doesn't know (byte 146), its CRC good, is refused by a `FileBlocks`
/// walk with either recovery: reported and stepped over.
#[test]
fn file_blocks_refuse_a_piece_head_of_an_unknown_integrity_level() {
    use ww_tape::{BlockError, FileBlocks, FileError, Recovery};
    let (_, files) = write(PieceTapes::One, 0x5eed);
    let at = TapeReader::open(&files[0][..]).unwrap().pieces()[1].start as usize;
    for level in [3u8, 200] {
        let mut bytes = files[0].clone();
        bytes[at + 146] = level;
        reseal_block(&mut bytes, at);
        for recovery in [Recovery::Prefix, Recovery::Resync] {
            let mut fb = FileBlocks::with_recovery(&bytes[..], recovery);
            let mut errors = vec![];
            let mut heads = 0;
            while let Some(b) = fb.next_block() {
                match b {
                    Ok(b) => heads += usize::from(b.kind == BlockKind::PieceHead),
                    Err(e) => errors.push(e),
                }
            }
            let len = ww_tape::block_len(&bytes[at..]).unwrap() as u64;
            assert!(
                matches!(
                    errors.as_slice(),
                    [FileError::Refused { offset, error: BlockError::UnhandledIntegrity { level: l }, len: n }]
                        if *offset == at as u64 && *l == level && *n == len
                ),
                "level {level}, {recovery:?}: {errors:?}"
            );
            let all = TapeReader::open(&files[0][..]).unwrap().pieces().len();
            assert_eq!(heads, all - 1, "level {level}, {recovery:?}");
        }
    }
}

/// A damaged chunk whose payload holds a good copy, with other bytes, of the data block two after it: a Resync walk
/// hands out the copy, then the block between, then reports the real block as a conflict naming the copy, though the
/// block before it has another seq (R4-3). A copy with the same bytes is no conflict: both are handed out, as a Prefix
/// walk may read the second.
#[test]
fn resync_reports_a_seq_met_again_past_other_blocks_as_a_conflict() {
    use crate::definitions::base_defs;
    use crate::pieces::{chunk, column, records};
    use ww_tape::{Block, FileBlocks, FileError, MemSink, Recovery, SyncPolicy, TapeWriter};
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    let tape = w.sink().tape();
    let defs = base_defs();
    let (h, c) = chunk(1, 0);
    w.write_chunk(defs, &h, &column(&c)).unwrap();
    let next = w.next_id();
    w.write_block(defs, &records(next, 7)).unwrap();
    let next = w.next_id();
    w.write_block(defs, &records(next, 8)).unwrap();
    let (h, c) = chunk(1, 1);
    w.write_chunk(defs, &h, &column(&c)).unwrap();
    let clean = tape.to_vec().unwrap();
    drop(w);
    let mut blocks = vec![];
    let mut at = 0;
    while let Ok(b) = Block::read(&clean[at..]) {
        let data = b.flags & ww_tape::FLAG_REPEAT == 0;
        blocks.push((at, b.len(), b.kind, data, b.seq));
        at += b.len();
    }
    let c0 = blocks.iter().find(|b| b.2 == BlockKind::Events).unwrap().0;
    let data: Vec<_> = blocks
        .iter()
        .filter(|b| b.2 == BlockKind::Records && b.3 && b.0 > c0)
        .collect();
    let [&(a_at, _, _, _, a_seq), &(r_at, r_len, _, _, r_seq)] = data[..] else {
        panic!("two record blocks of data after the chunk: {blocks:?}")
    };
    assert!(a_seq < r_seq);
    let po = Block::read(&clean[c0..])
        .unwrap()
        .chunk()
        .unwrap()
        .payload_offset();
    let walk = |changed: bool| {
        let mut bytes = clean.clone();
        let mut fake = bytes[r_at..r_at + r_len].to_vec();
        let n = fake.len();
        if changed {
            fake[n - 10] ^= 0x55;
            let crc = crc32c(&[&fake[..n - 4]]);
            fake[n - 4..].copy_from_slice(&crc.to_le_bytes());
        }
        assert!(Block::read(&fake).is_ok());
        assert!(po + n <= a_at - c0, "the copy fits the chunk's payload");
        bytes[c0 + po..c0 + po + n].copy_from_slice(&fake);
        // the chunk's head CRC fails
        bytes[c0 + 40] ^= 1;
        let mut fb = FileBlocks::with_recovery(&bytes[..], Recovery::Resync);
        let mut seen = vec![];
        while let Some(b) = fb.next_block() {
            let got = b.map(|b| b.seq);
            seen.push(got.map(|seq| (fb.offset(), seq)));
        }
        seen
    };
    let fake_at = (c0 + po) as u64;
    let seen = walk(true);
    let taken = |o: usize| {
        seen.iter()
            .position(|s| matches!(s, Ok((at, _)) if *at == o as u64))
    };
    let (i, j) = (taken(c0 + po).unwrap(), taken(a_at).unwrap());
    assert!(i < j, "the copy, then the block between: {seen:?}");
    let conflicts: Vec<_> = seen
        .iter()
        .filter(|s| matches!(s, Err(FileError::Conflict { .. })))
        .collect();
    let [conflict] = conflicts[..] else {
        panic!("one conflict reported: {seen:?}")
    };
    assert!(
        matches!(
            conflict,
            Err(FileError::Conflict { first, offset, seq, block, .. })
                if *first == fake_at && *offset == r_at as u64 && *seq == r_seq
                    && block[..] == clean[r_at..r_at + r_len]
        ),
        "{seen:?}"
    );
    assert!(taken(r_at).is_none(), "{seen:?}");
    // a copy with the same bytes: no conflict, the real block is handed out too
    let seen = walk(false);
    assert!(
        !seen
            .iter()
            .any(|s| matches!(s, Err(FileError::Conflict { .. }))),
        "{seen:?}"
    );
    let taken = |o: usize| {
        seen.iter()
            .any(|s| matches!(s, Ok((at, _)) if *at == o as u64))
    };
    assert!(taken(c0 + po) && taken(r_at), "{seen:?}");
}

/// A damaged chunk whose payload holds a good copy of the next data block with other bytes: a Resync walk hands out
/// the copy at the magic it finds in the payload, then reports the real block, of the same session and seq, as a
/// conflict naming both, never as a block of its own: a salvage can't tell which one is real.
#[test]
fn resync_reports_two_blocks_of_one_seq_as_a_conflict() {
    use crate::definitions::base_defs;
    use crate::pieces::{chunk, column, records};
    use ww_tape::{Block, FileBlocks, FileError, MemSink, Recovery, SyncPolicy, TapeWriter};
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    let tape = w.sink().tape();
    let defs = base_defs();
    let (h, c) = chunk(1, 0);
    w.write_chunk(defs, &h, &column(&c)).unwrap();
    let next = w.next_id();
    w.write_block(defs, &records(next, 7)).unwrap();
    let (h, c) = chunk(1, 1);
    w.write_chunk(defs, &h, &column(&c)).unwrap();
    let mut bytes = tape.to_vec().unwrap();
    drop(w);
    let mut blocks = vec![];
    let mut at = 0;
    while let Ok(b) = Block::read(&bytes[at..]) {
        // the definitions put before the records repeat and take no part in the seq order
        let data = b.flags & ww_tape::FLAG_REPEAT == 0;
        blocks.push((at, b.len(), b.kind, data, b.seq));
        at += b.len();
    }
    let c0 = blocks.iter().find(|b| b.2 == BlockKind::Events).unwrap().0;
    let (r_at, r_len, _, _, r_seq) = *blocks
        .iter()
        .find(|b| b.2 == BlockKind::Records && b.3 && b.0 > c0)
        .unwrap();
    // a copy of the records block with one byte of its item changed, its CRC good, in the chunk's payload
    let mut fake = bytes[r_at..r_at + r_len].to_vec();
    let n = fake.len();
    fake[n - 10] ^= 0x55;
    let crc = crc32c(&[&fake[..n - 4]]);
    fake[n - 4..].copy_from_slice(&crc.to_le_bytes());
    assert!(Block::read(&fake).is_ok());
    let po = Block::read(&bytes[c0..])
        .unwrap()
        .chunk()
        .unwrap()
        .payload_offset();
    assert!(po + n <= r_at - c0, "the copy fits the chunk's payload");
    bytes[c0 + po..c0 + po + n].copy_from_slice(&fake);
    // the chunk's head CRC fails
    bytes[c0 + 40] ^= 1;
    let mut fb = FileBlocks::with_recovery(&bytes[..], Recovery::Resync);
    let mut seen = vec![];
    while let Some(b) = fb.next_block() {
        let got = b.map(|b| (b.seq, b.bytes() == &fake[..]));
        seen.push(got.map(|(seq, is_fake)| (fb.offset(), seq, is_fake)));
    }
    let fake_at = (c0 + po) as u64;
    let i = seen
        .iter()
        .position(|s| matches!(s, Ok((o, _, true)) if *o == fake_at))
        .unwrap_or_else(|| panic!("the copy is taken: {seen:?}"));
    let conflicts: Vec<usize> = (0..seen.len())
        .filter(|k| matches!(seen[*k], Err(FileError::Conflict { .. })))
        .collect();
    let [k] = conflicts[..] else {
        panic!("one conflict reported: {seen:?}")
    };
    assert!(k > i, "{seen:?}");
    // the conflict names both copies and carries the second; neither is taken for the other: the real block is no
    // block of its own
    assert!(
        matches!(
            &seen[k],
            Err(FileError::Conflict { first, offset, seq, block, .. })
                if *first == fake_at && *offset == r_at as u64 && *seq == r_seq
                    && block[..] == bytes[r_at..r_at + r_len]
        ),
        "{seen:?}"
    );
    assert!(
        !seen
            .iter()
            .any(|s| matches!(s, Ok((o, _, _)) if *o == r_at as u64)),
        "{seen:?}"
    );
}
