//! Keyed state tracks across pieces: a cut carries every live key, a chain that cuts by itself or resumes after a
//! crash keeps them, a put past the carry's limit and a key of another live key's id are refused, and a reader names
//! a piece that doesn't carry its keys (TRACK-8).

use std::collections::BTreeMap;

use ww_tape::index::IndexPolicy;
use ww_tape::piece::TrackStart;
use ww_tape::{
    CarryLimit, ChainOptions, DescribeError, InputError, KeyId, MemPieces, MemSink, NearPolicy,
    OwnerId, PieceChain, PieceTapes, ResumeError, SyncPolicy, TapeReader, TapeSink, TapeWriter,
    WriteError, WriterOptions,
};

use crate::keyed::{KEYED, PLAIN, defs};

const OWNER: OwnerId = OwnerId([0x4c; 32]);

/// The live keys a test expects, with their values.
type Model = BTreeMap<Vec<u8>, Vec<u8>>;

fn options(entries_max: u64, kept_bytes_max: u64) -> ChainOptions {
    ChainOptions {
        tapes: PieceTapes::Each,
        writer: WriterOptions {
            sync: SyncPolicy::None,
            index: IndexPolicy {
                state_entries_max: exact_units::Count::new(entries_max),
                ..IndexPolicy::default()
            },
            near: NearPolicy {
                kept_bytes_max: exact_units::Bytes::new(kept_bytes_max),
                ..NearPolicy::default()
            },
            ..Default::default()
        },
        ..Default::default()
    }
}

fn wide_options() -> ChainOptions {
    options(1 << 20, 64 << 20)
}

fn chain_with(options: ChainOptions) -> (MemPieces, PieceChain<MemPieces>) {
    let mem = MemPieces::new();
    let chain = PieceChain::create_on(mem.clone(), OWNER, 0, &[], options).unwrap();
    (mem, chain)
}

/// Key `n` of the tests: `/p/` then its number, so the keys sort by it.
fn key(n: u32) -> Vec<u8> {
    format!("/p/{n:05}").into_bytes()
}

/// A value of 40 bytes for key `n`, put the `round`th time.
fn value(n: u32, round: u32) -> Vec<u8> {
    format!("{n:05}-{round:05}-").repeat(4).into_bytes()
}

/// Put `key` = `value` (a deletion for None) at `ticks` into the chain and the model.
fn apply(
    chain: &mut PieceChain<MemPieces>,
    model: &mut Model,
    ticks: i64,
    key: &[u8],
    value: Option<&[u8]>,
) {
    let defs = defs();
    match value {
        Some(v) => {
            chain.put_key(&defs, KEYED, ticks, key, v).unwrap();
            model.insert(key.to_vec(), v.to_vec());
        }
        None => {
            chain.delete_key(&defs, KEYED, ticks, key).unwrap();
            model.remove(key);
        }
    }
}

/// The live keys of [KEYED] in piece `i` of `bytes` as of its end, and whether the reader holds them whole.
fn keys_of(bytes: &[u8], i: usize) -> (Model, bool) {
    let r = TapeReader::open(bytes).unwrap();
    let at = r.keys_at(i, KEYED, b"", u64::MAX).unwrap();
    let keys = at
        .items
        .iter()
        .map(|(k, s)| (k.clone(), s.keyed().unwrap().value.unwrap().to_vec()))
        .collect();
    (keys, at.is_whole())
}

fn piece_bytes(mem: &MemPieces, key: u64) -> Vec<u8> {
    mem.get(&key).unwrap().unwrap().to_vec().unwrap()
}

/// The head row of [KEYED] in the only piece of `bytes`.
fn keyed_row(bytes: &[u8]) -> Option<TrackStart> {
    let r = TapeReader::open(bytes).unwrap();
    let (_, rows) = r.pieces()[0].head.as_ref().unwrap();
    rows.iter().find(|t| t.track_number == KEYED).copied()
}

/// 300 keys of 40 bytes, a third of them changed and a tenth deleted: more keys than one carried block holds.
fn fill(chain: &mut PieceChain<MemPieces>, model: &mut Model) {
    for n in 0..300 {
        apply(chain, model, i64::from(n), &key(n), Some(&value(n, 0)));
    }
    for n in (0..300).step_by(3) {
        apply(
            chain,
            model,
            1000 + i64::from(n),
            &key(n),
            Some(&value(n, 1)),
        );
    }
    for n in (0..300).step_by(10) {
        apply(chain, model, 2000 + i64::from(n), &key(n), None);
    }
}

