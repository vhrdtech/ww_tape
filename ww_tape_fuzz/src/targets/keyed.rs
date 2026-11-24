//! The keyed state targets: `tape-keyed` reads keyed items, key ids, keyed blocks and keyed index entries made from
//! the input; `tape-keyed-hostile` looks keys up in a piece of keyed puts whose keyed blocks and index entries lie
//! behind good CRCs, and resumes it.

use crate::hostile;
use ww_tape::index::{
    IndexBlock, IndexEntry, IndexHead, IndexKind, STATE_TRACK_ID, index_len, write_index,
};
use ww_tape::{
    Block, BlockId, BlockKind, BlockWriter, ClockNum, ClockTime, FLAG_KEYED, FLAG_REPEAT, ItemNum,
    KEY_HEAD_LEN, KeyId, KeyedItem, Medium, MemSink, OpenOptions, ReadError, ResumeError,
    STATE_KEY_LEN_MAX, SessionTag, TapeReader, TapeSink, TapeWriter, TrackId, TrackNum, WriteError,
};

/// `tape-keyed`: the input as a keyed item, as the items of a keyed block, and as the keyed fields of index entries.
///
/// An item that reads encodes to bytes that read the same; its key id is valid and decides only the prefixes the key
/// has. A keyed block reads exactly when each of its items does, and lists each record under its key id. A keyed
/// entry is written exactly when its fields are where a key may be, and reads back as written.
pub fn keyed(data: &[u8]) {
    item(data);
    block(data);
    entries(data);
}

/// The input as one keyed item.
fn item(data: &[u8]) {
    let Ok(k) = KeyedItem::read(data) else {
        return;
    };
    assert!(k.key.len() <= STATE_KEY_LEN_MAX, "a key within the cap");
    let mut again = vec![];
    k.encode_into(&mut again)
        .expect("a key within the cap encodes");
    assert_eq!(again.len(), k.len());
    assert_eq!(
        KeyedItem::read(&again),
        Ok(k),
        "an item reads as it encodes"
    );
    let id = KeyId::of(k.key).expect("a key within the cap has an id");
    assert!(id.is_valid(), "a key's id is valid");
    assert_eq!(k.id(), Some(id));
    if k.key.len() <= KEY_HEAD_LEN {
        assert_eq!(id.key(), Some(k.key), "a short key is whole in its id");
    }
    for n in 0..=k.key.len().min(2 * KEY_HEAD_LEN) {
        assert_ne!(
            id.has_prefix(&k.key[..n]),
            Some(false),
            "the key has its own prefixes"
        );
    }
}

/// The input split into items (a length byte, then that many bytes), written as a keyed block of puts.
fn block(data: &[u8]) {
    let mut items: Vec<&[u8]> = vec![];
    let mut rest = data;
    while let Some((&n, tail)) = rest.split_first() {
        let n = usize::from(n % 32).min(tail.len());
        let (a, b) = tail.split_at(n);
        items.push(a);
        rest = b;
        if items.len() == 16 {
            break;
        }
    }
    if items.is_empty() {
        return;
    }
    let clock = ClockNum::new(1);
    let mut buf = vec![0u8; 4096];
    let id = BlockId::new(SessionTag::writable(1), 0);
    let mut w =
        BlockWriter::new(&mut buf, BlockKind::Records, id, &[clock]).expect("a block starts");
    for (k, it) in items.iter().enumerate() {
        w.push(TrackNum::new(5), ClockTime::new(clock, k as i64), &[*it])
            .expect("a small item fits");
    }
    w.set_flags(FLAG_REPEAT | FLAG_KEYED);
    let len = w.finish();
    let bytes = &buf[..len];
    let all_read = items.iter().all(|i| KeyedItem::read(i).is_ok());
    let Ok(b) = Block::read(bytes) else {
        assert!(!all_read, "a keyed block of keyed items reads");
        return;
    };
    assert!(all_read, "a keyed block reads only when each item does");
    assert!(b.is_keyed(), "the block keeps its keyed flag");
    let listed = IndexEntry::all_of_block(0, &b);
    assert_eq!(listed.len(), items.len(), "an entry per record");
    for (e, it) in listed.iter().zip(&items) {
        let k = KeyedItem::read(it).expect("read above");
        let s = e.state().expect("a state record's entry");
        assert_eq!(e.key, k.id());
        assert_eq!(s.deleted, k.value.is_none());
        assert_eq!(s.hash, ww_tape::ItemHash::of(it));
    }
    let mut ix = vec![0u8; index_len(listed.len(), 0)];
    let head = IndexHead {
        kind: IndexKind::Checkpoint,
        self_offset: 4096,
        prev: None,
    };
    let n = write_index(&mut ix, id, &head, &listed).expect("a block's own entries write");
    let read = Block::read(&ix[..n]).expect("written good");
    let back: Vec<IndexEntry> = IndexBlock::read(&read)
        .expect("an index")
        .entries()
        .collect();
    assert_eq!(back, listed, "keyed entries read back");
}

