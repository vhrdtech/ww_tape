//! Keyed state records in groups of puts ([PieceChain::put_group], a relayed group): a group's keys and items read
//! all or none wherever a crash cuts it, a resume and the next cut take none of a torn group's keys, and the keys of a
//! group are checked all at once before any of it is written (TRACK-8).

use ww_tape::{
    CarryLimit, DescribeError, GroupItem, GroupPut, GroupRefusal, InputError, KeyId, MemPieces,
    MemSink, PieceChain, Slot, SyncPolicy, TapeReader, TapeSink, TapeWriter, WriteError,
};

use crate::keyed::{KEYED, PLAIN, defs};
use crate::keyed_carry::{
    Model, OWNER, PAIR, boundaries, chain_with, key, keys_of, options, piece_bytes, wide_options,
};

/// The plain state item in force at the end of piece `i` of `bytes`.
fn plain_of(bytes: &[u8], i: usize) -> Option<Vec<u8>> {
    let r = TapeReader::open(bytes).unwrap();
    r.state_at(i, Slot::State { track: PLAIN }, u64::MAX)
        .unwrap()
        .item
        .map(|i| i.item)
}

/// What a reader sees of the tests' chain: the live keys, whether whole, and the plain item.
type Seen = (Model, bool, Option<Vec<u8>>);

fn seen(bytes: &[u8], i: usize) -> Seen {
    let (keys, whole) = keys_of(bytes, i);
    (keys, whole, plain_of(bytes, i))
}

fn model(pairs: &[(&[u8], &[u8])]) -> Model {
    pairs
        .iter()
        .map(|(k, v)| (k.to_vec(), v.to_vec()))
        .collect()
}

/// The puts of the mixed group: a plain item, a changed key, a deletion and a new key.
fn mixed_group() -> [GroupPut<'static>; 4] {
    [
        GroupPut::state(PLAIN, 10, b"after"),
        GroupPut::key(KEYED, 10, b"/a", b"2"),
        GroupPut::delete_key(KEYED, 10, b"/b"),
        GroupPut::key(KEYED, 10, b"/c", b"3"),
    ]
}

/// The keys and plain item before the mixed group, and after it.
fn before_after() -> (Seen, Seen) {
    let before = (
        model(&[(b"/a", b"1"), (b"/b", b"1")]),
        true,
        Some(b"before".to_vec()),
    );
    let after = (
        model(&[(b"/a", b"2"), (b"/c", b"3")]),
        true,
        Some(b"after".to_vec()),
    );
    (before, after)
}

/// A chain with keys `/a` and `/b` and a plain item, then the mixed group, left unclosed; the bytes of its piece
/// before the group.
fn group_chain() -> (MemPieces, usize) {
    let defs = defs();
    let (mem, mut chain) = chain_with(wide_options());
    chain.put_key(&defs, KEYED, 0, b"/a", b"1").unwrap();
    chain.put_key(&defs, KEYED, 1, b"/b", b"1").unwrap();
    chain.put(&defs, PLAIN, 2, b"before").unwrap();
    let start = piece_bytes(&mem, 0).len();
    assert_eq!(chain.put_group(&defs, &mixed_group()).unwrap(), [true; 4]);
    drop(chain);
    (mem, start)
}

/// The points a crash may cut the group at: each block boundary inside it, a byte past each and inside each header.
fn crash_points(bytes: &[u8], start: usize) -> Vec<usize> {
    let mut points: Vec<usize> = boundaries(bytes)
        .into_iter()
        .filter(|b| *b >= start)
        .flat_map(|b| [b, b + 1, b + 33])
        .filter(|p| *p <= bytes.len())
        .collect();
    points.dedup();
    points
}