/// A piece cut after keyed puts carries every live key, so read alone it holds them all, whole, and its head row
/// says so; the keys take several carried blocks.
#[test]
fn a_cut_carries_every_live_key() {
    let defs = defs();
    let (mem, mut chain) = chain_with(wide_options());
    let mut model = Model::new();
    fill(&mut chain, &mut model);
    chain.cut_now(&defs).unwrap();
    let at_cut = model.clone();
    apply(&mut chain, &mut model, 5000, b"/q", Some(b"after"));
    let pieces = chain.close().unwrap();
    assert_eq!(pieces.len(), 2);
    assert_eq!(pieces[0].piece.keys_unknown, 0);
    let second = piece_bytes(&mem, pieces[1].tape);
    assert!(keyed_row(&second).unwrap().keys_carried);
    let (keys, whole) = keys_of(&second, 0);
    assert!(whole);
    assert_eq!(keys, model);
    // as of the first block after the carry: the keys in force at the cut
    let r = TapeReader::open(&second[..]).unwrap();
    let q = r.key_entries(0, KEYED, b"/q").unwrap();
    let at = r.keys_at(0, KEYED, b"", q[0].0.offset).unwrap();
    assert!(at.is_whole());
    let keys: Model = at
        .items
        .iter()
        .map(|(k, s)| (k.clone(), s.keyed().unwrap().value.unwrap().to_vec()))
        .collect();
    assert_eq!(keys, at_cut);
    let carried_blocks: std::collections::BTreeSet<u64> =
        at.items.values().map(|s| s.offset).collect();
    assert!(carried_blocks.len() > 1, "{carried_blocks:?}");
    assert!(r.verify(0).unwrap().holds);
}

/// A chain whose pieces fill with state entries cuts by itself inside a put ([PieceChain::put_key]'s room): every
/// piece read alone holds the keys in force at its close, whole.
#[test]
fn a_chain_that_cuts_by_itself_carries_every_key() {
    let (mem, mut chain) = chain_with(options(96, 64 << 20));
    let mut model = Model::new();
    // the model at each cut: the keys in force at the close of the piece before
    let mut at_close: Vec<Model> = Vec::new();
    for round in 0..12 {
        for n in 0..40 {
            let before = chain.closed().len();
            let snapshot = model.clone();
            let ticks = i64::from(round * 100 + n);
            let v = (n + round) % 7 != 0;
            let v = v.then(|| value(n, round));
            apply(&mut chain, &mut model, ticks, &key(n), v.as_deref());
            if chain.closed().len() > before {
                at_close.push(snapshot);
            }
        }
    }
    let pieces = chain.close().unwrap();
    at_close.push(model.clone());
    assert!(pieces.len() > 3, "{}", pieces.len());
    assert_eq!(pieces.len(), at_close.len());
    for (i, p) in pieces.iter().enumerate() {
        assert_eq!(p.piece.keys_unknown, 0, "piece {i}");
        let bytes = piece_bytes(&mem, p.tape);
        let (keys, whole) = keys_of(&bytes, 0);
        assert!(whole, "piece {i}");
        assert_eq!(keys, at_close[i], "piece {i}");
    }
}

/// A chain cut, its keys changed and deleted in the second piece, closed, resumed (the closed piece written on
/// again) and cut: the keys of the closed piece, read through its index over those of the piece before, are carried
/// whole into the next piece; a key deleted in the closed piece stays deleted.
#[test]
fn a_closed_piece_written_on_again_carries_its_keys() {
    let defs = defs();
    let (mem, mut chain) = chain_with(wide_options());
    let mut model = Model::new();
    fill(&mut chain, &mut model);
    chain.cut_now(&defs).unwrap();
    for n in (1..300).step_by(7) {
        apply(&mut chain, &mut model, 3000 + i64::from(n), &key(n), None);
    }
    apply(&mut chain, &mut model, 4000, &key(2), Some(b"changed"));
    chain.close().unwrap();
    let (mut chain, _) = PieceChain::resume_on(mem.clone(), OWNER, 0, wide_options()).unwrap();
    apply(&mut chain, &mut model, 6000, b"/r", Some(b"again"));
    chain.cut_now(&defs).unwrap();
    apply(&mut chain, &mut model, 6001, b"/s", Some(b"next"));
    let pieces = chain.close().unwrap();
    let last = pieces.last().unwrap();
    let (keys, whole) = keys_of(&piece_bytes(&mem, last.tape), 0);
    assert!(whole);
    assert_eq!(keys, model);
}

/// The block boundaries of the piece `bytes` from its start, each block's end, as its headers name them.
fn boundaries(bytes: &[u8]) -> Vec<usize> {
    let mut out = vec![0];
    let mut at = 0usize;
    while let Some(h) = bytes.get(at..at + 32) {
        let len = usize::try_from(u32::from_le_bytes(h[8..12].try_into().unwrap())).unwrap();
        if len < 32 || h[..4] == [0; 4] {
            break;
        }
        at += len;
        if at > bytes.len() {
            break;
        }
        out.push(at);
    }
    out
}

/// The chain of the crash test: keys put and changed, a cut, a key put after it; the second piece left unclosed.
fn crash_chain() -> (MemPieces, Model) {
    let defs = defs();
    let (mem, mut chain) = chain_with(wide_options());
    let mut model = Model::new();
    fill(&mut chain, &mut model);
    chain.cut_now(&defs).unwrap();
    apply(&mut chain, &mut model, 5000, b"/q", Some(b"after"));
    drop(chain);
    (mem, model)
}

/// The second piece of [crash_chain] written without a crash, the put after the cut made when `with_q`, then one
/// more put and the close: what a resume after a crash must write.
fn uninterrupted(with_q: bool) -> Vec<u8> {
    let defs = defs();
    let (mem, mut chain) = chain_with(wide_options());
    let mut model = Model::new();
    fill(&mut chain, &mut model);
    chain.cut_now(&defs).unwrap();
    if with_q {
        apply(&mut chain, &mut model, 5000, b"/q", Some(b"after"));
    }
    apply(&mut chain, &mut model, 7000, b"/y", Some(b"resumed"));
    let pieces = chain.close().unwrap();
    piece_bytes(&mem, pieces[1].tape)
}

