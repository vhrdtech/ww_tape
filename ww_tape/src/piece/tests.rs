use super::*;
use crate::ItemNum;
use crate::tests::id;

fn ident(seq: u64) -> PieceIdent {
    PieceIdent {
        id: PieceId([seq as u8 + 1; 16]),
        owner: OwnerId([0xaa; 32]),
        seq,
    }
}

#[test]
fn piece_head_round_trip() {
    let head = PieceHead {
        ident: ident(3),
        prev: Some((ContentHash([7; 32]), PieceId([3; 16]))),
        prev_session: crate::SessionTag::writable(0x1234_abcd),
        integrity: crate::Integrity::Full,
        unlisted_keys_unknown: false,
    };
    let tracks = [
        TrackStart {
            track_id: TrackId::new(11),
            track_number: TrackNum::new(1),
            next_item: ItemNum::new(8192),
            from_tick: -5,
            owner: OwnerId([4; 32]),
            hash: TrackHash([5; 32]),
            incomplete: true,
            not_carried: true,
            keys_carried: true,
        },
        TrackStart {
            track_id: TrackId::new(12),
            track_number: TrackNum::new(2),
            next_item: ItemNum::new(0),
            from_tick: i64::MIN,
            owner: OwnerId::NONE,
            hash: TrackHash::START,
            incomplete: false,
            not_carried: false,
            keys_carried: false,
        },
    ];
    let mut buf = vec![0u8; piece_head_len(2)];
    let n = write_piece_head(&mut buf, id(40), &head, &tracks).unwrap();
    assert_eq!(n, 32 + 120 + 2 * 96 + 4);
    let block = Block::read(&buf).unwrap();
    assert_eq!((block.kind, block.seq), (BlockKind::PieceHead, 40));
    let h = PieceHeadBlock::read(&block).unwrap();
    assert_eq!(h.head(), head);
    assert_eq!(h.tracks().collect::<Vec<_>>(), tracks);
    // the first row's flags carry its incomplete, not-carried and keys-carried marks; a flag this build doesn't know is refused, its CRC good
    let at = HEAD_TRACKS_AT + HEAD_ROW_FLAGS_AT;
    assert_eq!(
        buf[at..at + 4],
        (ROW_INCOMPLETE | ROW_NOT_CARRIED | ROW_KEYS_CARRIED).to_le_bytes()
    );
    for flag in [8u32, 1 << 31] {
        let mut b = buf[..n].to_vec();
        b[at..at + 4].copy_from_slice(&(flag | ROW_INCOMPLETE).to_le_bytes());
        crate::index::seal(&mut b);
        assert_eq!(
            PieceHeadBlock::read(&Block::read(&b).unwrap()).err(),
            Some(IndexError::Invalid(
                "a track row flag this build doesn't know"
            ))
        );
    }

    let first = PieceHead {
        ident: ident(0),
        prev: None,
        prev_session: crate::SessionTag::NONE,
        integrity: crate::Integrity::Full,
        unlisted_keys_unknown: false,
    };
    let n = write_piece_head(&mut buf, id(0), &first, &[]).unwrap();
    let block = Block::read(&buf[..n]).unwrap();
    assert_eq!(PieceHeadBlock::read(&block).unwrap().head(), first);
}

