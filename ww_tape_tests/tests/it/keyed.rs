//! Keyed state tracks: a key's records found through the index, its history, a key range in order, deletions, the
//! refusals of a put that doesn't fit its track (TRACK-8).

use ww_tape::TrackNum;
use ww_tape::index::STATE_TRACK_ID;
use ww_tape::{
    Block, BlockError, BlockKind, Definitions, Describe, DescribeError, FLAG_REPEAT, KeyId,
    KeyedItem, MemPieces, OwnerId, PieceChain, STATE_KEY_LEN_MAX, Slot, SyncPolicy, TapeReader,
    TrackDef, TrackRepeat, WriteError,
};

use crate::definitions::{CLOCK, base_defs, hold, record_blocks, state, track};

pub(crate) const KEYED: TrackNum = TrackNum::new(10);
pub(crate) const PLAIN: TrackNum = TrackNum::new(11);

/// Keyed state track `number` on [CLOCK].
pub(crate) fn keyed(number: u32) -> Describe<'static> {
    let Describe::Track(t) = track(number, CLOCK) else {
        unreachable!("track() gives a track")
    };
    Describe::Track(TrackDef {
        repeat: TrackRepeat::KEYED,
        ..t
    })
}

/// The tests' definitions with keyed track [KEYED] and plain state track [PLAIN].
pub(crate) fn defs() -> Definitions {
    let mut defs = base_defs().clone();
    hold(&mut defs, &[keyed(KEYED.get()), state(PLAIN.get())]).unwrap();
    defs
}

