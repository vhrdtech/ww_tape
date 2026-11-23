use super::*;
use crate::ClockNum;
use crate::ItemNum;
use crate::tests::id;
use crate::{BlockWriter, ClockTime, EndMode};

fn entry(track_id: u64, offset: u64) -> IndexEntry {
    IndexEntry {
        offset,
        // blocks of 64 bytes, a writer's own entries spaced at least that far apart
        len: 64,
        kind: BlockKind::Events,
        level: 0,
        tree: 0,
        track_id: TrackId::new(track_id),
        number: track_id as u32 + 1,
        second: 0,
        first_item: ItemNum::new(offset * 10),
        count: 10,
        first_tick: -(offset as i64),
        last_tick: offset as i64,
        latest_end: offset as i64 + 3,
        repeat: offset % 2 == 1,
        relation_break: false,
        unit: crate::piece::UnitHash([offset.to_le_bytes()[0]; 32]),
        unhashed: false,
        // every third entry a chunk with a range relation, (0, 0) and values past i64 among them
        range: match offset % 3 {
            0 => None,
            1 => Some((0, 0)),
            _ => Some((offset, u64::MAX - offset)),
        },
        tile: None,
    }
}

fn write(head: &IndexHead, entries: &[IndexEntry]) -> Vec<u8> {
    let tracks = match head.kind {
        IndexKind::Checkpoint => 0,
        IndexKind::Final => track_count(entries),
    };
    let mut buf = vec![0u8; index_len(entries.len(), tracks)];
    let n = write_index(&mut buf, id(5), head, entries).unwrap();
    assert_eq!(n, buf.len());
    buf
}

/// An index of no entries is shorter than one entry: the entry length is bounded by the format's cap, not the block.
#[test]
fn empty_index_round_trip() {
    for kind in [IndexKind::Checkpoint, IndexKind::Final] {
        let head = IndexHead {
            kind,
            self_offset: 10,
            prev: Some(7),
        };
        let bytes = write(&head, &[]);
        assert!(bytes.len() < ENTRY_LEN, "{kind:?}: {} bytes", bytes.len());
        let ix = IndexBlock::read(&Block::read(&bytes).unwrap()).unwrap();
        assert_eq!((ix.kind(), ix.entry_count(), ix.prev()), (kind, 0, Some(7)));
    }
}

#[test]
fn checkpoint_round_trip() {
    let entries: Vec<_> = (0..5).map(|i| entry(i % 2, 1000 + i * 300)).collect();
    let head = IndexHead {
        kind: IndexKind::Checkpoint,
        self_offset: 2500,
        prev: Some(400),
    };
    let bytes = write(&head, &entries);
    let block = Block::read(&bytes).unwrap();
    assert_eq!(
        (block.kind, block.seq, block.record_count),
        (BlockKind::Index, 5, 5)
    );
    let ix = IndexBlock::read(&block).unwrap();
    assert_eq!(ix.kind(), IndexKind::Checkpoint);
    assert_eq!((ix.self_offset(), ix.prev()), (2500, Some(400)));
    assert_eq!(ix.entries().collect::<Vec<_>>(), entries);
    assert_eq!(ix.tracks().count(), 0);
    assert_eq!(bytes.len(), 32 + 28 + 5 * ENTRY_LEN + 4);
    let first = IndexHead { prev: None, ..head };
    let ix_bytes = write(&first, &entries);
    assert_eq!(
        IndexBlock::read(&Block::read(&ix_bytes).unwrap())
            .unwrap()
            .prev(),
        None
    );
}