/// A crash at every block boundary of the piece a cut started, and inside each block: the chain's resume gives a
/// piece that, read alone after one more put, holds every live key, whole. A part of them read as whole would be
/// silent loss; a reader of a piece that lacks them says so ([a_piece_without_its_keys_reads_as_not_whole]). The
/// piece is byte for byte the one written without the crash.
#[test]
fn a_crash_anywhere_in_a_cut_keeps_every_key_or_says_so() {
    let (mem, _) = crash_chain();
    let whole = piece_bytes(&mem, 1);
    let mut points: Vec<usize> = Vec::new();
    for b in boundaries(&whole) {
        points.extend([b, b + 1, b + 33]);
    }
    points.retain(|p| *p <= whole.len());
    points.dedup();
    assert!(points.len() > 6, "{points:?}");
    for p in points {
        let (mem, mut model) = crash_chain();
        let mut sink = mem.reopen(&1).unwrap().unwrap();
        sink.truncate(u64::try_from(p).unwrap()).unwrap();
        drop(sink);
        let (mut chain, _) = PieceChain::resume_on(mem.clone(), OWNER, 0, wide_options()).unwrap();
        apply(&mut chain, &mut model, 7000, b"/y", Some(b"resumed"));
        let pieces = chain.close().unwrap();
        let last = pieces.last().unwrap();
        let bytes = piece_bytes(&mem, last.tape);
        let (keys, is_whole) = keys_of(&bytes, 0);
        // the chain's resume knows every key: through the piece before when the carry was lost
        assert!(is_whole, "crash at {p}");
        // the put after the cut survives only where the crash came after it
        let with_q = keys.contains_key(&b"/q"[..]);
        if !with_q {
            model.remove(&b"/q"[..]);
        }
        assert_eq!(keys, model, "crash at {p}");
        assert!(bytes == uninterrupted(with_q), "crash at {p}: other bytes");
    }
}

/// A crash at every block boundary of the close a cut writes into the first piece, and inside each block, before
/// the second piece was made: the chain's resume closes the first piece or goes on in it, and the piece it ends in,
/// read alone after one more put, holds every live key, whole.
#[test]
fn a_crash_inside_the_close_of_a_cut_keeps_every_key() {
    let defs = defs();
    let make = || {
        let (mem, mut chain) = chain_with(wide_options());
        let mut model = Model::new();
        fill(&mut chain, &mut model);
        chain.cut_now(&defs).unwrap();
        drop(chain);
        (mem, model)
    };
    let (mem, _) = make();
    let first = piece_bytes(&mem, 0);
    let ends = boundaries(&first);
    // the close starts with the first block of the final index
    let close = ends
        .iter()
        .position(|at| {
            first
                .get(*at..)
                .and_then(|b| ww_tape::Block::read(b).ok())
                .is_some_and(|b| b.kind == ww_tape::BlockKind::Index)
        })
        .unwrap();
    let mut points: Vec<usize> = Vec::new();
    for b in &ends[close..] {
        points.extend([*b, b + 1, b + 33]);
    }
    points.retain(|p| *p <= first.len());
    points.dedup();
    assert!(points.len() > 3, "{points:?}");
    for p in points {
        let (mut mem, mut model) = make();
        let mut sink = mem.reopen(&0).unwrap().unwrap();
        sink.truncate(u64::try_from(p).unwrap()).unwrap();
        drop(sink);
        // the second piece never made: its sink sealed, then taken out
        mem.reopen(&1).unwrap().unwrap().close().unwrap();
        assert!(ww_tape::PieceStore::remove(&mut mem, &1).unwrap());
        let (mut chain, _) = PieceChain::resume_on(mem.clone(), OWNER, 0, wide_options()).unwrap();
        apply(&mut chain, &mut model, 7000, b"/y", Some(b"resumed"));
        let pieces = chain.close().unwrap();
        let last = pieces.last().unwrap();
        assert_eq!(last.piece.keys_unknown, 0, "crash at {p}");
        let (keys, whole) = keys_of(&piece_bytes(&mem, last.tape), 0);
        assert!(whole, "crash at {p}");
        assert_eq!(keys, model, "crash at {p}");
    }
}

/// [wide_options] with a checkpoint every 1 KiB: a block that doesn't read stands by its entry.
fn checkpointed() -> ChainOptions {
    let mut o = wide_options();
    o.writer.index.bytes = exact_units::Bytes::new(1024);
    o
}