/// A group of a plain item, a changed key, a deletion and a new key: a reader of the piece cut anywhere inside the
/// group sees all of it or none, and the group takes blocks of its own for the keyed records. A piece cut inside a
/// block names its keys not whole.
#[test]
fn a_mixed_group_with_a_deletion_reads_all_or_none() {
    let (mem, start) = group_chain();
    let whole = piece_bytes(&mem, 0);
    let (before, after) = before_after();
    let points = crash_points(&whole, start);
    // the plain item's block and the keyed block at least, each with a byte and a header past its start
    assert!(points.len() >= 5, "{points:?}");
    assert_eq!(seen(&whole, 0), after);
    let ends = boundaries(&whole);
    for p in points {
        let (keys, is_whole, plain) = seen(&whole[..p], 0);
        let want = if p == whole.len() { &after } else { &before };
        assert_eq!((&keys, &plain), (&want.0, &want.2), "cut at {p}");
        // a torn block at the end may have held more keys: only a cut at a block's end reads whole
        assert_eq!(is_whole, ends.contains(&p), "cut at {p}");
    }
}

/// A crash inside the mixed group, the chain resumed and cut: the resume drops the torn group whole, so the next
/// piece, read alone, carries the keys from before the group, whole, and none of the group's.
#[test]
fn a_torn_group_leaves_no_key_in_the_resume_or_the_carry() {
    let defs = defs();
    let (mem, start) = group_chain();
    let whole = piece_bytes(&mem, 0);
    let (before, after) = before_after();
    for p in crash_points(&whole, start) {
        let (mem, _) = group_chain();
        let mut sink = mem.reopen(&0).unwrap().unwrap();
        sink.truncate(u64::try_from(p).unwrap()).unwrap();
        drop(sink);
        let (mut chain, _) = PieceChain::resume_on(mem.clone(), OWNER, 0, wide_options()).unwrap();
        chain.cut_now(&defs).unwrap();
        let pieces = chain.close().unwrap();
        let last = piece_bytes(&mem, pieces.last().unwrap().tape);
        let want = if p == whole.len() { &after } else { &before };
        assert_eq!(&seen(&last, 0), want, "crash at {p}");
    }
}

/// Two keys of one id in one group, or a key of a live key's id, are refused before anything is written
/// ([DescribeError::KeyCollision]); a group that deletes the live key first puts the other.
#[test]
fn a_group_refuses_a_key_collision() {
    let [a, b] = PAIR;
    assert_eq!(KeyId::of(&a), KeyId::of(&b));
    let defs = defs();
    let (mem, mut chain) = chain_with(wide_options());
    let both = [
        GroupPut::key(KEYED, 0, &a, b"1"),
        GroupPut::key(KEYED, 0, &b, b"2"),
    ];
    let e = chain.put_group(&defs, &both).unwrap_err();
    assert!(
        matches!(e, WriteError::Describe(DescribeError::KeyCollision { track }) if track == KEYED),
        "{e}"
    );
    chain.put_key(&defs, KEYED, 1, &a, b"1").unwrap();
    let len = piece_bytes(&mem, 0).len();
    let other = [
        GroupPut::state(PLAIN, 2, b"p"),
        GroupPut::key(KEYED, 2, &b, b"2"),
    ];
    let e = chain.put_group(&defs, &other).unwrap_err();
    assert!(
        matches!(e, WriteError::Describe(DescribeError::KeyCollision { .. })),
        "{e}"
    );
    assert_eq!(piece_bytes(&mem, 0).len(), len, "nothing written");
    let swap = [
        GroupPut::delete_key(KEYED, 3, &a),
        GroupPut::key(KEYED, 3, &b, b"2"),
    ];
    chain.put_group(&defs, &swap).unwrap();
    let pieces = chain.close().unwrap();
    let (keys, whole) = keys_of(&piece_bytes(&mem, pieces[0].tape), 0);
    assert!(whole);
    assert_eq!(keys, model(&[(&b, b"2")]));
}