#[test]
fn close_and_tail_round_trip() {
    let close = Close {
        reason: CloseReason::Closed,
        hash: ContentHash([9; 32]),
        checkpoint: Some(4 << 20),
        final_index: Some(5 << 20),
        prev: Some(3 << 20),
        summary: Some((4 << 20, 4096)),
        ident: Some(ident(1)),
        next: Some(PieceId([0x55; 16])),
        unlisted_keys_unknown: false,
    };
    let tracks = [TrackRange {
        track_id: TrackId::new(11),
        track_number: TrackNum::new(1),
        chunks: 2,
        first_item: ItemNum::new(8192),
        items: 8000,
        first_tick: 100,
        last_tick: 900,
        latest_end: 950,
        owner: OwnerId([6; 32]),
        hash: TrackHash([8; 32]),
        records: 3,
        incomplete: true,
    }];
    let mut buf = vec![0u8; close_len(1, 0) + 4095];
    for padding in [0, 1, 1000, 4095] {
        let n = write_close(&mut buf, id(77), &close, (&tracks, &[]), padding).unwrap();
        assert_eq!(n, close_len(1, 0) + padding);
        let block = Block::read(&buf[..n]).unwrap();
        let f = CloseBlock::read(&block).unwrap();
        assert_eq!(f.close(), close);
        assert_eq!(f.tracks().collect::<Vec<_>>(), tracks);
    }
    assert_eq!(tracks[0].end_item(), ItemNum::new(16192));
    assert!(write_close(&mut buf, id(77), &close, (&tracks, &[]), 4096).is_err());

    let plain = Close {
        ident: None,
        next: None,
        reason: CloseReason::Closed,
        ..close
    };
    let n = write_close(&mut buf, id(77), &plain, (&[], &[]), 0).unwrap();
    let block = Block::read(&buf[..n]).unwrap();
    assert_eq!(CloseBlock::read(&block).unwrap().close(), plain);

    // an aborted piece goes on in no next one: such a close record, its CRC good, is refused
    let aborted = Close {
        reason: CloseReason::Aborted,
        ..close
    };
    let n = write_close(&mut buf, id(77), &aborted, (&tracks, &[]), 0).unwrap();
    assert_eq!(
        CloseBlock::read(&Block::read(&buf[..n]).unwrap()).err(),
        Some(IndexError::BadHead)
    );
    let aborted = Close {
        next: None,
        ..aborted
    };
    let n = write_close(&mut buf, id(77), &aborted, (&tracks, &[]), 0).unwrap();
    let block = Block::read(&buf[..n]).unwrap();
    assert_eq!(CloseBlock::read(&block).unwrap().close(), aborted);

    // a track row flag this build doesn't know is refused, its CRC good
    let n = write_close(&mut buf, id(77), &close, (&tracks, &[]), 0).unwrap();
    for flag in [2u32, 1 << 31] {
        let mut b = buf[..n].to_vec();
        let at = CLOSE_TRACKS_AT + ROW_FLAGS_AT;
        b[at..at + 4].copy_from_slice(&(flag | ROW_INCOMPLETE).to_le_bytes());
        crate::index::seal(&mut b);
        assert_eq!(
            CloseBlock::read(&Block::read(&b).unwrap()).err(),
            Some(IndexError::Invalid(
                "a track row flag this build doesn't know"
            ))
        );
    }

    let tail = Tail {
        piece_len: 6 << 20,
        close_offset: (6 << 20) - 400,
        close_len: 200,
        reason: CloseReason::Closed,
        hash: close.hash,
        ident: close.ident,
        next: close.next,
    };
    let mut tb = [0u8; TAIL_LEN];
    assert_eq!(tail.write(&mut tb, id(78)), TAIL_LEN);
    assert_eq!(Tail::read(&tb).unwrap(), tail);
    for at in 0..TAIL_LEN {
        let mut bad = tb;
        bad[at] ^= 1;
        assert!(Tail::read(&bad).is_err(), "byte {at}");
    }
    // a close that doesn't fit before the tail
    let wrong = Tail {
        close_offset: 6 << 20,
        ..tail
    };
    wrong.write(&mut tb, id(78));
    assert!(Tail::read(&tb).is_err());
    // offsets whose sum overflows u64
    let wraps = Tail {
        piece_len: TAIL_LEN as u64,
        close_offset: u64::MAX - 343,
        close_len: 180,
        ..tail
    };
    wraps.write(&mut tb, id(78));
    assert_eq!(Tail::read(&tb), Err(IndexError::BadHead));
}

#[test]
fn piece_ids_follow_the_chain() {
    let owner = OwnerId([1; 32]);
    let a = PieceId::first(&owner);
    assert_eq!(a, PieceId::first(&owner));
    assert_ne!(a, PieceId::first(&OwnerId([2; 32])));
    let b = PieceId::after(a, &ContentHash([3; 32]));
    assert_ne!(b, a);
    assert_ne!(b, PieceId::after(a, &ContentHash([4; 32])));
    assert!(!a.is_none() && !b.is_none());
}