/// The input as the keyed fields of index entries: 21 bytes each (flags, key length u32, key head, key hash).
fn entries(data: &[u8]) {
    let entries: Vec<IndexEntry> = data
        .as_chunks::<21>()
        .0
        .iter()
        .enumerate()
        .map(|(k, c)| {
            let len = u32::from_le_bytes(c[1..5].try_into().expect("4 bytes")) % 2048;
            let mut head = [0u8; KEY_HEAD_LEN];
            head.copy_from_slice(&c[5..13]);
            IndexEntry {
                offset: 64 * k as u64,
                len: 40,
                kind: if c[0] & 0x80 != 0 {
                    BlockKind::Events
                } else {
                    BlockKind::Records
                },
                level: c[0] & 0x10,
                tree: 0,
                track_id: if c[0] & 0x08 != 0 {
                    TrackId::new(7)
                } else {
                    STATE_TRACK_ID
                },
                number: 5,
                second: if c[0] & 0x04 != 0 { len ^ 1 } else { len },
                first_item: ItemNum::new(0),
                count: 0,
                first_tick: 0,
                last_tick: 0,
                latest_end: 0,
                repeat: true,
                relation_break: false,
                unit: ww_tape::UnitHash::NONE,
                unhashed: false,
                range: None,
                tile: None,
                key: (c[0] & 0x01 != 0).then_some(KeyId {
                    head,
                    len,
                    hash: u64::from_le_bytes(c[13..21].try_into().expect("8 bytes")),
                }),
                deleted: c[0] & 0x02 != 0,
            }
        })
        .collect();
    let valid = entries.iter().all(|e| match e.key {
        None => !e.deleted && !(e.kind == BlockKind::Events && e.level != 0),
        Some(key) => {
            e.track_id == STATE_TRACK_ID
                && e.kind == BlockKind::Records
                && e.level == 0
                && key.len == e.second
                && key.is_valid()
        }
    });
    let mut buf = vec![0u8; index_len(entries.len(), 0)];
    let head = IndexHead {
        kind: IndexKind::Checkpoint,
        self_offset: 1 << 20,
        prev: None,
    };
    let id = BlockId::new(SessionTag::writable(1), 0);
    let n = match write_index(&mut buf, id, &head, &entries) {
        Ok(n) if valid => n,
        Err(ww_tape::IndexError::Invalid(_)) if !valid => return,
        r => unreachable!("write_index gave {r:?} for keyed entries valid: {valid}"),
    };
    let b = Block::read(&buf[..n]).expect("written good");
    let back: Vec<IndexEntry> = IndexBlock::read(&b).expect("an index").entries().collect();
    assert_eq!(back, entries);
    for e in &back {
        if let (Some(key), Some(s)) = (e.key, e.state()) {
            assert_eq!(
                s.slot,
                ww_tape::Slot::Keyed {
                    track: TrackNum::new(5),
                    key
                }
            );
        }
    }
}

/// `tape-keyed-hostile`: a piece of keyed puts and deletions whose keyed blocks and index entries lie behind good
/// CRCs, or which is cut or damaged.
///
/// A key's lookup hands out only a record of that key before the place, its history only that key's records newest
/// first, a listing only live keys in order that may have the prefix and only values of keys that have it. Nothing
/// forged, a lookup that differs from the template's names why. A resume of the piece takes a new key and closes.
pub fn keyed_hostile(data: &[u8]) {
    let a = hostile::keyed_attacked(data);
    let f = a.bytes;
    let (_, on) = hostile::keyed_definitions(None);
    for options in readers() {
        if let Ok(r) = TapeReader::open_with(&f[..], options) {
            for i in 0..r.pieces().len() {
                lookups(&r, i, on.keyed);
            }
            if !a.forged {
                model(&r, f.len() as u64, on.keyed);
            }
        }
    }
    resume(&f);
}