/// A chain in memory with one piece.
fn chain() -> (MemPieces, PieceChain<MemPieces>) {
    let mem = MemPieces::new();
    let chain = PieceChain::create_on(
        mem.clone(),
        OwnerId([0x4b; 32]),
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
    (mem, chain)
}

/// The keys the store puts, in the order it puts them: values, a change, a deletion, a key put again after it.
fn store() -> Vec<u8> {
    let defs = defs();
    let (mem, mut chain) = chain();
    let puts: [(&[u8], Option<&[u8]>); 9] = [
        (b"/b", Some(b"1")),
        (b"/a", Some(b"2")),
        (b"/eventfull/layouts/main", Some(b"L1")),
        (b"/eventfull/layouts/side", Some(b"S1")),
        (b"/a", Some(b"3")),
        (b"/eventfull/layouts/main", Some(b"L2")),
        (b"/b", None),
        (b"/c", Some(b"")),
        (b"/eventfull/layouts/side", None),
    ];
    for (t, (key, value)) in (0i64..).zip(puts) {
        let written = match value {
            Some(v) => chain.put_key(&defs, KEYED, t, key, v),
            None => chain.delete_key(&defs, KEYED, t, key),
        };
        assert!(written.unwrap(), "{key:?}");
    }
    let closed = chain.close().unwrap();
    mem.get(&closed[0].tape).unwrap().unwrap().to_vec().unwrap()
}

fn slot(key: &[u8]) -> Slot {
    Slot::Keyed {
        track: KEYED,
        key: KeyId::of(key).unwrap(),
    }
}

/// A key's newest record and every older one are found through the index without reading a block: each entry names
/// the key's slot, its item's hash and its time.
#[test]
fn a_keys_records_are_found_through_the_index() {
    let bytes = store();
    let r = TapeReader::open(&bytes[..]).unwrap();
    assert!(r.verify(0).unwrap().holds);
    let entries = r.key_entries(0, KEYED, b"/a").unwrap();
    assert_eq!(entries.len(), 2, "{entries:?}");
    let ticks: Vec<i64> = entries.iter().map(|(_, s)| s.ticks).collect();
    assert_eq!(ticks, [1, 4], "oldest first");
    for (e, s) in &entries {
        assert_eq!(e.track_id, STATE_TRACK_ID);
        assert_eq!(s.slot, slot(b"/a"));
        assert_eq!(e.key, Some(KeyId::of(b"/a").unwrap()));
        assert!(!s.deleted);
    }
    let mut item = Vec::new();
    KeyedItem {
        key: b"/a",
        value: Some(b"3"),
    }
    .encode_into(&mut item)
    .unwrap();
    assert_eq!(entries[1].1.hash, ww_tape::ItemHash::of(&item));
    let deleted = r.key_entries(0, KEYED, b"/b").unwrap();
    assert_eq!(
        deleted.iter().map(|(_, s)| s.deleted).collect::<Vec<_>>(),
        [false, true]
    );
    assert_eq!(
        r.key_entries(0, KEYED, b"/none").unwrap(),
        [] as [(ww_tape::IndexEntry, ww_tape::StateEntry); 0]
    );
}

/// The record in force at a place: the newest before it, a value or a deletion, its full key checked.
#[test]
fn a_key_reads_as_of_a_place() {
    let bytes = store();
    let r = TapeReader::open(&bytes[..]).unwrap();
    let now = r.key_at(0, KEYED, b"/a", u64::MAX).unwrap();
    assert!(now.is_whole() || now.uncovered.iter().all(|u| u.end == u64::MAX));
    let item = now.item.unwrap();
    let k = item.keyed().unwrap();
    assert_eq!((k.key, k.value), (&b"/a"[..], Some(&b"3"[..])));
    let entries = r.key_entries(0, KEYED, b"/a").unwrap();
    let before = r.key_at(0, KEYED, b"/a", entries[1].0.offset).unwrap();
    assert_eq!(before.item.unwrap().keyed().unwrap().value, Some(&b"2"[..]));
    let gone = r.key_at(0, KEYED, b"/b", u64::MAX).unwrap().item.unwrap();
    assert_eq!(gone.keyed().unwrap().value, None, "a deletion ends the key");
    let empty = r.key_at(0, KEYED, b"/c", u64::MAX).unwrap().item.unwrap();
    assert_eq!(
        empty.keyed().unwrap().value,
        Some(&b""[..]),
        "an empty value is not a deletion"
    );
    assert!(
        r.key_at(0, KEYED, b"/none", u64::MAX)
            .unwrap()
            .item
            .is_none()
    );
    let long = [b'k'; STATE_KEY_LEN_MAX + 1];
    assert!(r.key_at(0, KEYED, &long, u64::MAX).unwrap().item.is_none());
}

/// A key's history: every record of it, newest first, values and deletions.
#[test]
fn a_keys_history_lists_every_record_newest_first() {
    let bytes = store();
    let r = TapeReader::open(&bytes[..]).unwrap();
    let h = r
        .key_history(0, KEYED, b"/eventfull/layouts/main", u64::MAX)
        .unwrap();
    assert!(h.lost.is_empty() && !h.spent);
    let values: Vec<Option<Vec<u8>>> = h
        .items
        .iter()
        .map(|i| i.keyed().unwrap().value.map(<[u8]>::to_vec))
        .collect();
    assert_eq!(values, [Some(b"L2".to_vec()), Some(b"L1".to_vec())]);
    let side = r
        .key_history(0, KEYED, b"/eventfull/layouts/side", u64::MAX)
        .unwrap();
    assert_eq!(side.items.len(), 2);
    assert_eq!(side.items[0].keyed().unwrap().value, None);
}

/// A prefix lists its live keys in order from the index; a deletion drops a key, and a prefix past the key head
/// leaves the index unsure until the records are read.
#[test]
fn a_prefix_lists_its_live_keys_in_order() {
    let bytes = store();
    let r = TapeReader::open(&bytes[..]).unwrap();
    let short = r.key_slots(0, KEYED, b"/", u64::MAX).unwrap();
    let keys: Vec<Option<&[u8]>> = short.iter().map(|s| s.key.key()).collect();
    // "/b" deleted, "/eventfull/layouts/side" deleted: the long key's head is not a whole key
    assert_eq!(keys, [Some(&b"/a"[..]), Some(b"/c"), None]);
    assert!(short.iter().all(|s| s.sure));
    let long = r.key_slots(0, KEYED, b"/eventfull/lay", u64::MAX).unwrap();
    assert_eq!(long.len(), 1);
    assert!(
        !long[0].sure,
        "only the record decides a prefix past the head"
    );
    let at = r.keys_at(0, KEYED, b"/", u64::MAX).unwrap();
    assert!(at.lost.is_empty() && !at.spent);
    let listed: Vec<(&[u8], Vec<u8>)> = at
        .items
        .iter()
        .map(|(k, i)| (k.as_slice(), i.keyed().unwrap().value.unwrap().to_vec()))
        .collect();
    assert_eq!(
        listed,
        [
            (&b"/a"[..], b"3".to_vec()),
            (b"/c", Vec::new()),
            (b"/eventfull/layouts/main", b"L2".to_vec())
        ]
    );
    let layouts = r
        .keys_at(0, KEYED, b"/eventfull/layouts/", u64::MAX)
        .unwrap();
    assert_eq!(layouts.items.len(), 1);
    assert!(
        r.keys_at(0, KEYED, b"/eventfull/x", u64::MAX)
            .unwrap()
            .items
            .is_empty()
    );
    // as of the place of the first deletion, "/b" still lives
    let b = r.key_entries(0, KEYED, b"/b").unwrap();
    let before = r.keys_at(0, KEYED, b"/", b[1].0.offset).unwrap();
    assert!(before.items.contains_key(&b"/b"[..]));
}

/// The index's deletion mark lists a key's slots; the record decides a value. An index whose newest entry of a live
/// key claims a deletion, behind good CRCs, drops the key from [TapeReader::key_slots] but not from
/// [TapeReader::keys_at], which reads the record.
#[test]
fn keys_at_takes_a_deletion_from_the_record() {
    let mut bytes = store();
    let newest = r_entries(&bytes, b"/a").last().unwrap().0;
    forge_index(&mut bytes, |e| {
        if e.key == Some(KeyId::of(b"/a").unwrap()) && e.offset == newest.offset {
            e.deleted = true;
        }
    });
    let r = TapeReader::open(&bytes[..]).unwrap();
    assert!(
        r.key_entries(0, KEYED, b"/a").unwrap()[1].1.deleted,
        "the forged mark is read"
    );
    let slots = r.key_slots(0, KEYED, b"/a", u64::MAX).unwrap();
    assert!(slots.is_empty(), "the index alone lists no slot");
    let at = r.keys_at(0, KEYED, b"/a", u64::MAX).unwrap();
    let a = at.items.get(&b"/a"[..]).expect("the record is a value");
    assert_eq!(a.keyed().unwrap().value, Some(&b"3"[..]));
}

/// The entries of key `key` in the piece `bytes`.
fn r_entries(bytes: &[u8], key: &[u8]) -> Vec<(ww_tape::index::IndexEntry, ww_tape::StateEntry)> {
    TapeReader::open(bytes)
        .unwrap()
        .key_entries(0, KEYED, key)
        .unwrap()
}

/// Every index block of `bytes` written again with its entries passed through `edit`, at the same length.
fn forge_index(bytes: &mut [u8], edit: impl Fn(&mut ww_tape::index::IndexEntry)) {
    use ww_tape::index::{IndexBlock, IndexHead, index_len, write_index};
    let mut at = vec![];
    let mut rd = ww_tape::FileBlocks::with_recovery(&*bytes, ww_tape::Recovery::Prefix);
    while let Some(Ok(b)) = rd.next_block() {
        let (kind, len) = (b.kind, b.len());
        if kind == BlockKind::Index {
            at.push((rd.offset() as usize, len));
        }
    }
    for (at, len) in at {
        let copy = bytes[at..at + len].to_vec();
        let block = Block::read(&copy).unwrap();
        let ix = IndexBlock::read(&block).unwrap();
        let mut entries: Vec<_> = ix.entries().collect();
        entries.iter_mut().for_each(&edit);
        let head = IndexHead {
            kind: ix.kind(),
            self_offset: ix.self_offset(),
            prev: ix.prev(),
        };
        let id = ww_tape::BlockId::new(block.session, block.seq);
        let tracks = ww_tape::index::track_count(&entries);
        let mut buf = vec![0u8; index_len(entries.len(), tracks)];
        let n = write_index(&mut buf, id, &head, &entries).unwrap();
        assert_eq!(n, len, "the forged index keeps its length");
        bytes[at..at + n].copy_from_slice(&buf[..n]);
    }
}

/// A keyed track's records sit in blocks of their own, marked keyed and repeat; the same key and value again close
/// behind is not written.
#[test]
fn keyed_records_go_in_keyed_blocks_and_repeat_per_key() {
    let defs = defs();
    let (mem, mut chain) = chain();
    assert!(chain.put(&defs, PLAIN, 0, b"plain").unwrap());
    assert!(chain.put_key(&defs, KEYED, 1, b"/k", b"v").unwrap());
    assert!(!chain.put_key(&defs, KEYED, 2, b"/k", b"v").unwrap());
    assert!(chain.put_key(&defs, KEYED, 3, b"/j", b"v").unwrap());
    let closed = chain.close().unwrap();
    let bytes = mem.get(&closed[0].tape).unwrap().unwrap().to_vec().unwrap();
    let mut keyed_records = 0;
    for b in record_blocks(&bytes) {
        let block = Block::read(&b.bytes).unwrap();
        if block.is_keyed() {
            assert!(block.is_repeat());
            for r in block.records() {
                let r = r.unwrap();
                assert_eq!(r.track, ww_tape::RecordTrack::Data(KEYED));
                KeyedItem::read(r.item).unwrap();
                keyed_records += 1;
            }
        } else {
            assert!(
                block
                    .records()
                    .all(|r| r.unwrap().track != ww_tape::RecordTrack::Data(KEYED))
            );
        }
    }
    assert_eq!(keyed_records, 2);
}

/// A put that doesn't fit its track is refused with nothing written: a plain put on a keyed track, a put by key on
/// a plain state track or a stream, a key over the cap.
#[test]
fn puts_that_dont_fit_their_track_are_refused() {
    let defs = defs();
    let (_, mut chain) = chain();
    assert!(matches!(
        chain.put(&defs, KEYED, 0, b"x"),
        Err(WriteError::Describe(DescribeError::Keyed { track })) if track == KEYED
    ));
    assert!(matches!(
        chain.put_key(&defs, PLAIN, 0, b"/k", b"x"),
        Err(WriteError::Describe(DescribeError::NotKeyed { track })) if track == PLAIN
    ));
    assert!(matches!(
        chain.delete_key(&defs, TrackNum::new(1), 0, b"/k"),
        Err(WriteError::Describe(DescribeError::NotState { .. }))
    ));
    let long = [b'k'; STATE_KEY_LEN_MAX + 1];
    assert!(matches!(
        chain.put_key(&defs, KEYED, 0, &long, b"x"),
        Err(WriteError::Describe(DescribeError::KeyTooLong { len })) if len == STATE_KEY_LEN_MAX + 1
    ));
    assert!(chain.put_key(&defs, KEYED, 0, &long[1..], b"x").unwrap());
}

/// A block of keyed puts a reader refuses: without FLAG_REPEAT, with a record of track 0, an item that isn't a key
/// and a value.
#[test]
fn a_bad_keyed_block_is_refused() {
    let bytes = store();
    let keyed = record_blocks(&bytes)
        .into_iter()
        .find(|b| Block::read(&b.bytes).unwrap().is_keyed())
        .unwrap();
    let mut b = keyed.bytes.clone();
    let flags = u16::from_le_bytes([b[6], b[7]]) & !FLAG_REPEAT;
    b[6..8].copy_from_slice(&flags.to_le_bytes());
    reseal(&mut b);
    assert_eq!(Block::read(&b).err(), Some(BlockError::BadRecords));
    // a block of definitions given the kind of keyed puts
    let defs = record_blocks(&bytes)
        .into_iter()
        .find(|b| b.first_track == Some(0))
        .unwrap();
    let mut b = defs.bytes.clone();
    b[5] = BlockKind::KeyedPuts.to_u8();
    let flags = u16::from_le_bytes([b[6], b[7]]) | FLAG_REPEAT;
    b[6..8].copy_from_slice(&flags.to_le_bytes());
    reseal(&mut b);
    assert_eq!(Block::read(&b).err(), Some(BlockError::BadRecords));
    assert_eq!(
        Block::read(&keyed.bytes).unwrap().kind,
        BlockKind::KeyedPuts
    );
    assert_eq!(keyed.bytes[5], 16, "keyed puts are block kind 16");
}

/// Header bit 6, which marked keyed puts before they had a kind of their own, is retired: a reader ignores it on a
/// block of puts and on a block of keyed puts alike.
#[test]
fn retired_keyed_bit_is_ignored() {
    let bytes = store();
    for b in record_blocks(&bytes) {
        let read = Block::read(&b.bytes).unwrap();
        if !read.is_repeat() {
            continue;
        }
        let mut marked = b.bytes.clone();
        marked[6] |= 1 << 6;
        reseal(&mut marked);
        let again = Block::read(&marked).unwrap();
        assert_eq!((again.kind, again.is_keyed()), (read.kind, read.is_keyed()));
        let records = |x: &Block<'_>| {
            x.records()
                .map(|r| r.unwrap().item.to_vec())
                .collect::<Vec<_>>()
        };
        assert_eq!(records(&again), records(&read));
    }
}