#[test]
fn chains_of_one_result_start_apart() {
    let owner = OwnerId([1; 32]);
    assert_eq!(PieceId::first(&owner), PieceId::first_of(&owner, 0));
    let firsts: Vec<PieceId> = (0..4).map(|c| PieceId::first_of(&owner, c)).collect();
    for (i, a) in firsts.iter().enumerate() {
        assert_eq!(
            *a,
            PieceId::first_of(&owner, i as u64),
            "the same chain, the same id"
        );
        assert!(firsts[i + 1..].iter().all(|b| b != a), "chain {i}");
    }
    assert_ne!(
        PieceId::first_of(&owner, 1),
        PieceId::first_of(&OwnerId([2; 32]), 1)
    );
    // pinned: a chain's first id is part of the format
    assert_eq!(PieceId::first_of(&owner, 1).0[..4], [241, 163, 28, 235]);
}

/// How a test piece ends: closed, closed by a cut naming the next piece, or aborted.
#[derive(Copy, Clone, PartialEq)]
enum Ends {
    Complete,
    Continued,
    Aborted,
}

fn summary(seq: u64, end: Option<Ends>) -> PieceSummary {
    let id = |s: u64| PieceId([s as u8 + 1; 16]);
    let hash = |s: u64| ContentHash([s as u8 + 100; 32]);
    PieceSummary {
        ident: PieceIdent {
            id: id(seq),
            owner: OwnerId([0xaa; 32]),
            seq,
        },
        prev: (seq > 0).then(|| (hash(seq - 1), id(seq - 1))),
        end: end.map(|st| {
            let reason = match st {
                Ends::Aborted => CloseReason::Aborted,
                Ends::Complete | Ends::Continued => CloseReason::Closed,
            };
            (
                reason,
                hash(seq),
                (st == Ends::Continued).then(|| id(seq + 1)),
            )
        }),
        unclosed: false,
        head: HeadStatus::Read,
        removed: Vec::new(),
        integrity: Some(Integrity::Full),
    }
}

#[test]
fn chains_are_checked_both_ways() {
    use Ends::*;
    let whole = [
        summary(2, Some(Complete)),
        summary(0, Some(Continued)),
        summary(1, Some(Continued)),
    ];
    let r = check_chain(&whole);
    assert_eq!(r.order, [1, 2, 0]);
    assert!(r.is_whole());

    // the first piece deleted: head cut, not damage
    let r = check_chain(
        &whole[..1]
            .iter()
            .chain(&whole[2..])
            .cloned()
            .collect::<Vec<_>>(),
    );
    assert!(r.head_cut);
    assert_eq!(r.links, [ChainLink::Linked]);
    assert_eq!(
        r.end,
        ChainEnd::Closed {
            reason: CloseReason::Closed
        }
    );

    // a gap, an open end
    let r = check_chain(&[summary(0, Some(Continued)), summary(2, None)]);
    assert_eq!(r.links, [ChainLink::Gap { missing: 1 }]);
    assert_eq!(r.end, ChainEnd::Unclosed);

    // the last piece at hand says more follow
    let r = check_chain(&[summary(0, Some(Continued))]);
    assert_eq!(r.end, ChainEnd::TailCut);

    // the reason of the last close is said, not obeyed
    let r = check_chain(&[summary(0, Some(Aborted))]);
    assert_eq!(
        r.end,
        ChainEnd::Closed {
            reason: CloseReason::Aborted
        }
    );

    // a piece rewritten: its hash no longer matches the next head
    let mut changed = summary(0, Some(Continued));
    changed.end = Some((
        CloseReason::Closed,
        ContentHash([0; 32]),
        changed.end.unwrap().2,
    ));
    let r = check_chain(&[changed, summary(1, Some(Complete))]);
    assert_eq!(r.links, [ChainLink::Broken]);
    assert!(!r.is_whole());

    // a crash left the first piece unclosed
    let r = check_chain(&[summary(0, None), summary(1, Some(Complete))]);
    assert_eq!(r.links, [ChainLink::Unclosed]);

    // the same seq twice, another owner
    let mut other = summary(1, Some(Complete));
    other.ident.owner = OwnerId([1; 32]);
    let r = check_chain(&[
        summary(0, Some(Continued)),
        summary(0, Some(Continued)),
        other,
    ]);
    assert_eq!(r.order, [0]);
    assert_eq!(r.strays, [1, 2]);

    // a head lost: linked one way by the earlier end; a first piece's head lost is head cut past seq 0
    let lost = |seq, end| PieceSummary {
        prev: None,
        head: HeadStatus::Lost,
        ..summary(seq, end)
    };
    let r = check_chain(&[
        summary(0, Some(Continued)),
        lost(1, Some(Continued)),
        summary(2, Some(Complete)),
    ]);
    assert_eq!(r.links, [ChainLink::HeadLost, ChainLink::Linked]);
    assert!(!r.is_whole() && !r.head_cut);
    assert!(check_chain(&[lost(1, Some(Complete))]).head_cut);
    assert!(!check_chain(&[lost(0, Some(Complete))]).head_cut);
}