/// A chain, cut when `cut`, then in its last piece a deletion of a live key and data blocks after it past a
/// checkpoint, dropped unclosed. The last piece's key, its bytes and the offset of the deletion's block.
fn deletion_chain(cut: bool) -> (MemPieces, u64, Vec<u8>, usize) {
    let defs = defs();
    let (mem, mut chain) = chain_with(checkpointed());
    let mut model = Model::new();
    fill(&mut chain, &mut model);
    if cut {
        chain.cut_now(&defs).unwrap();
    }
    apply(&mut chain, &mut model, 5000, b"/q", Some(b"before"));
    apply(&mut chain, &mut model, 5001, &key(1), None);
    let session = ww_tape::SessionTag::writable(0x6461_7461);
    for n in 0..60 {
        let block = crate::pieces::records(ww_tape::BlockId::new(session, n), 6000 + n as i64);
        chain.adopt_block(&defs, &block).unwrap();
    }
    drop(chain);
    let last = u64::from(cut);
    let bytes = piece_bytes(&mem, last);
    let r = TapeReader::open(&bytes[..]).unwrap();
    let at = r
        .key_entries(0, KEYED, &key(1))
        .unwrap()
        .last()
        .unwrap()
        .0
        .offset;
    (mem, last, bytes, usize::try_from(at).unwrap())
}

/// The chain of [deletion_chain] with its deletion's block damaged, resumed, a key put, cut and closed: the closed
/// pieces.
fn resume_damaged(cut: bool) -> (MemPieces, Vec<ww_tape::ChainPiece<u64>>) {
    let defs = defs();
    let (mem, last, mut bytes, at) = deletion_chain(cut);
    bytes[at + 33] ^= 0x55;
    let mut sink = mem.reopen(&last).unwrap().unwrap();
    sink.truncate(0).unwrap();
    sink.write_all(&bytes).unwrap();
    drop(sink);
    let (mut chain, _) = PieceChain::resume_on(mem.clone(), OWNER, 0, checkpointed()).unwrap();
    chain
        .put_key(&defs, KEYED, 7000, b"/y", b"resumed")
        .unwrap();
    chain.cut_now(&defs).unwrap();
    let pieces = chain.close().unwrap();
    (mem, pieces)
}

/// A resume of a first piece whose block of a deletion doesn't read, listed in a checkpoint: the keys may miss the
/// deletion, so the next cut doesn't mark them carried and counts the track ([Closed::keys_unknown](ww_tape::Closed));
/// a reader of the next piece names them not whole.
#[test]
fn a_keyed_block_that_doesnt_read_leaves_its_keys_unknown() {
    let (mem, pieces) = resume_damaged(false);
    assert_eq!(pieces.len(), 2);
    assert_eq!(pieces[0].piece.keys_unknown, 1);
    let (_, whole) = keys_of(&piece_bytes(&mem, pieces[1].tape), 0);
    assert!(!whole);
}

/// The same after a cut, resumed through the chain: the keys of the piece before don't make the doubted keys of the
/// resumed piece complete, so the deleted key doesn't come back as whole.
#[test]
fn keys_before_a_damaged_piece_dont_make_its_keys_whole() {
    let (mem, pieces) = resume_damaged(true);
    assert_eq!(pieces.len(), 3);
    assert_eq!(pieces[1].piece.keys_unknown, 1);
    let (_, whole) = keys_of(&piece_bytes(&mem, pieces[2].tape), 0);
    assert!(!whole);
}

/// The offsets of the blocks of kind `kind` in the piece `bytes`.
fn blocks_of(bytes: &[u8], kind: ww_tape::BlockKind) -> Vec<usize> {
    boundaries(bytes)
        .into_iter()
        .filter(|at| {
            bytes
                .get(*at..)
                .and_then(|b| ww_tape::Block::read(b).ok())
                .is_some_and(|b| b.kind == kind)
        })
        .collect()
}

/// Piece `key` of `mem` with byte `at` flipped.
fn flip(mem: &MemPieces, key: u64, at: usize) {
    let mut bytes = piece_bytes(mem, key);
    bytes[at] ^= 0x55;
    let mut sink = mem.reopen(&key).unwrap().unwrap();
    sink.truncate(0).unwrap();
    sink.write_all(&bytes).unwrap();
    drop(sink);
}

/// A closed piece written on again alone whose final index doesn't read (a chain's resume refuses it): its keys
/// can't be read through the index, so the next cut counts the track ([Closed::keys_unknown](ww_tape::Closed)) and
/// a reader of the next piece names them not whole.
#[test]
fn a_closed_piece_whose_index_doesnt_read_leaves_its_keys_unknown() {
    let defs = defs();
    let (mem, mut chain) = chain_with(wide_options());
    let mut model = Model::new();
    fill(&mut chain, &mut model);
    chain.cut_now(&defs).unwrap();
    apply(&mut chain, &mut model, 3000, &key(1), None);
    chain.close().unwrap();
    let mut bytes = piece_bytes(&mem, 1);
    let index = blocks_of(&bytes, ww_tape::BlockKind::Index);
    bytes[index.last().unwrap() + 40] ^= 0x55;
    let mut sink = MemSink::new();
    sink.write_all(&bytes).unwrap();
    let (mut w, _) = TapeWriter::resume_on(sink, wide_options().writer).unwrap();
    w.put_key(&defs, KEYED, 7000, b"/y", b"resumed").unwrap();
    let next = MemSink::new();
    let tape = next.tape();
    let (closed, mut w) = w.cut_on(&defs, next).map_err(|e| e.error).unwrap();
    assert_eq!(closed.keys_unknown, 1);
    w.put_key(&defs, KEYED, 7001, b"/z", b"next").unwrap();
    w.close().unwrap();
    let (_, whole) = keys_of(&tape.to_vec().unwrap(), 0);
    assert!(!whole);
}