/// The CRC of block `b` made good again.
fn reseal(b: &mut [u8]) {
    let n = b.len();
    let crc = crate::range_hash::crc32c(&[&b[..n - 4]]);
    b[n - 4..].copy_from_slice(&crc.to_le_bytes());
}

/// The piece of [store], pinned byte for byte.
#[test]
fn keyed_golden() {
    crate::quiet::golden(&store(), "v1-keyed.hex");
}

/// A chain resumed after keyed puts, without a close: the writer takes the keys it finds as written, so the same key
/// and value again close behind is not written, a new value is, and the reader finds both pieces' records.
#[test]
fn a_resumed_chain_knows_its_keys() {
    let defs = defs();
    let (mem, mut chain) = chain();
    assert!(chain.put_key(&defs, KEYED, 0, b"/k", b"v").unwrap());
    drop(chain);
    let options = ww_tape::ChainOptions {
        tapes: ww_tape::PieceTapes::Each,
        writer: ww_tape::WriterOptions {
            sync: SyncPolicy::None,
            ..Default::default()
        },
        ..Default::default()
    };
    let (mut chain, _) =
        PieceChain::resume_on(mem.clone(), OwnerId([0x4b; 32]), 0, options).unwrap();
    assert!(!chain.put_key(&defs, KEYED, 1, b"/k", b"v").unwrap());
    assert!(chain.put_key(&defs, KEYED, 2, b"/k", b"w").unwrap());
    let closed = chain.close().unwrap();
    let bytes = mem.get(&closed[0].tape).unwrap().unwrap().to_vec().unwrap();
    let r = TapeReader::open(&bytes[..]).unwrap();
    let h = r.key_history(0, KEYED, b"/k", u64::MAX).unwrap();
    let values: Vec<&[u8]> = h
        .items
        .iter()
        .map(|i| i.keyed().unwrap().value.unwrap())
        .collect();
    assert_eq!(values, [&b"w"[..], b"v"]);
}