#[test]
fn close_padding_ends_the_piece_on_4_kib() {
    for (tracks, removed) in [(0, 0), (1, 0), (3, 2), (70, 9)] {
        for offset in [0, 1, 100, 4095, 4096, 12345, (1 << 40) + 7, u64::MAX - 5] {
            let pad = close_padding(offset, tracks, removed);
            assert!(pad < 4096);
            let end = offset.wrapping_add((close_len(tracks, removed) + pad + TAIL_LEN) as u64);
            assert_eq!(end % 4096, 0, "{tracks} tracks at {offset}");
        }
    }
}

#[test]
fn close_padding_is_checked_against_the_length() {
    let close = Close {
        reason: CloseReason::Closed,
        hash: ContentHash([3; 32]),
        checkpoint: None,
        final_index: None,
        prev: None,
        summary: None,
        ident: None,
        next: None,
        unlisted_keys_unknown: false,
    };
    let mut buf = vec![0u8; close_len(0, 0) + 64];
    let n = write_close(&mut buf, id(5), &close, (&[], &[]), 64).unwrap();
    // a padding field that disagrees with the block's length, CRC made good again
    for padding in [0u32, 63, 65, 4095, 4096, u32::MAX] {
        let mut b = buf[..n].to_vec();
        b[172..176].copy_from_slice(&padding.to_le_bytes());
        crate::index::seal(&mut b);
        let block = Block::read(&b).unwrap();
        assert_eq!(
            CloseBlock::read(&block).err(),
            Some(IndexError::BadHead),
            "padding {padding}"
        );
    }
}

/// The integrity byte (146) is read through [Integrity]: each level writes and reads back, a byte no level has is
/// refused as unknown, not as damage.
#[test]
fn a_piece_head_reads_each_integrity_level() {
    let mut head = PieceHead {
        ident: ident(0),
        prev: None,
        prev_session: crate::SessionTag::NONE,
        integrity: Integrity::Full,
        unlisted_keys_unknown: false,
    };
    let mut buf = vec![0u8; piece_head_len(0)];
    let n = write_piece_head(&mut buf, id(0), &head, &[]).unwrap();
    assert_eq!(buf[146], u8::from(Integrity::Full));
    let block = Block::read(&buf[..n]).unwrap();
    assert_eq!(PieceHeadBlock::read(&block).unwrap().head(), head);
    for level in [Integrity::CrcOnly, Integrity::None] {
        head.integrity = level;
        let n = write_piece_head(&mut buf, id(0), &head, &[]).unwrap();
        assert_eq!(buf[146], u8::from(level));
        let block = Block::read(&buf[..n]).unwrap();
        assert_eq!(PieceHeadBlock::read(&block).unwrap().head(), head);
    }
    for level in [3u8, 255] {
        let mut b = buf[..n].to_vec();
        b[146] = level;
        let crc = crate::CRC.checksum(&b[..n - 4]);
        b[n - 4..].copy_from_slice(&crc.to_le_bytes());
        let block = Block::read(&b).expect("a good block");
        assert_eq!(
            PieceHeadBlock::read(&block).err(),
            Some(IndexError::UnknownIntegrity { level })
        );
    }
}

/// A content hash over a stretch of crafted headers, a block magic every 32 bytes and each header's length reaching
/// to the end, reads a bounded multiple of the stretch, not its square.
#[test]
fn a_content_hash_over_crafted_headers_reads_linear_bytes() {
    let n: usize = 256 * 1024;
    let mut buf = vec![0u8; n];
    for k in (0..n).step_by(32) {
        buf[k..k + 4].copy_from_slice(&crate::MAGIC);
        buf[k + 4] = 1;
        buf[k + 5] = 1;
        let len = u32::try_from(n - k).unwrap();
        buf[k + 8..k + 12].copy_from_slice(&len.to_le_bytes());
    }
    let mut read = 0u64;
    let mut h = blake3::Hasher::new();
    hash_piece::<()>(&mut h, len_u64(n), |at, out| {
        read += len_u64(out.len());
        let at = usize::try_from(at).unwrap();
        out.copy_from_slice(&buf[at..at + out.len()]);
        Ok(())
    })
    .unwrap();
    let n = len_u64(n);
    // the bytes once, a header per magic, the misses' budget and the one miss past it
    let bound = n + n + WALK_MISS_PER_BYTE_MAX * n + WALK_MISS_BASE_MAX + n;
    assert!(read <= bound, "{read} bytes read over {n}, at most {bound}");
    let mut whole = blake3::Hasher::new();
    whole.update(&buf);
    assert_eq!(
        h.finalize(),
        whole.finalize(),
        "no block reads whole: the bytes as they are"
    );
}