/// A reader of one track reads the head and that track's entries alone, each checked by its CRC.
#[test]
fn final_index_reads_one_track_from_its_head() {
    let mut entries: Vec<_> = (0..40)
        .map(|i| entry([0, 3, 7, 9][i as usize % 4], i * 64))
        .collect();
    entries.sort_by_key(|e| (e.track_id, e.offset));
    let head = IndexHead {
        kind: IndexKind::Final,
        self_offset: 9000,
        prev: None,
    };
    let bytes = write(&head, &entries);
    let ix = IndexBlock::read(&Block::read(&bytes).unwrap()).unwrap();
    assert_eq!(
        ix.tracks().map(|t| t.track_id.get()).collect::<Vec<_>>(),
        [0, 3, 7, 9]
    );

    let head_len = IndexBlock::head_len(&bytes[..HEAD_LEN_PREFIX]).unwrap();
    assert_eq!(head_len, 32 + 28 + 4 * 24);
    let only_head = IndexBlock::read_head(&bytes[..head_len]).unwrap();
    assert_eq!(only_head.entries().count(), 0, "entries aren't in the head");
    let list = only_head.track(TrackId::new(7)).unwrap();
    assert_eq!((list.first, list.count), (20, 10));
    let (from, n) = only_head.track_range(&list);
    let got: Vec<_> = only_head
        .track_entries(&list, &bytes[from..from + n])
        .unwrap()
        .collect();
    assert_eq!(got, entries[20..30]);
    assert!(only_head.track(TrackId::new(4)).is_none());

    let mut bad = bytes[from..from + n].to_vec();
    bad[3] ^= 1;
    assert!(matches!(
        only_head.track_entries(&list, &bad),
        Err(IndexError::BadEntries)
    ));
    let mut head_bad = bytes[..head_len].to_vec();
    head_bad[60] ^= 1;
    assert_eq!(
        IndexBlock::read_head(&head_bad).unwrap_err(),
        IndexError::BadHeadCrc
    );
}

#[test]
fn final_index_must_be_sorted() {
    let entries = [entry(3, 0), entry(1, 64)];
    let head = IndexHead {
        kind: IndexKind::Final,
        self_offset: 0,
        prev: None,
    };
    let mut buf = vec![0u8; 4096];
    assert!(matches!(
        write_index(&mut buf, id(0), &head, &entries),
        Err(IndexError::Invalid(_))
    ));
    let short = &mut [0u8; 40][..];
    assert!(matches!(
        write_index(short, id(0), &head, &[]),
        Err(IndexError::Full { .. })
    ));
}

/// A final index whose track lists overlapping blocks is refused when written, as every reader refuses it.
#[test]
fn final_index_refuses_overlapping_blocks() {
    let head = IndexHead {
        kind: IndexKind::Final,
        self_offset: 0,
        prev: None,
    };
    let mut buf = vec![0u8; 4096];
    // a block of 100 bytes at 0 and one at 64 of the same track overlap
    let overlap = [
        IndexEntry {
            len: 100,
            ..entry(1, 0)
        },
        entry(1, 64),
    ];
    assert_eq!(
        write_index(&mut buf, id(0), &head, &overlap),
        Err(IndexError::Invalid("a track's blocks overlap"))
    );
    // blocks of different tracks are listed apart, each track in file order
    let n = write_index(&mut buf, id(0), &head, &[entry(1, 0), entry(2, 64)]).unwrap();
    let ix = IndexBlock::read(&Block::read(&buf[..n]).unwrap()).unwrap();
    for t in ix.tracks() {
        let (from, len) = ix.track_range(&t);
        assert!(ix.track_entries(&t, &buf[from..from + len]).is_ok());
    }
}