/// The clocks and tracks a mux defines: [crate::definitions::base]'s clock and tracks, [KEYED] and [PLAIN].
fn mux_batch() -> Vec<Describe<'static>> {
    let mut b = crate::definitions::base(ww_tape::Layout::UNALIGNED)[2..].to_vec();
    b.push(keyed(KEYED.get()));
    b.push(state(PLAIN.get()));
    b
}

/// A state handle on a keyed track puts by key and writes the bytes the same calls write on a chain directly; a
/// plain put there, a put by key on a plain track and a key over the cap are refused at the call.
#[test]
fn a_mux_state_handle_puts_by_key() {
    use ww_tape_mux::{CallError, MuxError, MuxOptions, TapeMux};
    let calls: [(&[u8], Option<&[u8]>); 4] = [
        (b"/a", Some(b"1")),
        (b"/b", Some(b"2")),
        (b"/a", None),
        (b"/b", Some(b"3")),
    ];
    let mut defs = Definitions::new();
    hold(&mut defs, &mux_batch()).unwrap();
    let (direct, mut chain) = chain();
    for (t, (key, value)) in (0i64..).zip(calls) {
        match value {
            Some(v) => chain.put_key(&defs, KEYED, t, key, v),
            None => chain.delete_key(&defs, KEYED, t, key),
        }
        .unwrap();
    }
    chain.close().unwrap();

    let mem = MemPieces::new();
    let mux = TapeMux::create(
        mem.clone(),
        OwnerId([0x4b; 32]),
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
        MuxOptions::default(),
    )
    .unwrap();
    for d in mux_batch() {
        match d {
            Describe::Clock(c) => assert_eq!(mux.define_clock(c.spec()).unwrap(), c.number),
            Describe::Track(t) => assert_eq!(mux.define_track(t.spec()).unwrap(), t.number),
            _ => unreachable!("the batch holds clocks and tracks"),
        }
    }
    let h = mux.state(KEYED).unwrap();
    assert!(matches!(
        h.put(0, b"x"),
        Err(MuxError::Call(CallError::Keyed { keyed: true, .. }))
    ));
    let long = [b'k'; STATE_KEY_LEN_MAX + 1];
    assert!(matches!(
        h.put_key(0, &long, b"x"),
        Err(MuxError::Call(CallError::KeyTooLong { .. }))
    ));
    assert!(matches!(
        mux.state(PLAIN).unwrap().delete_key(0, b"/a"),
        Err(MuxError::Call(CallError::Keyed { keyed: false, .. }))
    ));
    for (t, (key, value)) in (0i64..).zip(calls) {
        match value {
            Some(v) => h.put_key(t, key, v),
            None => h.delete_key(t, key),
        }
        .unwrap();
    }
    mux.close().unwrap();
    assert_eq!(crate::mux::pieces(&mem), crate::mux::pieces(&direct));
}