/// The removed piece of sequence number `seq` of the chain [summary] makes.
fn removed_row(seq: u64) -> RemovedPiece {
    let s = summary(seq, None);
    RemovedPiece {
        seq,
        id: s.ident.id,
        hash: ContentHash([seq as u8 + 100; 32]),
    }
}

fn plain_close() -> Close {
    Close {
        reason: CloseReason::Closed,
        hash: ContentHash([3; 32]),
        checkpoint: None,
        final_index: None,
        prev: None,
        summary: None,
        ident: Some(ident(9)),
        next: None,
        unlisted_keys_unknown: false,
    }
}

#[test]
fn removed_pieces_round_trip_in_the_close() {
    let removed = [removed_row(1), removed_row(2), removed_row(5)];
    let tracks = [TrackRange {
        track_id: TrackId::new(1),
        track_number: TrackNum::new(1),
        chunks: 0,
        first_item: ItemNum::new(0),
        items: 0,
        first_tick: 0,
        last_tick: 0,
        latest_end: 0,
        owner: OwnerId::NONE,
        hash: TrackHash([1; 32]),
        records: 1,
        incomplete: false,
    }];
    let mut buf = vec![0u8; close_len(1, 3) + 4095];
    for padding in [0, 7, 4095] {
        let n = write_close(
            &mut buf,
            id(5),
            &plain_close(),
            (&tracks, &removed),
            padding,
        )
        .unwrap();
        assert_eq!(n, close_len(1, 3) + padding);
        let block = Block::read(&buf[..n]).unwrap();
        let c = CloseBlock::read(&block).unwrap();
        assert_eq!(c.tracks().collect::<Vec<_>>(), tracks);
        assert_eq!(c.removed().collect::<Vec<_>>(), removed);
        assert_eq!(c.close(), plain_close());
    }
    // the writer refuses rows out of order or named twice, and more than the cap
    for rows in [
        vec![removed_row(2), removed_row(1)],
        vec![removed_row(2), removed_row(2)],
    ] {
        assert!(write_close(&mut buf, id(5), &plain_close(), (&tracks, &rows), 0).is_err());
    }
    let many = vec![removed_row(1); REMOVED_PIECES_MAX + 1];
    let mut big = vec![0u8; close_len(1, many.len())];
    assert!(write_close(&mut big, id(5), &plain_close(), (&tracks, &many), 0).is_err());
}