#[test]
fn entries_of_blocks() {
    let mut buf = [0u8; 128];
    let mut w = BlockWriter::new(&mut buf, BlockKind::Records, id(3), &[ClockNum::new(1)]).unwrap();
    w.push(
        TrackNum::new(2),
        ClockTime::new(ClockNum::new(1), 5),
        &[b"x"],
    )
    .unwrap();
    w.push(
        TrackNum::new(4),
        ClockTime::new(ClockNum::new(1), 6),
        &[b"y"],
    )
    .unwrap();
    let len = w.finish();
    let e = IndexEntry::of_block(77, &Block::read(&buf[..len]).unwrap()).unwrap();
    assert_eq!(
        (e.offset, e.len, e.kind, e.track_id, e.count),
        (77, len as u32, BlockKind::Records, RECORDS_TRACK_ID, 2)
    );

    let head = crate::ChunkHead {
        kind: BlockKind::Events,
        track_id: TrackId::new(0xfeed),
        track_number: TrackNum::new(2),
        first_item: ItemNum::new(4096),
        count: 3,
        first_tick: 10,
        last_tick: 30,
        latest_end: 45,
        time_mode: 0,
        end_mode: EndMode::Durations,
        grid: None,
        tile: crate::TileCoords::NONE,
    };
    let col = [0u8; 24];
    let cols = [(
        crate::ColumnSpec {
            kind: crate::ColumnKind::Starts,
            codec: crate::Codec::RAW,
            param: 0,
            len: 24,
        },
        &col[..],
    )];
    let mut cb = vec![0u8; crate::chunk::chunk_len(&head, &[cols[0].0], 0)];
    let n = crate::chunk::write_chunk(&mut cb, id(1), &head, &cols, 0).unwrap();
    let e = IndexEntry::of_block(4000, &Block::read(&cb[..n]).unwrap()).unwrap();
    assert_eq!(Some(e), IndexEntry::of_chunk(4000, n, &head));
    assert_eq!(
        (
            e.track_id.get(),
            e.first_item.get(),
            e.count,
            e.first_tick,
            e.last_tick,
            e.latest_end
        ),
        (0xfeed, 4096, 3, 10, 30, 45)
    );

    // a summary block: the fields of its head, its nodes left zero
    use crate::summary::{EventFields, Layout, SummaryHead, SummaryWriter, Tree, summary_len};
    let h = SummaryHead {
        track_id: TrackId::new(0xabc),
        track_number: TrackNum::new(5),
        tree: Tree::Events,
        level: 0,
        node_shift: 11,
        layout: Layout::Events(EventFields {
            flags: false,
            latest_end: true,
        }),
        first_item: ItemNum::new(512),
        items: 3000,
        first_tick: -7,
        last_tick: 29_983,
        latest_end: 29_995,
        tile: crate::TileCoords::NONE,
    };
    let mut sb = vec![0u8; summary_len(&h).unwrap()];
    let n = SummaryWriter::new(&mut sb, id(3), &h).unwrap().finish();
    assert_eq!(n, sb.len());
    let block = Block::read(&sb).unwrap();
    assert_eq!(block.summary_head(), Some(h));
    let e = IndexEntry::of_block(10, &block).unwrap();
    assert_eq!(
        (e.kind, e.len as usize, e.track_id.get(), e.track()),
        (BlockKind::Summary, sb.len(), 0xabc, Some(TrackNum::new(5)))
    );
    assert_eq!((e.tree, e.level), (1, h.level));
    assert_eq!((e.first_item, e.count), (h.first_item, h.items));
    assert_eq!(e.first_item, ItemNum::new(512));
    assert_eq!(e.first_tick, -7);
    assert_eq!(
        (e.first_tick, e.last_tick, e.latest_end),
        (h.first_tick, h.last_tick, h.latest_end)
    );

    let ix = write(
        &IndexHead {
            kind: IndexKind::Checkpoint,
            self_offset: 0,
            prev: None,
        },
        &[],
    );
    assert!(
        IndexEntry::of_block(0, &Block::read(&ix).unwrap()).is_none(),
        "an index lists no index"
    );
}

#[test]
fn damaged_heads_are_refused() {
    let entries: Vec<_> = (0..3).map(|i| entry(1, i * 100)).collect();
    let bytes = write(
        &IndexHead {
            kind: IndexKind::Final,
            self_offset: 0,
            prev: None,
        },
        &entries,
    );
    for at in 24..bytes.len() - 4 {
        let mut b = bytes.clone();
        b[at] ^= 0x10;
        // the block CRC is rewritten: only the head CRC and the ranges stand between the damage and the reader
        seal(&mut b);
        if let Ok(block) = Block::read(&b) {
            let r = IndexBlock::read(&block);
            if at < 32 + 24 + 24 {
                assert!(r.is_err(), "head byte {at}");
            }
        }
    }
}