/// A piece whose checkpoint doesn't read, with good blocks after it: the bytes are junk to a resume, which may hide
/// keyed records, so the next cut counts the track and a reader of the next piece names its keys not whole.
#[test]
fn junk_in_a_resumed_piece_leaves_its_keys_unknown() {
    let defs = defs();
    let (mem, last, bytes, at) = deletion_chain(false);
    let checkpoint = blocks_of(&bytes, ww_tape::BlockKind::Index)
        .into_iter()
        .find(|c| *c > at)
        .unwrap();
    flip(&mem, last, checkpoint + 40);
    let (mut chain, _) = PieceChain::resume_on(mem.clone(), OWNER, 0, checkpointed()).unwrap();
    chain
        .put_key(&defs, KEYED, 7000, b"/y", b"resumed")
        .unwrap();
    chain.cut_now(&defs).unwrap();
    let pieces = chain.close().unwrap();
    assert_eq!(pieces.len(), 2);
    assert_eq!(pieces[0].piece.keys_unknown, 1);
    let (_, whole) = keys_of(&piece_bytes(&mem, pieces[1].tape), 0);
    assert!(!whole);
}

/// A crash right after a cut drops the piece's carry, and a resume of that piece alone, without its chain, can't
/// put the keys again: the first call is refused ([ResumeError::KeysUnknown]).
#[test]
fn a_lone_resume_without_the_keys_refuses_to_carry() {
    let defs = defs();
    let (mem, _) = crash_chain();
    let bytes = piece_bytes(&mem, 1);
    let head_end = boundaries(&bytes)[1];
    let mut sink = MemSink::new();
    sink.write_all(&bytes[..head_end]).unwrap();
    let (mut w, _) = TapeWriter::resume_on(sink, wide_options().writer).unwrap();
    let e = w.put_key(&defs, KEYED, 9000, b"/x", b"1").unwrap_err();
    assert!(
        matches!(e, WriteError::Resume(ResumeError::KeysUnknown { track }) if track == KEYED),
        "{e}"
    );
}

/// A chain with a plain state track, and a keyed one with a key when `with_key`, cut, dropped right after the cut:
/// the second piece holds its head alone, the carry lost. The bytes of that head, its rows in order.
fn cut_head(with_key: bool) -> (MemPieces, Vec<u8>, Vec<TrackStart>) {
    let defs = defs();
    let (mem, mut chain) = chain_with(wide_options());
    chain.put(&defs, PLAIN, 1, b"idle").unwrap();
    if with_key {
        chain.put_key(&defs, KEYED, 2, b"/a", b"1").unwrap();
    }
    chain.cut_now(&defs).unwrap();
    drop(chain);
    let bytes = piece_bytes(&mem, 1);
    let head = bytes[..boundaries(&bytes)[1]].to_vec();
    let rows = TapeReader::open(&head[..]).unwrap().pieces()[0]
        .head
        .clone()
        .unwrap()
        .1;
    (mem, head, rows)
}

/// The head `head` with row `i` marked keys carried behind a good CRC: 32 bytes of header, 120 of the head's fixed
/// part, rows of 96 bytes with their flags at 12.
fn mark_row(head: &mut [u8], i: usize) {
    head[32 + 120 + 96 * i + 12] |= 0x04;
    let n = head.len();
    let crc = crate::range_hash::crc32c(&[&head[..n - 4]]);
    head[n - 4..].copy_from_slice(&crc.to_le_bytes());
}

/// A keys-carried mark on the row of a track that isn't keyed, behind a good CRC, with the carry lost: the writer
/// passes it over as a reader does, alone and through its chain, and the next call goes.
#[test]
fn a_keys_carried_mark_on_a_plain_row_is_passed_over() {
    let defs = defs();
    let (mem, mut head, rows) = cut_head(false);
    assert!(rows.iter().all(|r| r.track_number != KEYED));
    let i = rows.iter().position(|r| r.track_number == PLAIN).unwrap();
    assert!(!rows[i].keys_carried);
    mark_row(&mut head, i);
    let r = TapeReader::open(&head[..]).unwrap();
    assert!(r.pieces()[0].head.as_ref().unwrap().1[i].keys_carried);
    let mut sink = MemSink::new();
    sink.write_all(&head).unwrap();
    let (mut w, _) = TapeWriter::resume_on(sink, wide_options().writer).unwrap();
    w.put(&defs, PLAIN, 10, b"run").unwrap();
    let mut piece = mem.reopen(&1).unwrap().unwrap();
    piece.truncate(0).unwrap();
    piece.write_all(&head).unwrap();
    drop(piece);
    let (mut chain, _) = PieceChain::resume_on(mem.clone(), OWNER, 0, wide_options()).unwrap();
    chain.put(&defs, PLAIN, 10, b"run").unwrap();
    chain.close().unwrap();
}