#[test]
fn a_close_with_bad_removed_rows_is_refused() {
    let removed = [removed_row(1), removed_row(2)];
    let mut buf = vec![0u8; close_len(0, 2)];
    let n = write_close(&mut buf, id(5), &plain_close(), (&[], &removed), 0).unwrap();
    let read = |b: &[u8]| CloseBlock::read(&Block::read(b).unwrap()).map(|c| c.removed().count());
    let edit = |at: usize, v: &[u8]| {
        let mut b = buf[..n].to_vec();
        b[at..at + v.len()].copy_from_slice(v);
        crate::index::seal(&mut b);
        b
    };
    let rows_at = CLOSE_TRACKS_AT;
    // out of order, named twice
    for seq in [3u64, 2] {
        assert_eq!(
            read(&edit(rows_at, &seq.to_le_bytes())),
            Err(IndexError::Invalid(
                "removed pieces not in ascending order of distinct seqs"
            ))
        );
    }
    // a row length below the one this build writes, a count the length doesn't hold, a count past the cap
    assert_eq!(
        read(&edit(REMOVED_ROW_LEN_AT, &55u16.to_le_bytes())),
        Err(IndexError::BadHead)
    );
    assert_eq!(
        read(&edit(REMOVED_COUNT_AT, &3u32.to_le_bytes())),
        Err(IndexError::BadHead)
    );
    assert_eq!(
        read(&edit(REMOVED_COUNT_AT, &1u32.to_le_bytes())),
        Err(IndexError::BadHead)
    );
    let past = u32::try_from(REMOVED_PIECES_MAX + 1).unwrap();
    assert_eq!(
        read(&edit(REMOVED_COUNT_AT, &past.to_le_bytes())),
        Err(IndexError::Invalid(
            "more removed pieces than REMOVED_PIECES_MAX"
        ))
    );
    assert_eq!(read(&buf[..n]), Ok(2));

    // a close without removed pieces and with no row length, as closes written before rows existed: read, no rows
    let mut none = vec![0u8; close_len(0, 0)];
    let k = write_close(&mut none, id(5), &plain_close(), (&[], &[]), 0).unwrap();
    none[REMOVED_ROW_LEN_AT..REMOVED_ROW_LEN_AT + 2].copy_from_slice(&0u16.to_le_bytes());
    crate::index::seal(&mut none[..k]);
    assert_eq!(read(&none[..k]), Ok(0));
    // ... but a row length of 0 with rows is refused
    assert_eq!(
        read(&edit(REMOVED_ROW_LEN_AT, &0u16.to_le_bytes())),
        Err(IndexError::BadHead)
    );
}

/// A later writer's longer removed rows: a reader takes their first 56 bytes.
#[test]
fn longer_removed_rows_read_their_first_bytes() {
    let removed = [removed_row(1), removed_row(4)];
    let mut buf = vec![0u8; close_len(0, 2)];
    let n = write_close(&mut buf, id(5), &plain_close(), (&[], &removed), 0).unwrap();
    let rows_at = CLOSE_TRACKS_AT;
    let mut b = buf[..rows_at].to_vec();
    for row in buf[rows_at..n - crate::CRC_LEN].chunks(REMOVED_ROW_LEN) {
        b.extend_from_slice(row);
        b.extend_from_slice(&[0xee; 8]);
    }
    b.extend_from_slice(&[0; 4]);
    let len = u32::try_from(b.len()).unwrap();
    b[8..12].copy_from_slice(&len.to_le_bytes());
    b[REMOVED_ROW_LEN_AT..REMOVED_ROW_LEN_AT + 2].copy_from_slice(&64u16.to_le_bytes());
    crate::index::seal(&mut b);
    let block = Block::read(&b).unwrap();
    let c = CloseBlock::read(&block).unwrap();
    assert_eq!(c.removed().collect::<Vec<_>>(), removed);
}