/// Renumbering an index block rewrites its head CRC too, so its head still reads.
#[test]
fn renumbered_index_block_reads() {
    let entries: Vec<_> = (0..3).map(|i| entry(i, 100 * i)).collect();
    for kind in [IndexKind::Checkpoint, IndexKind::Final] {
        let head = IndexHead {
            kind,
            self_offset: 900,
            prev: None,
        };
        let mut bytes = write(&head, &entries);
        crate::set_id(&mut bytes, id(77)).unwrap();
        let block = Block::read(&bytes).unwrap();
        assert_eq!(block.seq, 77);
        assert_eq!(IndexBlock::read(&block).unwrap().entry_count(), 3);
        let head_len = IndexBlock::head_len(&bytes).unwrap();
        assert!(IndexBlock::read_head(&bytes[..head_len]).is_ok());
    }
}

/// Index, close, tail and piece head blocks are the writer's own: not taken as deferred blocks.
#[test]
fn structural_blocks_are_not_deferred() {
    let mut w = crate::TapeWriter::create_on(crate::MemSink::new(), crate::SyncPolicy::None);
    let mut defs = crate::Definitions::new();
    crate::tests::hold(
        &mut defs,
        &crate::tests::test_definitions(ClockNum::new(1), &[1]),
    )
    .unwrap();
    let head = IndexHead {
        kind: IndexKind::Checkpoint,
        self_offset: 0,
        prev: None,
    };
    let index = write(&head, &[entry(1, 0)]);
    assert!(matches!(
        w.defer_block(&defs, index),
        Err(crate::WriteError::Index(IndexError::Invalid(_)))
    ));
    let mut tail = [0u8; crate::piece::TAIL_LEN];
    crate::piece::Tail {
        piece_len: 1000,
        close_offset: 0,
        close_len: 200,
        reason: crate::CloseReason::Closed,
        hash: crate::piece::ContentHash([0; 32]),
        ident: None,
        next: None,
    }
    .write(&mut tail, id(0));
    assert!(w.defer_block(&defs, tail.to_vec()).is_err());
    let mut buf = [0u8; 64];
    let mut b = BlockWriter::new(&mut buf, BlockKind::Records, id(0), &[ClockNum::new(1)]).unwrap();
    b.push(
        TrackNum::new(1),
        ClockTime::new(ClockNum::new(1), 5),
        &[b"x"],
    )
    .unwrap();
    let len = b.finish();
    w.defer_block(&defs, buf[..len].to_vec()).unwrap();
    w.close().unwrap();
}

/// An entry naming no block (length 0, longer than a block, an end past u64) is refused when a whole block or a
/// track's entries are read, and by the writer.
#[test]
fn entries_naming_no_block_are_refused() {
    let entries: Vec<_> = (0..3).map(|i| entry(1, i * 400)).collect();
    let bad_entries = [
        IndexEntry {
            len: 0,
            ..entries[1]
        },
        IndexEntry {
            len: (crate::MAX_BLOCK_LEN + 1) as u32,
            ..entries[1]
        },
        IndexEntry {
            offset: u64::MAX - 10,
            ..entries[1]
        },
    ];
    for kind in [IndexKind::Checkpoint, IndexKind::Final] {
        let head = IndexHead {
            kind,
            self_offset: 9000,
            prev: None,
        };
        let bytes = write(&head, &entries);
        let at = IndexBlock::read(&Block::read(&bytes).unwrap())
            .unwrap()
            .entry_offset(1)
            .unwrap();
        for bad in bad_entries {
            let mut buf = vec![0u8; bytes.len()];
            assert!(matches!(
                write_index(&mut buf, id(5), &head, &[entries[0], bad, entries[2]]),
                Err(IndexError::Invalid(_))
            ));
            // the bad entry patched in, every CRC over it made good again
            let mut b = bytes.clone();
            b[at..at + ENTRY_LEN].copy_from_slice(&bad.encode());
            if kind == IndexKind::Final {
                let entries_at = IndexBlock::head_len(&b).unwrap();
                let crc = CRC.checksum(&b[entries_at..b.len() - CRC_LEN]);
                b[DIR_AT + 16..DIR_AT + 20].copy_from_slice(&crc.to_le_bytes());
                restamp_head(&mut b);
            }
            seal(&mut b);
            let block = Block::read(&b).unwrap();
            assert_eq!(
                IndexBlock::read(&block).unwrap_err(),
                IndexError::BadEntries,
                "{kind:?} {bad:?}"
            );
            if kind == IndexKind::Final {
                let only_head = IndexBlock::read_head(&b).unwrap();
                let list = only_head.track(TrackId::new(1)).unwrap();
                let (from, n) = only_head.track_range(&list);
                assert!(matches!(
                    only_head.track_entries(&list, &b[from..from + n]),
                    Err(IndexError::BadEntries)
                ));
            }
        }
    }
}

