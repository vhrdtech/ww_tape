//! ww_tape index, close, tail and piece head blocks and the positional reader on any bytes: decoding never panics,
//! a reader opens any bytes and every entry it gives lies in its piece, a good chunk's head reads alone, a series
//! timeline builds from the heads and a track's range hash folds from the index or the chunks, the same from both for a
//! piece that verifies; index blocks built from the input read back equal, a final one also track by track from its
//! head.
use ww_tape::index::{
    IndexBlock, IndexEntry, IndexHead, IndexKind, index_len, track_count, write_index,
};
use ww_tape::piece::{CloseBlock, PieceHeadBlock};
use ww_tape::{
    Block, BlockKind, IndexError, Layout, Medium, OpenOptions, OwnerId, Ratio64, ReadError, Tail,
    TapeReader, TrackHash,
};

/// Look at everything the block decoders give for `data`.
pub(crate) fn decode(data: &[u8]) {
    if let Ok(len) = IndexBlock::head_len(data)
        && let Ok(ix) = IndexBlock::read_head(&data[..len.min(data.len())])
    {
        for t in ix.tracks() {
            let (from, n) = ix.track_range(&t);
            if let Some(bytes) = data.get(from..from.saturating_add(n))
                && let Ok(it) = ix.track_entries(&t, bytes)
            {
                it.for_each(drop);
            }
        }
    }
    if let Ok(b) = Block::read(data) {
        if let Ok(ix) = IndexBlock::read(&b) {
            assert_eq!(ix.entries().count(), ix.entry_count());
            let _ = ix.track(0);
        }
        if let Ok(f) = CloseBlock::read(&b) {
            let _ = f.close();
            f.tracks().for_each(drop);
        }
        if let Ok(h) = PieceHeadBlock::read(&b) {
            let _ = h.head();
            h.tracks().for_each(drop);
        }
    }
    let _ = Tail::read(data);
}

pub(crate) fn read(data: &[u8], options: OpenOptions) {
    let Ok(r) = TapeReader::open_with(data, options) else {
        return;
    };
    let mut end = 0;
    for (i, p) in r.pieces().iter().enumerate() {
        assert!(p.start >= end && p.end >= p.start && p.end <= data.len() as u64);
        end = p.end;
        if let Ok(entries) = r.entries(i) {
            let mut series = Vec::new();
            for e in &entries {
                assert!(p.start + e.offset < data.len() as u64);
                let mut buf = vec![];
                let block = r.read_block(i, e, &mut buf);
                let chunk = block
                    .as_ref()
                    .ok()
                    .and_then(|b| b.chunk())
                    .map(|c| c.head());
                let mut hbuf = vec![];
                let head = r.read_chunk_head(i, e, &mut hbuf);
                // a whole chunk block that reads has a head that reads alone, the same one, when it is the chunk
                // its entry names; else the index is damaged
                if let Some(c) = chunk {
                    let named = e.kind == c.kind
                        && e.track_id == c.track_id
                        && e.track_number == c.track_number
                        && e.first_item == c.first_item
                        && e.count == u64::from(c.count);
                    match named {
                        true => assert_eq!(head.ok(), Some(c), "a good chunk's head reads alone"),
                        false => assert!(
                            matches!(head, Err(ReadError::Index(IndexError::BadEntries))),
                            "a chunk other than its entry names is refused"
                        ),
                    }
                }
                if e.kind == BlockKind::Series && !series.contains(&e.track_id) {
                    series.push(e.track_id);
                }
            }
            for track_id in series {
                if let Ok((t, _)) = r.series_timeline(i, track_id, Ratio64::new(5, 3).unwrap()) {
                    for s in t.stretches() {
                        assert!(s.first_item < s.end_item);
                    }
                }
            }
        }
        let _ = r.track_entries(i, 0);
        relations_in_order(&r, i);
    }
    let _ = r.chain();
    // the hash of a track's chunks, from the index or, where it doesn't read, from the chunks; never more chunks
    // than blocks fit the bytes
    let ids: Vec<u64> = r
        .pieces()
        .iter()
        .enumerate()
        .flat_map(|(i, _)| r.entries(i).unwrap_or_default())
        .map(|e| e.track_id)
        .take(4)
        .collect();
    for id in ids {
        if let Ok(h) = r.range_hash(id, OwnerId::NONE, 0..u64::MAX, TrackHash::START) {
            assert!(h.read <= h.chunks);
            assert!(h.chunks <= data.len() as u64 / 36 + 1);
        }
    }
}