/// A group naming one key twice is refused ([GroupRefusal::SlotTwice]), as is an item on a keyed track
/// ([DescribeError::Keyed]) and a key on a plain one ([DescribeError::NotKeyed]); two keys of one track go.
#[test]
fn a_group_refuses_a_key_twice_and_the_wrong_kind() {
    let defs = defs();
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    let twice = [
        GroupPut::key(KEYED, 0, b"/a", b"1"),
        GroupPut::delete_key(KEYED, 0, b"/a"),
    ];
    let e = w.put_group(&defs, &twice).unwrap_err();
    assert!(
        matches!(
            e,
            WriteError::Input(InputError::Group(GroupRefusal::SlotTwice { track })) if track == KEYED
        ),
        "{e}"
    );
    let item = [GroupPut::state(KEYED, 0, b"/a")];
    let e = w.put_group(&defs, &item).unwrap_err();
    assert!(
        matches!(e, WriteError::Describe(DescribeError::Keyed { track }) if track == KEYED),
        "{e}"
    );
    let key = [GroupPut::key(PLAIN, 0, b"/a", b"1")];
    let e = w.put_group(&defs, &key).unwrap_err();
    assert!(
        matches!(e, WriteError::Describe(DescribeError::NotKeyed { track }) if track == PLAIN),
        "{e}"
    );
    let two = [
        GroupPut::key(KEYED, 0, b"/a", b"1"),
        GroupPut::key(KEYED, 0, b"/b", b"1"),
    ];
    assert_eq!(w.put_group(&defs, &two).unwrap(), [true, true]);
}

/// A group whose new keys bring the carry past the state entries is refused whole, naming the limit
/// ([InputError::CarryFull]); the same keys one at a time go up to the limit.
#[test]
fn a_group_past_the_carry_entries_is_refused_whole() {
    let defs = defs();
    let (mem, mut chain) = chain_with(options(40, 64 << 20));
    let keys: Vec<Vec<u8>> = (0..40).map(key).collect();
    let puts: Vec<GroupPut<'_>> = keys
        .iter()
        .map(|k| GroupPut::key(KEYED, 0, k, b"v"))
        .collect();
    let len = piece_bytes(&mem, 0).len();
    let e = chain.put_group(&defs, &puts).unwrap_err();
    assert!(
        matches!(
            e,
            WriteError::Input(InputError::CarryFull {
                max: 40,
                limit: CarryLimit::StateEntries,
                ..
            })
        ),
        "{e}"
    );
    assert_eq!(piece_bytes(&mem, 0).len(), len, "nothing written");
    // as keyed_carry's single puts: the clock and track definitions, the layout, the producer and one put
    chain.put_group(&defs, &puts[..35]).unwrap();
    assert!(chain.put_group(&defs, &puts[35..36]).is_err());
}