/// Entries and offsets past the last are None, never an overflow.
#[test]
fn entry_past_the_last_is_none() {
    let entries: Vec<_> = (0..3).map(|i| entry(1, i * 400)).collect();
    let bytes = write(
        &IndexHead {
            kind: IndexKind::Checkpoint,
            self_offset: 9000,
            prev: None,
        },
        &entries,
    );
    let ix = IndexBlock::read(&Block::read(&bytes).unwrap()).unwrap();
    assert_eq!(ix.entry_offset(3), Some(bytes.len() - CRC_LEN));
    assert_eq!(ix.entry_offset(4), None);
    assert_eq!(ix.entry(usize::MAX), None);
    assert_eq!(ix.entry_offset(usize::MAX), None);
}

/// A directory list over more entries than the block holds is refused by the head, and a list not of this block
/// by [IndexBlock::track_entries].
#[test]
fn lists_past_the_entries_are_refused() {
    let entries: Vec<_> = (0..4).map(|i| entry(i % 2, i * 400)).collect();
    let mut sorted = entries.clone();
    sorted.sort_by_key(|e| (e.track_id, e.offset));
    let bytes = write(
        &IndexHead {
            kind: IndexKind::Final,
            self_offset: 9000,
            prev: None,
        },
        &sorted,
    );
    let mut b = bytes.clone();
    // the second list's count: u32::MAX
    b[DIR_AT + TRACK_LEN + 12..DIR_AT + TRACK_LEN + 16].copy_from_slice(&u32::MAX.to_le_bytes());
    restamp_head(&mut b);
    seal(&mut b);
    assert!(matches!(
        IndexBlock::read_head(&b),
        Err(IndexError::Bound(_))
    ));
    let ix = IndexBlock::read_head(&bytes).unwrap();
    let list = TrackList {
        first: 3,
        count: 2,
        ..ix.track(TrackId::new(1)).unwrap()
    };
    let (from, n) = ix.track_range(&list);
    assert_eq!(from + n, bytes.len() - CRC_LEN + ENTRY_LEN);
    assert!(matches!(
        ix.track_entries(&list, &[]),
        Err(IndexError::BadEntries)
    ));
}

/// An index whose entry is marked unhashed (flag bit 2, reserved) is refused as a level this build doesn't handle.
/// An entry of a writer below Integrity::Full is marked unhashed and holds no unit hash: it reads back so.
#[test]
fn an_unhashed_entry_reads_back_with_no_unit_hash() {
    let entries: Vec<_> = (0..3)
        .map(|i| IndexEntry {
            unit: crate::piece::UnitHash::NONE,
            unhashed: true,
            ..entry(i % 2, 1000 + i * 300)
        })
        .collect();
    let head = IndexHead {
        kind: IndexKind::Checkpoint,
        self_offset: 2500,
        prev: None,
    };
    let bytes = write(&head, &entries);
    let ix = IndexBlock::read(&Block::read(&bytes).unwrap()).unwrap();
    let at = ix.entry_offset(1).unwrap() + 15;
    assert_eq!(bytes[at] & ENTRY_UNHASHED, ENTRY_UNHASHED, "marked");
    assert_eq!(ix.entries().collect::<Vec<_>>(), entries);
}