/// A track's chunks whose entries [TapeReader::track_entries] hands out start each range at or above the top of the
/// range before it of their track number, but at a chunk marked a break; a piece that verifies (its relation columns
/// in order) lists no entries that go down unmarked.
fn relations_in_order(r: &TapeReader<&[u8]>, i: usize) {
    let verified = r.verify(i).map(|v| v.holds).is_ok_and(|v| v);
    let mut ids: Vec<u64> = r
        .entries(i)
        .unwrap_or_default()
        .iter()
        .map(|e| e.track_id)
        .collect();
    ids.sort_unstable();
    ids.dedup();
    for id in ids.into_iter().take(8) {
        match r.track_entries(i, id) {
            Ok(entries) => {
                let mut top = std::collections::BTreeMap::new();
                for e in entries
                    .iter()
                    .filter(|e| e.kind == BlockKind::Events && !e.repeat)
                {
                    if e.relation_break {
                        top.remove(&e.track_number);
                    }
                    if let Some((lo, hi)) = e.range {
                        let before = top.insert(e.track_number, hi);
                        assert!(
                            before.is_none_or(|t| lo >= t),
                            "track {id:#x}: a range goes down without a break: {entries:?}"
                        );
                    }
                }
            }
            Err(ReadError::RelationOrder { track_id, .. }) => {
                assert_eq!(track_id, id);
                assert!(
                    !verified,
                    "a verified piece lists track {id:#x} out of relation order"
                );
            }
            Err(_) => {}
        }
    }
}

/// A piece that [TapeReader::verify] passes folds a track to the same hash from its index as from its chunks walked
/// with the final index made unreadable, where the walk explains every byte and finds the chunks the index lists.
fn index_folds_as_the_walk(data: &[u8]) {
    let Ok(r) = TapeReader::open(data) else {
        return;
    };
    let ([p], Ok(true)) = (r.pieces(), r.verify(0).map(|v| v.holds)) else {
        return;
    };
    let Some(at) = p
        .close
        .as_ref()
        .and_then(|(f, _)| f.final_index)
        .and_then(|a| usize::try_from(p.start.saturating_add(a)).ok())
    else {
        return;
    };
    let mut walked = data.to_vec();
    // a byte of the final index's head: its CRC fails, the reader walks the chunks
    let Some(byte) = walked.get_mut(at.saturating_add(40)) else {
        return;
    };
    *byte ^= 1;
    let Ok(w) = TapeReader::open(&walked[..]) else {
        return;
    };
    let mut ids: Vec<u64> = r
        .entries(0)
        .unwrap_or_default()
        .iter()
        .map(|e| e.track_id)
        .collect();
    ids.sort_unstable();
    ids.dedup();
    for id in ids.into_iter().take(4) {
        let fold =
            |r: &TapeReader<&[u8]>| r.range_hash(id, OwnerId::NONE, 0..u64::MAX, TrackHash::START);
        if let (Ok(a), Ok(b)) = (fold(&r), fold(&w))
            && b.skipped == 0
            && a.skipped == 0
            && a.chunks == b.chunks
        {
            assert_eq!(
                (a.hash, a.items),
                (b.hash, b.items),
                "a verified piece folds track {id:#x} from its index as from its chunks"
            );
        }
    }
}