#[test]
fn chains_link_over_removed_pieces() {
    use Ends::*;
    let with = |mut s: PieceSummary, rows: &[u64]| {
        s.removed = rows.iter().map(|q| removed_row(*q)).collect();
        s
    };
    // pieces 1 and 2 removed, named by the last piece's close
    let r = check_chain(&[
        summary(0, Some(Continued)),
        with(summary(3, Some(Complete)), &[1, 2]),
    ]);
    assert_eq!(r.links, [ChainLink::Removed { pieces: 2 }]);
    assert!(r.is_whole());
    assert_eq!(r.removed, [removed_row(1), removed_row(2)]);

    // named by an earlier piece's close: the same
    let r = check_chain(&[
        summary(0, Some(Continued)),
        with(summary(2, Some(Continued)), &[1]),
        summary(3, Some(Complete)),
    ]);
    assert_eq!(
        r.links,
        [ChainLink::Removed { pieces: 1 }, ChainLink::Linked]
    );
    assert!(r.is_whole());

    // one of them not named: a gap
    let r = check_chain(&[
        summary(0, Some(Continued)),
        with(summary(3, Some(Complete)), &[2]),
    ]);
    assert_eq!(r.links, [ChainLink::Gap { missing: 2 }]);
    assert!(!r.is_whole());

    // a removed piece whose hash isn't what the next head names: broken
    let mut forged = with(summary(3, Some(Complete)), &[1, 2]);
    forged.removed[1].hash = ContentHash([0; 32]);
    let r = check_chain(&[summary(0, Some(Continued)), forged]);
    assert_eq!(r.links, [ChainLink::Broken]);
    // ... or whose id isn't what the piece before names
    let mut forged = with(summary(3, Some(Complete)), &[1, 2]);
    forged.removed[0].id = PieceId([0x77; 16]);
    let r = check_chain(&[summary(0, Some(Continued)), forged]);
    assert_eq!(r.links, [ChainLink::Broken]);

    // two closes naming one piece differently: left out, a gap
    let mut other = with(summary(5, Some(Complete)), &[1]);
    other.removed[0].hash = ContentHash([1; 32]);
    let r = check_chain(&[
        with(summary(0, Some(Continued)), &[]),
        summary(3, Some(Continued)),
        with(summary(4, Some(Continued)), &[1, 2]),
        other,
    ]);
    assert_eq!(r.links[0], ChainLink::Gap { missing: 2 });
    assert_eq!(r.removed, [removed_row(2)]);

    // a row naming a piece still at hand: no part in a link, not listed
    let r = check_chain(&[
        summary(0, Some(Continued)),
        summary(1, Some(Continued)),
        with(summary(2, Some(Complete)), &[1]),
    ]);
    assert_eq!(r.links, [ChainLink::Linked, ChainLink::Linked]);
    assert!(r.removed.is_empty(), "{:?}", r.removed);

    // the earlier piece unclosed: unclosed, as without the removal
    let r = check_chain(&[summary(0, None), with(summary(2, Some(Complete)), &[1])]);
    assert_eq!(r.links, [ChainLink::Unclosed]);

    // the later piece's head lost: linked one way
    let lost = PieceSummary {
        prev: None,
        head: HeadStatus::Lost,
        ..with(summary(2, Some(Complete)), &[1])
    };
    let r = check_chain(&[summary(0, Some(Continued)), lost]);
    assert_eq!(r.links, [ChainLink::HeadLost]);
}

#[test]
fn a_head_cut_the_closes_name_reads_as_removed() {
    use Ends::*;
    let with = |mut s: PieceSummary, rows: &[u64]| {
        s.removed = rows.iter().map(|q| removed_row(*q)).collect();
        s
    };
    let r = check_chain(&[with(summary(3, Some(Complete)), &[0, 1, 2])]);
    assert!(!r.head_cut);
    assert_eq!(r.head_removed, 3);
    assert!(r.is_whole());

    // only some of the pieces before named: head cut
    let r = check_chain(&[with(summary(3, Some(Complete)), &[1, 2])]);
    assert!(r.head_cut);
    assert_eq!(r.head_removed, 0);
    assert!(!r.is_whole());

    // the last named piece's hash isn't what the head names: head cut
    let mut forged = with(summary(3, Some(Complete)), &[0, 1, 2]);
    forged.removed[2].hash = ContentHash([0; 32]);
    let r = check_chain(&[forged]);
    assert!(r.head_cut);
    assert_eq!(r.head_removed, 0);
}

/// A piece below Integrity::Full has no content hash: the next head names it by id with a zero hash, the link is
/// by id alone (Unproven), whole but not proven; another id is broken, and the zero hash is never compared.
#[test]
fn a_link_from_a_piece_without_a_content_hash_is_by_id() {
    use Ends::*;
    let unhashed = |seq, end| {
        let mut s = summary(seq, end);
        s.integrity = Some(Integrity::CrcOnly);
        if let Some(e) = &mut s.end {
            e.1 = ContentHash::default();
        }
        s
    };
    let mut second = summary(1, Some(Complete));
    second.prev = second.prev.map(|(_, id)| (ContentHash::default(), id));
    let r = check_chain(&[unhashed(0, Some(Continued)), second.clone()]);
    assert_eq!(r.links, [ChainLink::Unproven]);
    assert!(r.is_whole());
    assert!(!r.is_proven());
    // the head names some hash: still by id, the hash absent
    let r = check_chain(&[unhashed(0, Some(Continued)), summary(1, Some(Complete))]);
    assert_eq!(r.links, [ChainLink::Unproven]);
    // another id: broken
    second.prev = Some((ContentHash::default(), PieceId([0x55; 16])));
    let r = check_chain(&[unhashed(0, Some(Continued)), second]);
    assert_eq!(r.links, [ChainLink::Broken]);
    // pieces of Full: proven
    let r = check_chain(&[summary(0, Some(Continued)), summary(1, Some(Complete))]);
    assert_eq!(r.links, [ChainLink::Linked]);
    assert!(r.is_proven());
}