/// An entry with its flag byte or range rewritten in a final index, its list, head and block CRCs made good again:
/// refused alike by a whole read and by one track's entries read alone.
fn refused_both_ways(change: impl Fn(&mut [u8]), error: IndexError) {
    let entries: Vec<_> = (0..4).map(|i| entry(i / 2, 1000 + i * 300)).collect();
    let head = IndexHead {
        kind: IndexKind::Final,
        self_offset: 4000,
        prev: None,
    };
    let mut bytes = write(&head, &entries);
    let ix = IndexBlock::read(&Block::read(&bytes).unwrap()).unwrap();
    let list = ix.track(TrackId::new(1)).unwrap();
    let (from, n) = ix.track_range(&list);
    change(&mut bytes[from..from + ENTRY_LEN]);
    // the list's CRC in the directory, then the head CRC and the block CRC
    let dir_at = DIR_AT + TRACK_LEN + 16;
    let crc = crate::CRC.checksum(&bytes[from..from + n]);
    bytes[dir_at..dir_at + 4].copy_from_slice(&crc.to_le_bytes());
    restamp_head(&mut bytes);
    seal(&mut bytes);
    let block = Block::read(&bytes).expect("a good block");
    assert_eq!(IndexBlock::read(&block).err(), Some(error));
    let ix = IndexBlock::read_head(&bytes).unwrap();
    let list = ix.track(TrackId::new(1)).unwrap();
    assert_eq!(
        ix.track_entries(&list, &bytes[from..from + n]).err(),
        Some(error)
    );
}

/// One check of an entry on every read: an unhashed entry holding a unit hash, unknown flag bits, a range that decreases or sits on a block
/// that isn't a chunk, a relation break on a block that isn't an events chunk, refused by a whole index and by one track's entries alike.
#[test]
fn an_entry_is_checked_the_same_on_every_read() {
    refused_both_ways(|e| e[15] |= ENTRY_UNHASHED, IndexError::BadEntries);
    for bit in 5..8 {
        refused_both_ways(
            |e| e[15] |= 1 << bit,
            IndexError::Invalid("an entry flag this build doesn't know"),
        );
    }
    refused_both_ways(
        |e| {
            e[15] |= ENTRY_RANGE;
            e[ENTRY_RANGE_AT..ENTRY_RANGE_AT + 8].copy_from_slice(&5u64.to_le_bytes());
            e[ENTRY_RANGE_AT + 8..ENTRY_RANGE_AT + 16].copy_from_slice(&4u64.to_le_bytes());
        },
        IndexError::BadEntries,
    );
    refused_both_ways(
        |e| {
            e[15] |= ENTRY_RANGE;
            e[12] = BlockKind::Summary.to_u8();
        },
        IndexError::BadEntries,
    );
    refused_both_ways(
        |e| {
            e[15] |= ENTRY_BREAK;
            e[12] = BlockKind::Series.to_u8();
        },
        IndexError::BadEntries,
    );
    // a tile number on an events chunk, and beside a range on a series chunk
    refused_both_ways(|e| e[15] = ENTRY_TILE, IndexError::BadEntries);
    refused_both_ways(
        |e| {
            e[15] = ENTRY_TILE | ENTRY_RANGE;
            e[12] = BlockKind::Series.to_u8();
        },
        IndexError::BadEntries,
    );
}

/// A tile number on a series summary block's entry reads back; on an event tree's summary block it is refused.
#[test]
fn summary_entries_carry_tile_numbers() {
    let tree = |t: crate::summary::Tree| IndexEntry {
        kind: BlockKind::Summary,
        tree: t.to_u8(),
        range: None,
        tile: Some(7 << 12 | 3 << 6 | 2 << 3 | 5),
        ..entry(1, 1000)
    };
    let head = IndexHead {
        kind: IndexKind::Checkpoint,
        self_offset: 4000,
        prev: None,
    };
    let series = [tree(crate::summary::Tree::Series)];
    let bytes = write(&head, &series);
    let ix = IndexBlock::read(&Block::read(&bytes).unwrap()).unwrap();
    assert_eq!(ix.entries().collect::<Vec<_>>(), series);
    refused_both_ways(
        |e| {
            e[15] = ENTRY_TILE;
            e[12] = BlockKind::Summary.to_u8();
            e[14] = crate::summary::Tree::Events.to_u8();
        },
        IndexError::BadEntries,
    );
}