/// A file; a raw medium; a cache too small for the piece; read budgets that run out.
fn readers() -> [OpenOptions; 4] {
    let file = OpenOptions::default();
    [
        file,
        OpenOptions {
            medium: Medium::Raw,
            layout: Some(hostile::keyed_options().layout),
            ..file
        },
        OpenOptions {
            state_cache_max: exact_units::Count::new(1),
            ..file
        },
        OpenOptions {
            state_read_max: exact_units::Bytes::new(300),
            ..file
        },
    ]
}

/// The places a lookup of piece `i` asks at: its first, middle and last state entry, and its end.
fn places(r: &TapeReader<&[u8]>, i: usize) -> Vec<u64> {
    let Ok(entries) = r.state_entries(i) else {
        return vec![u64::MAX];
    };
    let n = entries.len();
    [0, n / 2, n.saturating_sub(1)]
        .into_iter()
        .filter_map(|k| entries.get(k).map(|(e, _)| e.offset.saturating_add(1)))
        .chain([u64::MAX])
        .collect()
}

/// Every key and prefix of the template looked up in piece `i`, each answer checked against what it asks.
fn lookups(r: &TapeReader<&[u8]>, i: usize, track: TrackNum) {
    for place in places(r, i) {
        for key in hostile::keyed_keys() {
            key_lookups(r, i, track, key, place);
        }
        for prefix in hostile::keyed_prefixes() {
            prefix_lookups(r, i, track, prefix, place);
        }
    }
}

/// Key `key` looked up at `place`: only its records, before the place, its history newest first.
fn key_lookups(r: &TapeReader<&[u8]>, i: usize, track: TrackNum, key: &[u8], place: u64) {
    if let Ok(found) = r.key_at(i, track, key, place)
        && let Some(item) = &found.item
    {
        let k = item.keyed().expect("a keyed record");
        assert_eq!(k.key, key, "the key asked");
        assert_eq!(ww_tape::ItemHash::of(&item.item), item.hash);
        assert!(item.offset < place, "before the place asked");
    }
    if let Ok(h) = r.key_history(i, track, key, place) {
        for w in h.items.windows(2) {
            assert!(w[0].offset >= w[1].offset, "newest first");
        }
        for item in &h.items {
            assert_eq!(item.keyed().map(|k| k.key), Some(key));
            assert!(item.offset < place, "a record before the place");
        }
    }
}

/// Prefix `prefix` listed at `place`: live keys in order that may have it, values only of keys that have it, each
/// a key the index lists.
fn prefix_lookups(r: &TapeReader<&[u8]>, i: usize, track: TrackNum, prefix: &[u8], place: u64) {
    let Ok(slots) = r.key_slots(i, track, prefix, place) else {
        return;
    };
    for w in slots.windows(2) {
        assert!(w[0].key < w[1].key, "each key once, in order");
    }
    for s in &slots {
        assert_eq!(s.key.has_prefix(prefix) == Some(true), s.sure);
        assert_ne!(s.key.has_prefix(prefix), Some(false));
        assert!(
            s.entry.offset < place,
            "a key's newest record before the place"
        );
        assert!(!s.entry.deleted, "a live key");
    }
    let Ok(at) = r.keys_at(i, track, prefix, place) else {
        return;
    };
    let first = r.pieces().get(i).is_some_and(|p| {
        p.head_error.is_none() && p.head.as_ref().is_none_or(|(h, _)| h.prev.is_none())
    });
    assert!(!(first && at.not_carried), "a first piece holds every key");
    for (key, item) in &at.items {
        let k = item.keyed().expect("a keyed record");
        assert_eq!(k.key, &key[..]);
        assert!(key.starts_with(prefix), "a key of the prefix");
        assert!(k.value.is_some(), "a live key");
        assert!(item.offset < place, "a value before the place");
        // the record decides a deletion: a key whose newest entry is marked deleted, by a forged mark or with its
        // block lost, lists its value though key_slots leaves it out
        let listed = slots.iter().any(|s| Some(s.key) == k.id());
        let marked = r.key_entries(i, track, key).is_ok_and(|es| {
            es.iter()
                .rev()
                .find(|(e, _)| e.offset < place)
                .is_some_and(|(_, s)| s.deleted)
        });
        assert!(
            listed || marked,
            "a key the index lists, live or marked deleted"
        );
    }
}