/// The tests' definitions with track 10 another track than [KEYED], plain: [PLAIN] without the keyed track.
fn plain_defs() -> ww_tape::Definitions {
    let ww_tape::Describe::Track(t) = crate::definitions::state(KEYED.get()) else {
        unreachable!("state() gives a track")
    };
    let other = ww_tape::Describe::Track(ww_tape::TrackDef {
        key: ww_tape::track_key(0xae, &[ww_tape::Segment::Index(KEYED.get())]),
        ..t
    });
    let mut plain = crate::definitions::base_defs().clone();
    crate::definitions::hold(&mut plain, &[other, crate::definitions::state(PLAIN.get())]).unwrap();
    plain
}

/// A call after a crash dropped a carry of keys, given definitions without the keyed track, is refused naming the
/// track ([ResumeError::KeysUndefined]): the carry can't be put again without it.
#[test]
fn a_lost_carry_without_its_keyed_track_is_refused_by_name() {
    let (mem, _, _) = cut_head(true);
    // track 10 another track than the keyed one the head names: the keyed track not held
    let plain = plain_defs();
    let (mut chain, _) = PieceChain::resume_on(mem.clone(), OWNER, 0, wide_options()).unwrap();
    let e = chain.put(&plain, PLAIN, 10, b"run").unwrap_err();
    assert!(
        matches!(e, WriteError::Resume(ResumeError::KeysUndefined { track }) if track == KEYED),
        "{e}"
    );
    chain.put(&defs(), PLAIN, 10, b"run").unwrap();
}

/// A piece after another whose head row of a keyed track isn't marked carried: a reader names its keys as not
/// carried, a key it doesn't hold as not found for sure, and the writer's next cut doesn't mark the row either.
#[test]
fn a_piece_without_its_keys_reads_as_not_whole() {
    let defs = defs();
    let owner = OWNER;
    let first = ww_tape::piece::PieceId::first(&owner);
    let head = ww_tape::PieceHead {
        ident: ww_tape::PieceIdent {
            id: ww_tape::piece::PieceId([7; 16]),
            owner,
            seq: 1,
        },
        prev: Some((ww_tape::ContentHash([1; 32]), first)),
        prev_session: ww_tape::SessionTag::NONE,
        integrity: ww_tape::Integrity::Full,
    };
    let ww_tape::Describe::Track(t) = crate::keyed::keyed(KEYED.get()) else {
        unreachable!("keyed() gives a track")
    };
    let (id, own) = (t.id(), t.owner);
    let row = TrackStart {
        track_id: id,
        track_number: KEYED,
        next_item: ww_tape::ItemNum::new(0),
        from_tick: i64::MIN,
        owner: OwnerId(own),
        hash: ww_tape::TrackHash::START,
        incomplete: false,
        not_carried: false,
        keys_carried: false,
    };
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    w.set_session(ww_tape::SessionTag::writable(0x6b65_7973))
        .unwrap();
    w.begin_piece(&head, &[row]).unwrap();
    w.put_key(&defs, KEYED, 0, b"/a", b"1").unwrap();
    let (closed, next) = w.cut_on(&defs, MemSink::new()).unwrap();
    assert_eq!(closed.keys_unknown, 1);
    assert!(!closed.tracks.iter().any(|t| t.keys_carried));
    let tape = next.sink().tape();
    next.close().unwrap();
    let bytes = tape.to_vec().unwrap();
    let r = TapeReader::open(&bytes[..]).unwrap();
    let at = r.keys_at(0, KEYED, b"", u64::MAX).unwrap();
    assert!(at.not_carried && !at.is_whole());
    assert!(at.items.contains_key(&b"/a"[..]));
    let missing = r.key_at(0, KEYED, b"/zz", u64::MAX).unwrap();
    assert!(missing.item.is_none() && missing.not_carried && !missing.is_whole());
    let found = r.key_at(0, KEYED, b"/a", u64::MAX).unwrap();
    assert!(found.item.is_some() && !found.not_carried && found.is_whole());
}

/// A put by key that would bring the keys every cut carries past the index policy's state entries is refused,
/// naming the limit; a change of a live key and a deletion still go, and a deletion makes room for a new key.
#[test]
fn a_put_past_the_carry_entries_is_refused() {
    let defs = defs();
    let max = 40;
    let (_, mut chain) = chain_with(options(max, 64 << 20));
    let mut model = Model::new();
    let mut n = 0;
    let refused = loop {
        match chain.put_key(&defs, KEYED, i64::from(n), &key(n), b"v") {
            Ok(_) => {
                model.insert(key(n), b"v".to_vec());
                n += 1;
            }
            Err(e) => break e,
        }
        assert!(n < 100);
    };
    let WriteError::Input(InputError::CarryFull {
        needed,
        max: m,
        limit,
    }) = refused
    else {
        panic!("{refused}");
    };
    assert_eq!((m, limit), (40, CarryLimit::StateEntries));
    assert!(needed > m);
    assert!(
        refused.to_string().contains("state_entries_max"),
        "{refused}"
    );
    // the definitions of the clock and the track, a record per key, the layout, the producer and one put
    assert_eq!(n, 40 - 2 - 2 - 1);
    // the piece goes on: a changed value, a deletion, then a new key in its room; cuts keep them
    for round in 0..5 {
        apply(&mut chain, &mut model, 100 + round, &key(0), Some(b"w"));
    }
    apply(&mut chain, &mut model, 200, &key(1), None);
    apply(&mut chain, &mut model, 201, &key(n), Some(b"new"));
    assert!(chain.put_key(&defs, KEYED, 202, &key(n + 1), b"v").is_err());
    // at the limit a track the carry doesn't hold yet has no room for its definitions until keys are deleted
    let e = chain.put(&defs, PLAIN, 203, b"p").unwrap_err();
    assert!(
        matches!(e, WriteError::Input(InputError::CarriedEntries { .. })),
        "{e}"
    );
    apply(&mut chain, &mut model, 204, &key(2), None);
    apply(&mut chain, &mut model, 205, &key(3), None);
    chain.put(&defs, PLAIN, 206, b"p").unwrap();
}