pub fn index(data: &[u8]) {
    decode(data);
    read(data, OpenOptions::default());
    index_folds_as_the_walk(data);
    // as a raw medium with the gap rules of an SD card: walked forward, gaps read past
    read(
        data,
        OpenOptions {
            medium: Medium::Raw,
            layout: Some(Layout::UNALIGNED.on_sd_card()),
            ..OpenOptions::default()
        },
    );
    // index blocks of entries made from the input: [final?][entries of 9 bytes: track u8, offset u64]
    let Some((&flag, rest)) = data.split_first() else {
        return;
    };
    let mut entries: Vec<IndexEntry> = rest
        .as_chunks::<9>()
        .0
        .iter()
        .map(|c| IndexEntry {
            offset: u64::from_le_bytes(c[1..9].try_into().unwrap()),
            // a header and a CRC at least, as write_index takes
            len: u32::from(c[0]) + 36,
            kind: BlockKind::from_u8(c[0]),
            level: c[0],
            tree: 0,
            track_id: u64::from(c[0] % 5),
            track_number: c[0].into(),
            second: 0,
            first_item: 0,
            count: 1,
            first_tick: -1,
            last_tick: 1,
            latest_end: 2,
            repeat: c[0] & 0x80 != 0,
            relation_break: false,
            unit: ww_tape::UnitHash::NONE,
            // a chunk's range relation, both values round-tripped; one that decreases or sits on another kind is refused
            range: (c[0] & 0x40 != 0).then(|| {
                (
                    u64::from(c[2]),
                    u64::from_le_bytes(c[1..9].try_into().unwrap()),
                )
            }),
        })
        .collect();
    let kind = if flag & 1 == 1 {
        entries.sort_by_key(|e| (e.track_id, e.offset));
        entries.dedup_by_key(|e| (e.track_id, e.offset));
        IndexKind::Final
    } else {
        IndexKind::Checkpoint
    };
    let tracks = if kind == IndexKind::Final {
        track_count(&entries)
    } else {
        0
    };
    let mut buf = vec![0u8; index_len(entries.len(), tracks)];
    let head = IndexHead {
        kind,
        self_offset: u64::from(flag),
        prev: (flag & 2 == 2).then_some(7),
    };
    // write_index refuses an entry whose end passes a u64, a range that decreases or sits on no chunk's entry, and
    // in a final index a track whose blocks overlap (only a state record's entries share a block)
    let valid = entries.iter().all(|e| {
        e.offset.checked_add(u64::from(e.len)).is_some()
            && e.range.is_none_or(|(lo, hi)| {
                lo <= hi && matches!(e.kind, BlockKind::Events | BlockKind::Series)
            })
    }) && (kind == IndexKind::Checkpoint
        || entries.windows(2).all(|w| {
            w[0].track_id != w[1].track_id
                || w[0].offset.saturating_add(u64::from(w[0].len)) <= w[1].offset
                || (w[0].offset == w[1].offset
                    && w[0].len == w[1].len
                    && w[0].track_id == ww_tape::index::STATE_TRACK_ID)
        }));
    let n = match write_index(
        &mut buf,
        ww_tape::BlockId::new(ww_tape::SessionTag::writable(1), 1),
        &head,
        &entries,
    ) {
        Ok(n) if valid => n,
        Err(ww_tape::IndexError::Invalid(_)) if !valid => return,
        r => panic!("write_index gave {r:?} for entries that are valid: {valid}"),
    };
    let b = Block::read(&buf[..n]).expect("written good");
    let ix = IndexBlock::read(&b).expect("an index");
    assert_eq!(ix.entries().collect::<Vec<_>>(), entries);
    assert_eq!(
        (ix.kind(), ix.self_offset(), ix.prev()),
        (kind, head.self_offset, head.prev)
    );
    let len = IndexBlock::head_len(&buf[..ww_tape::index::HEAD_LEN_PREFIX]).expect("a head");
    let only = IndexBlock::read_head(&buf[..len]).expect("a head");
    let mut again = vec![];
    for t in only.tracks() {
        let (from, n) = only.track_range(&t);
        again.extend(
            only.track_entries(&t, &buf[from..from + n])
                .expect("good entries"),
        );
    }
    if kind == IndexKind::Final {
        assert_eq!(again, entries);
    }
}