/// Grid chunks' tile numbers, 0 and all 128 bits among them, read back from bytes 72..88 under flag bit 4; a series
/// chunk's entry without one keeps those bytes zero.
#[test]
fn tile_numbers_round_trip() {
    let tiles = [
        None,
        Some(0),
        Some(1),
        Some(7 << 12 | 3 << 6 | 2 << 3 | 5),
        Some(u128::MAX),
    ];
    let entries: Vec<_> = tiles
        .iter()
        .enumerate()
        .map(|(i, tile)| IndexEntry {
            kind: BlockKind::Series,
            range: None,
            tile: *tile,
            ..entry(1, 1000 + i as u64 * 300)
        })
        .collect();
    let head = IndexHead {
        kind: IndexKind::Checkpoint,
        self_offset: 4000,
        prev: None,
    };
    let bytes = write(&head, &entries);
    let ix = IndexBlock::read(&Block::read(&bytes).unwrap()).unwrap();
    assert_eq!(ix.entries().collect::<Vec<_>>(), entries);
    let at = HEADER_LEN + INDEX_FIXED_LEN;
    let first = &bytes[at..at + ENTRY_LEN];
    assert_eq!(first[ENTRY_FLAGS_AT] & ENTRY_TILE, 0);
    assert_eq!(first[ENTRY_RANGE_AT..ENTRY_UNIT_AT], [0; 16]);
    let last = &bytes[at + 4 * ENTRY_LEN..at + 5 * ENTRY_LEN];
    assert_eq!(last[ENTRY_FLAGS_AT] & ENTRY_TILE, ENTRY_TILE);
    assert_eq!(last[ENTRY_RANGE_AT..ENTRY_UNIT_AT], [0xff; 16]);
}

/// A checkpoint whose entries are `len` bytes, each the first `len` bytes of a written one, its head saying so and
/// its CRCs made good.
fn entries_cut_to(entries: &[IndexEntry], len: usize) -> Vec<u8> {
    let head = IndexHead {
        kind: IndexKind::Checkpoint,
        self_offset: 4000,
        prev: None,
    };
    let bytes = write(&head, entries);
    let at = HEADER_LEN + INDEX_FIXED_LEN;
    let mut out = bytes[..at].to_vec();
    for e in bytes[at..bytes.len() - CRC_LEN].chunks(ENTRY_LEN) {
        out.extend_from_slice(&e[..len]);
    }
    out.extend([0; CRC_LEN]);
    let n = out.len() as u32;
    out[8..12].copy_from_slice(&n.to_le_bytes());
    out[58..60].copy_from_slice(&(len as u16).to_le_bytes());
    restamp_head(&mut out);
    seal(&mut out);
    out
}

/// Entries of [ENTRY_READ_LEN] bytes, as pieces written before the entry grew to [ENTRY_LEN] hold, read as the
/// written ones; one byte shorter is refused.
#[test]
fn entries_of_the_read_length_read_and_shorter_are_refused() {
    let entries: Vec<_> = (0..4).map(|i| entry(i / 2, 1000 + i * 300)).collect();
    let old = entries_cut_to(&entries, ENTRY_READ_LEN);
    let ix = IndexBlock::read(&Block::read(&old).unwrap()).unwrap();
    assert_eq!(ix.entry_len(), ENTRY_READ_LEN);
    assert_eq!(ix.entries().collect::<Vec<_>>(), entries);
    let short = entries_cut_to(&entries, ENTRY_READ_LEN - 1);
    assert_eq!(
        IndexBlock::read(&Block::read(&short).unwrap()).err(),
        Some(IndexError::BadHead)
    );
}