/// A put by key whose live keys' items would take more than the bytes the writer keeps is refused, naming that
/// limit; plain state items are forgotten first to make room for the keys.
#[test]
fn a_put_past_the_kept_bytes_is_refused() {
    let defs = defs();
    let (_, mut chain) = chain_with(options(1 << 20, 1000));
    let big = vec![7u8; 300];
    for n in 0..3 {
        chain
            .put_key(&defs, KEYED, n, &key(n as u32), &big)
            .unwrap();
    }
    let e = chain.put_key(&defs, KEYED, 3, &key(3), &big).unwrap_err();
    assert!(
        matches!(
            e,
            WriteError::Input(InputError::CarryFull {
                max: 1000,
                limit: CarryLimit::KeptBytes,
                ..
            })
        ),
        "{e}"
    );
    // a shorter value of a live key goes
    chain.put_key(&defs, KEYED, 4, &key(0), b"short").unwrap();
    chain.put_key(&defs, KEYED, 5, &key(3), &big).unwrap();
}

/// A closed piece written on again: a plain item put before a call reads the piece's keys through its index, then a
/// cut, which reads them. They count under the bytes kept, so the plain item is forgotten and the cut doesn't carry
/// it.
#[test]
fn keys_read_after_a_close_count_under_the_kept_bytes() {
    let defs = defs();
    let o = options(1 << 20, 1000);
    let (mem, mut chain) = chain_with(o.clone());
    let big = vec![7u8; 300];
    for n in 0..3 {
        chain
            .put_key(&defs, KEYED, n, &key(n as u32), &big)
            .unwrap();
    }
    chain.close().unwrap();
    let (mut chain, _) = PieceChain::resume_on(mem.clone(), OWNER, 0, o).unwrap();
    // definitions without a keyed track: the keys of the closed piece aren't read yet
    chain.put(&plain_defs(), PLAIN, 10, &[1u8; 200]).unwrap();
    // the cut reads the keys and carries them first
    chain.cut_now(&defs).unwrap();
    let pieces = chain.close().unwrap();
    let second = piece_bytes(&mem, pieces[1].tape);
    let r = TapeReader::open(&second[..]).unwrap();
    let plain = r
        .state_at(0, ww_tape::Slot::State { track: PLAIN }, u64::MAX)
        .unwrap();
    assert!(
        plain.item.is_none(),
        "the plain item forgotten, not carried"
    );
    let (keys, whole) = keys_of(&second, 0);
    assert!(whole);
    assert_eq!(keys.len(), 3);
}

/// Two keys found to share a [KeyId]: the same 8-byte head, length 16 and first 8 bytes of BLAKE3,
/// `0x9f4d_f4fa_77ba_1a80`. Found by Pollard's rho with distinguished points (20 low zero bits) over
/// `x -> first 8 bytes, little endian, of BLAKE3("/store/k" ++ x as 8 bytes little endian)`, on 2 threads, about
/// 2^32 hashes.
const PAIR: [[u8; 16]; 2] = [
    [
        0x2f, 0x73, 0x74, 0x6f, 0x72, 0x65, 0x2f, 0x6b, 0xdc, 0x4f, 0x5a, 0x16, 0xd0, 0x38, 0x52,
        0x28,
    ],
    [
        0x2f, 0x73, 0x74, 0x6f, 0x72, 0x65, 0x2f, 0x6b, 0xc1, 0x81, 0xe2, 0x3c, 0x3f, 0x48, 0xf9,
        0x13,
    ],
];

/// A put by key whose key has the id of another live key is refused ([DescribeError::KeyCollision]): a listing
/// would show only one of them. Once the other key is deleted the put goes, and so does the same key again; a
/// producer's block is checked as a put is ([a_producers_block_of_a_colliding_key_is_refused]).
#[test]
fn a_key_of_another_live_keys_id_is_refused() {
    let [a, b] = PAIR;
    assert_ne!(a, b);
    assert_eq!(KeyId::of(&a), KeyId::of(&b));
    let defs = defs();
    let (_, mut chain) = chain_with(wide_options());
    chain.put_key(&defs, KEYED, 0, &a, b"1").unwrap();
    chain.put_key(&defs, KEYED, 1, &a, b"2").unwrap();
    let e = chain.put_key(&defs, KEYED, 2, &b, b"3").unwrap_err();
    assert!(
        matches!(e, WriteError::Describe(DescribeError::KeyCollision { track }) if track == KEYED),
        "{e}"
    );
    chain.delete_key(&defs, KEYED, 3, &b).unwrap();
    assert!(chain.put_key(&defs, KEYED, 4, &b, b"3").is_err());
    chain.delete_key(&defs, KEYED, 5, &a).unwrap();
    chain.put_key(&defs, KEYED, 6, &b, b"3").unwrap();
    let e = chain.put_key(&defs, KEYED, 7, &a, b"4").unwrap_err();
    assert!(matches!(
        e,
        WriteError::Describe(DescribeError::KeyCollision { .. })
    ));
}