/// The bytes of the blocks of the groups a producer puts, `groups` one after the other, after its keys `keys`.
fn producer_blocks(keys: &[(&[u8], &[u8])], groups: &[&[GroupPut<'_>]]) -> Vec<Vec<u8>> {
    let defs = defs();
    let (mem, mut chain) = chain_with(wide_options());
    for (k, v) in keys {
        chain.put_key(&defs, KEYED, 0, k, v).unwrap();
    }
    for g in groups {
        chain.put_group(&defs, g).unwrap();
    }
    let pieces = chain.close().unwrap();
    crate::definitions::record_blocks(&piece_bytes(&mem, pieces[0].tape))
        .into_iter()
        .map(|b| b.bytes)
        .collect()
}

/// A producer's group of a plain item, keys and a deletion, relayed: the relay holds it to its last block and puts
/// it whole, keyed blocks among its blocks, so the relay's piece reads as the producer's.
#[test]
fn a_relayed_group_puts_its_keys() {
    let blocks = producer_blocks(&[(b"/a", b"1"), (b"/b", b"1")], &[&mixed_group()[..]]);
    let group: Vec<&Vec<u8>> = blocks
        .iter()
        .filter(|b| ww_tape::Block::read(b).unwrap().flags & ww_tape::FLAG_GROUP != 0)
        .collect();
    assert!(group.len() >= 2, "a plain block and a keyed one");
    assert!(
        group
            .iter()
            .any(|b| ww_tape::Block::read(b).unwrap().is_keyed())
    );
    let mut defs = defs();
    let (mem, mut chain) = chain_with(wide_options());
    chain.put(&defs, PLAIN, 0, b"before").unwrap();
    for b in &blocks {
        chain.relay_block(&mut defs, b).unwrap();
    }
    assert_eq!(chain.writer().unwrap().relay_held_bytes(), 0);
    let pieces = chain.close().unwrap();
    let (_, after) = before_after();
    assert_eq!(seen(&piece_bytes(&mem, pieces[0].tape), 0), after);
}

/// A relayed group with a key of the id of a key live in the relay's piece is refused at its last block before any
/// of it is written, and dropped ([DescribeError::KeyCollision]).
#[test]
fn a_relayed_group_of_a_colliding_key_is_refused() {
    let [a, b] = PAIR;
    let group = [
        GroupPut::state(PLAIN, 1, b"p"),
        GroupPut::key(KEYED, 1, &b, b"2"),
    ];
    let blocks = producer_blocks(&[], &[&group[..]]);
    let mut defs = defs();
    let (mem, mut chain) = chain_with(wide_options());
    chain.put_key(&defs, KEYED, 0, &a, b"1").unwrap();
    let len = piece_bytes(&mem, 0).len();
    let refused: Vec<_> = blocks
        .iter()
        .filter_map(|b| chain.relay_block(&mut defs, b).err())
        .collect();
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert!(
        matches!(
            refused[0],
            WriteError::Describe(DescribeError::KeyCollision { .. })
        ),
        "{}",
        refused[0]
    );
    assert_eq!(chain.writer().unwrap().relay_held_bytes(), 0);
    assert_eq!(
        piece_bytes(&mem, 0).len(),
        len,
        "nothing of the group written"
    );
}

/// A relayed group of new keys past the carry's state entries is refused at its last block ([InputError::CarryFull])
/// and dropped.
#[test]
fn a_relayed_group_past_the_carry_is_refused() {
    let keys: Vec<Vec<u8>> = (0..40).map(key).collect();
    let puts: Vec<GroupPut<'_>> = keys
        .iter()
        .map(|k| GroupPut::key(KEYED, 0, k, b"v"))
        .collect();
    let blocks = producer_blocks(&[], &[&puts[..]]);
    let mut defs = defs();
    let (_, mut chain) = chain_with(options(40, 64 << 20));
    let refused: Vec<_> = blocks
        .iter()
        .filter_map(|b| chain.relay_block(&mut defs, b).err())
        .collect();
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert!(
        matches!(refused[0], WriteError::Input(InputError::CarryFull { .. })),
        "{}",
        refused[0]
    );
    assert_eq!(chain.writer().unwrap().relay_held_bytes(), 0);
}

/// A group item's length is the record's item as written: a key's head, key and value.
#[test]
fn a_keyed_group_item_counts_its_encoding() {
    let put = GroupPut::key(KEYED, 0, b"/a", b"12");
    assert_eq!(put.item.len(), 1 + 2 + 2);
    assert!(matches!(put.item, GroupItem::Key(_)));
}

/// A [ww_tape_mux::GroupHandle] on a keyed and a plain track: it refuses at the call an item on the keyed track, a
/// key on the plain one and a key past [ww_tape::STATE_KEY_LEN_MAX]; it puts a group's keys and deletion with its
/// item, and the writer refuses a group of a colliding key whole, the handle's next flush naming it.
#[test]
fn a_mux_group_puts_keys_and_deletions() {
    use ww_tape_mux::{CallError, MuxError, MuxOptions, TapeMux};
    let [a, b] = PAIR;
    let mem = MemPieces::new();
    let mux = TapeMux::create(
        mem.clone(),
        OWNER,
        0,
        &[],
        wide_options(),
        MuxOptions::default(),
    )
    .unwrap();
    // the tests' definitions, numbered as the mux numbers them
    let mut batch = crate::definitions::base(ww_tape::Layout::UNALIGNED).split_off(2);
    batch.extend([
        crate::keyed::keyed(KEYED.get()),
        crate::definitions::state(PLAIN.get()),
    ]);
    for d in batch {
        match d {
            ww_tape::Describe::Clock(c) => {
                assert_eq!(mux.define_clock(c.spec()).unwrap(), c.number);
            }
            ww_tape::Describe::Track(t) => {
                assert_eq!(mux.define_track(t.spec()).unwrap(), t.number);
            }
            _ => unreachable!("base holds clocks and tracks past its first two"),
        }
    }
    let group = mux.group(&[KEYED, PLAIN]).unwrap();
    let wrong = |puts: &[GroupPut<'_>]| match group.put_group(puts) {
        Err(MuxError::Call(e)) => e,
        r => panic!("{r:?}"),
    };
    assert_eq!(
        wrong(&[GroupPut::state(KEYED, 0, b"x")]),
        CallError::Keyed {
            track: KEYED,
            keyed: true
        }
    );
    assert_eq!(
        wrong(&[GroupPut::key(PLAIN, 0, b"/a", b"1")]),
        CallError::Keyed {
            track: PLAIN,
            keyed: false
        }
    );
    let long = vec![b'k'; ww_tape::STATE_KEY_LEN_MAX + 1];
    assert!(matches!(
        wrong(&[GroupPut::key(KEYED, 0, &long, b"1")]),
        CallError::KeyTooLong { .. }
    ));
    let first = [
        GroupPut::state(PLAIN, 0, b"before"),
        GroupPut::key(KEYED, 0, b"/a", b"1"),
        GroupPut::key(KEYED, 0, b"/b", b"1"),
        GroupPut::key(KEYED, 0, &a, b"1"),
    ];
    group.put_group(&first).unwrap();
    group.put_group(&mixed_group()).unwrap();
    group.flush().unwrap();
    group
        .put_group(&[GroupPut::key(KEYED, 20, &b, b"2")])
        .unwrap();
    let e = group.flush().unwrap_err();
    assert!(
        matches!(
            e,
            MuxError::Refused(WriteError::Describe(DescribeError::KeyCollision { .. }))
        ),
        "{e:?}"
    );
    let pieces = mux.close().unwrap();
    let mut want = before_after().1;
    want.0.insert(a.to_vec(), b"1".to_vec());
    assert_eq!(seen(&piece_bytes(&mem, pieces[0].tape), 0), want);
}

/// A chain with keys `/a` and `/b` and a plain item, the mixed group, then data blocks past a checkpoint, dropped
/// unclosed: the group lies inside the piece. The offsets of the group's blocks.
fn mid_piece_group() -> (MemPieces, Vec<usize>) {
    let defs = defs();
    let (mem, mut chain) = chain_with(crate::keyed_carry::checkpointed());
    chain.put_key(&defs, KEYED, 0, b"/a", b"1").unwrap();
    chain.put_key(&defs, KEYED, 1, b"/b", b"1").unwrap();
    chain.put(&defs, PLAIN, 2, b"before").unwrap();
    let start = piece_bytes(&mem, 0).len();
    chain.put_group(&defs, &mixed_group()).unwrap();
    let end = piece_bytes(&mem, 0).len();
    let session = ww_tape::SessionTag::writable(0x6461_7461);
    for n in 0..60 {
        let block = crate::pieces::records(ww_tape::BlockId::new(session, n), 6000 + n as i64);
        chain.adopt_block(&defs, &block).unwrap();
    }
    drop(chain);
    let blocks = boundaries(&piece_bytes(&mem, 0))
        .into_iter()
        .filter(|b| (start..end).contains(b))
        .collect();
    (mem, blocks)
}

/// A group inside a piece with one of its blocks damaged, the plain one or the keyed one: a reader takes none of the
/// group; the chain's resume takes none of its keys either and, as the damage may have hidden others, doubts the
/// keyed track, so the next cut carries no key of it as whole and a reader of the next piece names its keys not whole.
#[test]
fn a_damaged_group_inside_a_piece_leaves_its_keys_doubted() {
    let defs = defs();
    let (_, blocks) = mid_piece_group();
    assert!(blocks.len() >= 2, "{blocks:?}");
    let (before, _) = before_after();
    for at in blocks {
        let (mem, _) = mid_piece_group();
        crate::keyed_carry::flip(&mem, 0, at + 33);
        assert_eq!(
            seen(&piece_bytes(&mem, 0), 0),
            (before.0.clone(), false, before.2.clone()),
            "damage at {at}"
        );
        let (mut chain, _) =
            PieceChain::resume_on(mem.clone(), OWNER, 0, crate::keyed_carry::checkpointed())
                .unwrap();
        chain.cut_now(&defs).unwrap();
        let pieces = chain.close().unwrap();
        let next = piece_bytes(&mem, pieces.last().unwrap().tape);
        let (keys, whole) = keys_of(&next, 0);
        assert!(!whole, "damage at {at}: {keys:?}");
        assert!(
            !keys.contains_key(&b"/c"[..]),
            "damage at {at}: a key of the dropped group"
        );
        // the plain item of the dropped group isn't carried: the one before it, or none where the damage hid it
        let plain = plain_of(&next, 0);
        assert_ne!(plain.as_deref(), Some(&b"after"[..]), "damage at {at}");
    }
}

/// A chain with keys `/a` and `/b`, a plain item and a block of data, a group of three blocks (a key, the plain
/// item, a deletion), then blocks of data past a checkpoint, the first right after the group, dropped unclosed; the group's blocks as offsets and ends, then the
/// group's block `k` given header flags `flags(old)` behind a good CRC. The piece, and the group's block boundaries.
fn forged_group(k: usize, flags: impl Fn(u16) -> u16) -> (MemPieces, Vec<usize>) {
    let defs = defs();
    let (mem, mut chain) = chain_with(crate::keyed_carry::checkpointed());
    chain.put_key(&defs, KEYED, 0, b"/a", b"1").unwrap();
    chain.put_key(&defs, KEYED, 1, b"/b", b"1").unwrap();
    chain.put(&defs, PLAIN, 2, b"before").unwrap();
    // a block of data before the group, so the definitions of those after it lie close behind
    let session = ww_tape::SessionTag::writable(0x6461_7461);
    let block = crate::pieces::records(ww_tape::BlockId::new(session, 0), 5999);
    chain.adopt_block(&defs, &block).unwrap();
    let start = piece_bytes(&mem, 0).len();
    let group = [
        GroupPut::key(KEYED, 10, b"/a", b"2"),
        GroupPut::state(PLAIN, 10, b"after"),
        GroupPut::delete_key(KEYED, 10, b"/b"),
    ];
    chain.put_group(&defs, &group).unwrap();
    let end = piece_bytes(&mem, 0).len();
    for n in 1..60 {
        let block = crate::pieces::records(ww_tape::BlockId::new(session, n), 6000 + n as i64);
        chain.adopt_block(&defs, &block).unwrap();
    }
    drop(chain);
    let mut bytes = piece_bytes(&mem, 0);
    let ends: Vec<usize> = boundaries(&bytes)
        .into_iter()
        .filter(|b| (start..=end).contains(b))
        .collect();
    assert_eq!(ends.len(), 4, "the group takes three blocks: {ends:?}");
    let (b0, b1) = (ends[k], ends[k + 1]);
    let old = u16::from_le_bytes([bytes[b0 + 6], bytes[b0 + 7]]);
    bytes[b0 + 6..b0 + 8].copy_from_slice(&flags(old).to_le_bytes());
    let crc = crate::range_hash::crc32c(&[&bytes[b0..b1 - 4]]);
    bytes[b1 - 4..b1].copy_from_slice(&crc.to_le_bytes());
    let mut sink = mem.reopen(&0).unwrap().unwrap();
    sink.truncate(0).unwrap();
    sink.write_all(&bytes).unwrap();
    drop(sink);
    (mem, ends)
}

/// The offsets of the blocks a follower of the piece `bytes`, in a file of its own named for `name`, hands out.
fn followed(bytes: &[u8], name: &str) -> Vec<usize> {
    use std::io::Write;
    use ww_tape_fs::follow::{ChainFollower, FollowOptions};
    let dir = crate::pieces::scratch(&format!("keyed_groups_{name}"));
    let path = dir.join("tape.wwt");
    std::fs::File::create(&path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
    let mut f = ChainFollower::new(
        &path,
        FollowOptions {
            tapes: ww_tape::PieceTapes::One,
            ..FollowOptions::default()
        },
    )
    .unwrap();
    let mut got = Vec::new();
    while let Some(b) = f.try_next().unwrap() {
        got.push(usize::try_from(b.offset).unwrap());
    }
    std::fs::remove_dir_all(&dir).unwrap();
    got
}

/// The forged piece of [forged_group] read by a scan and through its index alike, then resumed and cut: a carry
/// read whole holds what a reader of the piece holds.
fn carry_agrees(mem: &MemPieces, what: &str) {
    let defs = defs();
    let bytes = piece_bytes(mem, 0);
    let piece = seen(&bytes, 0);
    // a scan of the piece, without its index, decides the group as a lookup through the index does
    let raw = ww_tape::OpenOptions {
        medium: ww_tape::Medium::Raw,
        ..Default::default()
    };
    let scan = TapeReader::open_with(&bytes[..], raw).unwrap();
    let scanned = scan.keys_at(0, KEYED, b"", u64::MAX).unwrap();
    let scanned: Model = scanned
        .items
        .iter()
        .map(|(k, s)| (k.clone(), s.keyed().unwrap().value.unwrap().to_vec()))
        .collect();
    assert_eq!(scanned, piece.0, "{what}: the scan and the index differ");
    let (mut chain, _) =
        PieceChain::resume_on(mem.clone(), OWNER, 0, crate::keyed_carry::checkpointed()).unwrap();
    chain.cut_now(&defs).unwrap();
    let pieces = chain.close().unwrap();
    let next = seen(&piece_bytes(mem, pieces.last().unwrap().tape), 0);
    if next.1 {
        assert_eq!(
            next.0, piece.0,
            "{what}: the carry reads whole but differs from the piece"
        );
    }
    assert!(
        next.2.is_none() || next.2 == piece.2,
        "{what}: the carried item differs from the piece's"
    );
}

/// The middle block of a group forged without `FLAG_GROUP`, behind a good CRC: a reader drops the group's other
/// blocks, a follower hands out none of them, and the resume takes none of them either, so a carry marked whole
/// agrees with the reader.
#[test]
fn a_forged_block_inside_a_group_breaks_it_for_the_resume_too() {
    let (mem, ends) = forged_group(1, |f| f & !ww_tape::FLAG_GROUP);
    // a follower hands out the forged block alone, none of the group's others
    let out = followed(&piece_bytes(&mem, 0), "forged_middle");
    assert!(out.contains(&ends[1]), "{out:?}");
    assert!(
        !out.contains(&ends[0]) && !out.contains(&ends[2]),
        "{out:?}"
    );
    let (keys, _, _) = seen(&piece_bytes(&mem, 0), 0);
    assert_eq!(
        keys,
        model(&[(b"/a", b"1"), (b"/b", b"1")]),
        "a reader drops the group's keys"
    );
    carry_agrees(&mem, "middle block outside the group");
}

/// The last block of a group forged with `FLAG_WITH_NEXT`, then a block of data: the run ends with the data, so a
/// reader takes the group whole, a follower hands it out, and the resume takes it too, so the carry holds the
/// group's keys.
#[test]
fn a_group_ended_by_a_block_of_data_is_taken_whole_by_the_resume() {
    let (mem, ends) = forged_group(2, |f| f | ww_tape::FLAG_WITH_NEXT);
    // a follower hands out the group whole, with the block of data that ends its run
    let out = followed(&piece_bytes(&mem, 0), "forged_last");
    assert!(ends[..3].iter().all(|b| out.contains(b)), "{out:?}");
    let (keys, _, plain) = seen(&piece_bytes(&mem, 0), 0);
    assert_eq!(
        keys,
        model(&[(b"/a", b"2")]),
        "a reader takes the group whole"
    );
    assert_eq!(plain.as_deref(), Some(&b"after"[..]));
    carry_agrees(&mem, "last block marked with next");
}