/// The template opened once.
fn template_reader() -> &'static TapeReader<&'static [u8]> {
    static T: std::sync::OnceLock<TapeReader<&'static [u8]>> = std::sync::OnceLock::new();
    T.get_or_init(|| TapeReader::open(hostile::keyed_template()).expect("the template opens"))
}

/// A piece damaged where a CRC finds it, or cut, nothing forged: each key's record and each prefix's live values
/// are the template's at the same place, or the answer names why.
fn model(r: &TapeReader<&[u8]>, len: u64, track: TrackNum) {
    let Some(p) = r
        .pieces()
        .first()
        .filter(|p| p.start == 0 && p.version.is_none())
    else {
        return;
    };
    let truth = template_reader();
    let end = p.end.min(len);
    for place in places(truth, 0) {
        let held = if place == u64::MAX { end } else { place };
        for key in hostile::keyed_keys() {
            let want = truth
                .key_at(0, track, key, held)
                .expect("the template's lookup")
                .item;
            let Ok(got) = r.key_at(0, track, key, place) else {
                continue;
            };
            if got.item.as_ref().map(|s| (s.offset, &s.item))
                != want.as_ref().map(|s| (s.offset, &s.item))
            {
                assert!(
                    !got.is_whole(),
                    "{key:?} at {place}: differs, nothing named"
                );
            }
        }
        for prefix in hostile::keyed_prefixes() {
            let want = truth
                .keys_at(0, track, prefix, held)
                .expect("the template's lookup");
            let Ok(got) = r.keys_at(0, track, prefix, place) else {
                continue;
            };
            let offsets = |k: &ww_tape::reader::KeysAt| -> Vec<(Vec<u8>, u64)> {
                k.items
                    .iter()
                    .map(|(key, s)| (key.clone(), s.offset))
                    .collect()
            };
            if offsets(&got) != offsets(&want) {
                assert!(
                    !got.is_whole(),
                    "{prefix:?} at {place}: differs, nothing named"
                );
            }
        }
    }
}

/// A resume of `f`: when it resumes, a new key put and a close a reader finds, the key in force at its end; or the
/// put refused when the piece's carry is lost. A final index refused before the resume stays in the piece: a lookup
/// after it may be refused the same way.
fn resume(f: &[u8]) {
    let refused = |b: &[u8]| match TapeReader::open(b) {
        Ok(r) => r
            .key_at(
                0,
                hostile::keyed_definitions(None).1.keyed,
                b"/resumed",
                u64::MAX,
            )
            .err()
            .filter(|e| matches!(e, ReadError::Index(_)))
            .map(|e| e.to_string()),
        Err(_) => None,
    };
    let refused_before = refused(f);
    let mut sink = MemSink::new();
    sink.write_all(f).expect("memory takes the bytes");
    let tape = sink.tape();
    let Ok((mut w, resumed)) = TapeWriter::resume_on(sink, hostile::keyed_options()) else {
        return;
    };
    let (defs, on) = hostile::keyed_definitions(Some(&resumed));
    match w.put_key(&defs, on.keyed, 1000, b"/resumed", b"v") {
        Ok(_) => {}
        // a carry a crash dropped, which a piece resumed alone can't put again
        Err(WriteError::Resume(ResumeError::KeysUnknown { track })) => {
            assert_eq!(track, on.keyed);
            return;
        }
        Err(e) => panic!("a resumed piece takes puts: {e}"),
    }
    w.close().expect("a resumed piece closes");
    let out = tape.to_vec().expect("the bytes");
    let r = TapeReader::open(&out[..]).expect("a closed piece opens");
    let last = r.pieces().len().checked_sub(1).expect("a piece");
    let found = match r.key_at(last, on.keyed, b"/resumed", u64::MAX) {
        Ok(found) => found,
        Err(e @ ReadError::Index(_)) if refused_before.is_some() => {
            assert_eq!(
                refused_before,
                Some(e.to_string()),
                "refused as before the resume"
            );
            return;
        }
        Err(e) => panic!("a lookup: {e:?}"),
    };
    assert_eq!(
        found
            .item
            .and_then(|s| s.keyed().and_then(|k| k.value.map(<[u8]>::to_vec))),
        Some(b"v".to_vec())
    );
}