/// The blocks of keyed records a producer writes for `puts`, each a key and its value: what a relay passes on.
fn keyed_blocks(puts: &[(&[u8], &[u8])]) -> Vec<Vec<u8>> {
    let defs = defs();
    let (mem, mut chain) = chain_with(wide_options());
    for (i, (k, v)) in puts.iter().enumerate() {
        chain
            .put_key(&defs, KEYED, i64::try_from(i).unwrap(), k, v)
            .unwrap();
    }
    let pieces = chain.close().unwrap();
    crate::definitions::record_blocks(&piece_bytes(&mem, pieces[0].tape))
        .into_iter()
        .filter(|b| ww_tape::Block::read(&b.bytes).unwrap().is_keyed())
        .map(|b| b.bytes)
        .collect()
}

/// Pass the producer's block `block` into `chain` as `how` says: relayed or adopted.
fn pass(
    chain: &mut PieceChain<MemPieces>,
    how: &str,
    block: &[u8],
) -> Result<(), WriteError<<MemPieces as ww_tape::PieceStore>::Error>> {
    let defs = defs();
    match how {
        "relay" => chain.relay_block(&mut defs.clone(), block),
        _ => chain.adopt_block(&defs, block),
    }
}

/// A producer's block of keyed records relayed, adopted or put whose key has the id of another live key is refused
/// as a put by key is ([DescribeError::KeyCollision]).
#[test]
fn a_producers_block_of_a_colliding_key_is_refused() {
    let [a, b] = PAIR;
    let defs = defs();
    let blocks = keyed_blocks(&[(&b, b"3")]);
    assert_eq!(blocks.len(), 1);
    let collides = |e: &WriteError<_>| matches!(e, WriteError::Describe(DescribeError::KeyCollision { track }) if *track == KEYED);
    for how in ["relay", "adopt"] {
        let (_, mut chain) = chain_with(wide_options());
        chain.put_key(&defs, KEYED, 0, &a, b"1").unwrap();
        let e = pass(&mut chain, how, &blocks[0]).unwrap_err();
        assert!(collides(&e), "{how}: {e}");
    }
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    w.put_key(&defs, KEYED, 0, &a, b"1").unwrap();
    let e = w.put_block(&defs, &blocks[0]).unwrap_err();
    assert!(collides(&e), "put_block: {e}");
}

/// A producer's block of keyed records relayed, adopted or put that would bring the carry past the index policy's
/// state entries is refused as a put by key is ([InputError::CarryFull]).
#[test]
fn a_producers_block_past_the_carry_limit_is_refused() {
    let defs = defs();
    let blocks = keyed_blocks(&[(b"/new", b"v")]);
    let full = |e: &WriteError<_>| {
        matches!(
            e,
            WriteError::Input(InputError::CarryFull {
                limit: CarryLimit::StateEntries,
                ..
            })
        )
    };
    // the keys a carry of 40 entries holds: the definitions of the clock and the track, the layout, the producer
    // and one put
    let keys = 40 - 2 - 2 - 1;
    for how in ["relay", "adopt"] {
        let (_, mut chain) = chain_with(options(40, 64 << 20));
        for n in 0..keys {
            chain
                .put_key(&defs, KEYED, i64::from(n), &key(n), b"v")
                .unwrap();
        }
        let e = pass(&mut chain, how, &blocks[0]).unwrap_err();
        assert!(full(&e), "{how}: {e}");
    }
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    w.set_index_policy(IndexPolicy {
        state_entries_max: exact_units::Count::new(40),
        ..IndexPolicy::default()
    })
    .unwrap();
    for n in 0..keys {
        w.put_key(&defs, KEYED, i64::from(n), &key(n), b"v")
            .unwrap();
    }
    let e = w.put_block(&defs, &blocks[0]).unwrap_err();
    assert!(full(&e), "put_block: {e}");
}

/// A cut of a small store: the second piece's head row marked, its carry of the live keys after the track's
/// definition, a put after it; pinned byte for byte.
#[test]
fn keyed_carried_golden() {
    let defs = defs();
    let (mem, mut chain) = chain_with(wide_options());
    let mut model = Model::new();
    apply(&mut chain, &mut model, 0, b"/a", Some(b"1"));
    apply(&mut chain, &mut model, 1, b"/b", Some(b"2"));
    apply(
        &mut chain,
        &mut model,
        2,
        b"/eventfull/layouts/main",
        Some(b"L"),
    );
    apply(&mut chain, &mut model, 3, b"/b", None);
    chain.cut_now(&defs).unwrap();
    apply(&mut chain, &mut model, 4, b"/c", Some(b"3"));
    let pieces = chain.close().unwrap();
    let second = piece_bytes(&mem, pieces[1].tape);
    assert!(keyed_row(&second).unwrap().keys_carried);
    assert_eq!(keys_of(&second, 0), (model, true));
    crate::quiet::golden(&second, "v1-keyed-carried.hex");
}
