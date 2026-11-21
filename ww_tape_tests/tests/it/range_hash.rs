//! The hash of a range of a track's chunks from the index ([TapeReader::range_hash]) against a fold that reads them.

use std::path::Path;
use ww_tape::TrackId;
use ww_tape::index::{RECORDS_TRACK_ID, STATE_TRACK_ID};

use crate::definitions::base_defs;
use crate::pieces::{chunk, column, records};
use ww_tape::index::IndexPolicy;
use ww_tape::piece::payload_hash;
use ww_tape::{
    ChainOptions, Closed, CutPolicy, FileBlocks, MemTapes, OwnerId, PieceChain, PieceTapes,
    SyncPolicy, TapeReader, TrackHash, WriterOptions,
};

/// xorshift64: the tests' random choices, the same on every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Track `t`'s id: [chunk] writes track `t` as number `t + 1` with this id.
pub(crate) fn id_of(t: u64) -> TrackId {
    crate::pieces::chunk_id(t)
}

/// A chain of 60 chunks on tracks 0 to 2 in a random order, with record blocks between, cut every 7 chunks: the
/// closed pieces and the bytes of each file, in order.
pub(crate) fn write(files: PieceTapes, seed: u64) -> (Vec<Closed>, Vec<Vec<u8>>) {
    let mut rng = Rng(seed);
    let options = ChainOptions {
        tapes: files,
        writer: WriterOptions {
            sync: SyncPolicy::None,
            index: IndexPolicy {
                bytes: 2048,
                interval: None,
                ..IndexPolicy::default()
            },
            producer: "range_hash".to_owned(),
            ..WriterOptions::default()
        },
        cut: CutPolicy {
            chunks: 7,
            ..CutPolicy::default()
        },
    };
    let mut c = PieceChain::create_on(MemTapes::new(), OwnerId([4; 32]), 0, &[], options).unwrap();
    let defs = base_defs();
    let mut next = [0u64; 3];
    for n in 0..60 {
        let t = rng.below(3);
        let (h, b) = chunk(t, next[t as usize]);
        next[t as usize] += 1;
        c.write_chunk(defs, &h, &column(&b)).unwrap();
        if n % 4 == 0 {
            let w = c.writer().unwrap();
            let id = w.next_id();
            w.write_block(defs, &records(id, n)).unwrap();
        }
    }
    let mem = c.tapes().clone();
    let done = c.close().unwrap();
    let mut keys: Vec<u64> = vec![];
    for f in &done {
        if !keys.contains(&f.tape) {
            keys.push(f.tape);
        }
    }
    let bytes = keys
        .iter()
        .map(|k| mem.get(k).unwrap().unwrap().to_vec().unwrap())
        .collect();
    (done.into_iter().map(|f| f.piece).collect(), bytes)
}

/// The hash of track `t`'s chunks holding an item of `items` in `files`, read block by block, and the items they
/// hold.
fn read_fold(files: &[&[u8]], t: u64, items: &std::ops::Range<u64>) -> (TrackHash, u64) {
    let mut h = TrackHash::START;
    let mut n = 0;
    for bytes in files {
        let mut rd = FileBlocks::new(*bytes);
        while let Some(b) = rd.next_block() {
            let b = b.unwrap();
            let Some(c) = b.chunk() else { continue };
            let head = c.head();
            let end = head.first_item + u64::from(head.count);
            if head.track_id == id_of(t) && head.first_item < items.end && end > items.start {
                h = h.chunk(&c, &payload_hash(c.payload()));
                n += u64::from(head.count);
            }
        }
    }
    (h, n)
}

/// The range hash of track `t` over `files`, each read by a reader of its own, folded on from one to the next.
fn index_fold(files: &[&[u8]], t: u64, items: &std::ops::Range<u64>) -> (TrackHash, u64, u64) {
    let mut h = TrackHash::START;
    let (mut n, mut read) = (0, 0);
    for bytes in files {
        let r = TapeReader::open(*bytes).unwrap();
        let got = r
            .range_hash(id_of(t), OwnerId::NONE, items.clone(), h)
            .unwrap();
        h = got.hash;
        n += got.items.end - got.items.start;
        read += got.read;
    }
    (h, n, read)
}

/// Random ranges of random tracks hash the same from the index as by reading the chunks, with no chunk read: in one
/// file of several pieces, in a file per piece, and with the first pieces gone.
#[test]
fn a_range_hash_from_the_index_is_the_read_fold() {
    let mut rng = Rng(0x7a6e_5eed);
    for files in [PieceTapes::One, PieceTapes::Each] {
        let (done, bytes) = write(files, 0x5eed + files as u64);
        assert!(done.len() >= 5, "{files:?}: {} pieces", done.len());
        let all: Vec<&[u8]> = bytes.iter().map(Vec::as_slice).collect();
        // the first pieces gone: from piece 2 of the one file, or from the third file
        let tail: Vec<&[u8]> = match files {
            PieceTapes::One => {
                let r = TapeReader::open(&bytes[0][..]).unwrap();
                vec![&bytes[0][r.pieces()[2].start as usize..]]
            }
            PieceTapes::Each => all[2..].to_vec(),
        };
        for set in [&all, &tail] {
            for _ in 0..40 {
                let t = rng.below(3);
                let a = rng.below(1200);
                let items = a..a + 1 + rng.below(600);
                let (want, n) = read_fold(set, t, &items);
                let (got, m, read) = index_fold(set, t, &items);
                assert_eq!(got, want, "{files:?} track {t} {items:?}");
                assert_eq!(m, n, "{files:?} track {t} {items:?}: items");
                assert_eq!(read, 0, "{files:?}: every unit hash from the index");
            }
        }
        // the whole of a track of chunks alone is its hash as the writer reports it
        for t in 0..3 {
            let (got, ..) = index_fold(&all, t, &(0..u64::MAX));
            let want = done
                .last()
                .unwrap()
                .tracks
                .iter()
                .find(|s| s.track_id == id_of(t))
                .unwrap()
                .hash;
            assert_eq!(got, want, "{files:?} track {t}: the writer's hash");
        }
    }
}

/// A piece whose final index is lost folds by reading its chunks, to the same hash.
#[test]
fn a_range_hash_reads_the_chunks_of_a_piece_without_an_index() {
    let (_, bytes) = write(PieceTapes::One, 0xdead);
    let mut damaged = bytes[0].clone();
    let r = TapeReader::open(&damaged[..]).unwrap();
    let p = &r.pieces()[1];
    let (fin, _) = p.close.as_ref().unwrap();
    let at = (p.start + fin.final_index.unwrap()) as usize;
    let end = (p.start + p.close_offset.unwrap()) as usize;
    damaged[at..end].fill(0);
    for t in 0..3 {
        let items = 0..u64::MAX;
        let (want, _) = read_fold(&[&bytes[0]], t, &items);
        let r = TapeReader::open(&damaged[..]).unwrap();
        let got = r
            .range_hash(id_of(t), OwnerId::NONE, items, TrackHash::START)
            .unwrap();
        assert_eq!(got.hash, want, "track {t}");
        assert!(got.read > 0, "track {t}: piece 1's chunks read");
    }
}

/// CRC-32C, as the format's blocks carry it.
pub(crate) fn crc32c(parts: &[&[u8]]) -> u32 {
    let crc = crc::Crc::<u32>::new(&crc::CRC_32_ISCSI);
    let mut d = crc.digest();
    for p in parts {
        d.update(p);
    }
    d.finalize()
}

/// The final index of piece `i` of `bytes` with every entry of track `t` replaced by `change` of it, the list's CRC,
/// the head CRC and the block CRC made good again.
pub(crate) fn rewrite_final_entries(
    bytes: &mut [u8],
    i: usize,
    t: u64,
    change: impl FnMut(usize, &mut [u8]),
) {
    rewrite_final_entries_of(bytes, i, id_of(t), change);
}

/// [rewrite_final_entries] of the track of id `track_id`.
pub(crate) fn rewrite_final_entries_of(
    bytes: &mut [u8],
    i: usize,
    track_id: TrackId,
    mut change: impl FnMut(usize, &mut [u8]),
) {
    use ww_tape::index::{ENTRY_LEN, INDEX_FIXED_LEN, IndexBlock, TRACK_LEN};
    let r = TapeReader::open(&bytes[..]).unwrap();
    let p = &r.pieces()[i];
    let at = (p.start + p.close.as_ref().unwrap().0.final_index.unwrap()) as usize;
    let len = ww_tape::block_len(&bytes[at..]).unwrap();
    let block = &mut bytes[at..at + len];
    let ix = IndexBlock::read_head(block).unwrap();
    let k = ix.tracks().position(|l| l.track_id == track_id).unwrap();
    let list = ix.tracks().nth(k).unwrap();
    let (from, n) = ix.track_range(&list);
    let entries_at = IndexBlock::head_len(block).unwrap();
    for (j, e) in block[from..from + n]
        .as_chunks_mut::<ENTRY_LEN>()
        .0
        .iter_mut()
        .enumerate()
    {
        change(j, e);
    }
    let crc = crc32c(&[&block[from..from + n]]);
    let dir = ww_tape::HEADER_LEN + INDEX_FIXED_LEN + k * TRACK_LEN + 16;
    block[dir..dir + 4].copy_from_slice(&crc.to_le_bytes());
    let head = crc32c(&[&block[..28], &block[32..entries_at]]);
    block[28..32].copy_from_slice(&head.to_le_bytes());
    let crc = crc32c(&[&block[..len - 4]]);
    block[len - 4..].copy_from_slice(&crc.to_le_bytes());
}

/// A final index whose entries of a track all name its first chunk, its CRCs good, is refused, never read and
/// folded once per entry: the range hash reads the piece's chunks instead, each once.
#[test]
fn an_index_naming_a_chunk_many_times_is_refused() {
    use ww_tape::index::ENTRY_LEN;
    let (_, bytes) = write(PieceTapes::One, 0xd0b1e);
    let good = TapeReader::open(&bytes[0][..]).unwrap();
    let t = 1;
    let entries = good.track_entries(1, id_of(t)).unwrap();
    assert!(entries.len() > 2, "{} entries", entries.len());
    let want = good
        .range_hash(id_of(t), OwnerId::NONE, 0..u64::MAX, TrackHash::START)
        .unwrap();
    let mut crafted = bytes[0].clone();
    let mut first = [0u8; ENTRY_LEN];
    rewrite_final_entries(&mut crafted, 1, t, |j, e| match j {
        0 => first.copy_from_slice(e),
        _ => e.copy_from_slice(&first),
    });
    let r = TapeReader::open(&crafted[..]).unwrap();
    assert!(matches!(
        r.track_entries(1, id_of(t)),
        Err(ww_tape::ReadError::Index(
            ww_tape::index::IndexError::BadEntries
        ))
    ));
    let got = r
        .range_hash(id_of(t), OwnerId::NONE, 0..u64::MAX, TrackHash::START)
        .unwrap();
    assert_eq!((got.hash, got.chunks), (want.hash, want.chunks));
    assert!(got.read > 0, "piece 1's chunks read");
}

/// `bytes` with piece `i`'s final index zeroed: the piece reads by its tail, its index doesn't.
fn index_lost(bytes: &[u8], i: usize) -> Vec<u8> {
    let r = TapeReader::open(bytes).unwrap();
    let p = &r.pieces()[i];
    let (fin, _) = p.close.as_ref().unwrap();
    let at = (p.start + fin.final_index.unwrap()) as usize;
    let end = (p.start + p.close_offset.unwrap()) as usize;
    let mut out = bytes.to_vec();
    out[at..end].fill(0);
    out
}

/// Without its index a piece is walked: a damaged chunk of the track whose head reads is an error, as by the index,
/// never a hash of the chunks around it; another track's hash is whole, the lost index counted as skipped bytes.
#[test]
fn a_range_hash_without_an_index_reports_a_damaged_chunk() {
    let (_, bytes) = write(PieceTapes::One, 0x5eed);
    let good = TapeReader::open(&bytes[0][..]).unwrap();
    let (t, c) = (0..3)
        .find_map(|t| {
            let e = good.track_entries(1, id_of(t)).unwrap();
            e.into_iter()
                .find(|e| e.kind == ww_tape::BlockKind::Events)
                .map(|c| (t, c))
        })
        .unwrap();
    let mut damaged = index_lost(&bytes[0], 1);
    let at = (good.pieces()[1].start + c.offset) as usize + c.len as usize - 20;
    damaged[at] ^= 1;
    let r = TapeReader::open(&damaged[..]).unwrap();
    let got = r.range_hash(id_of(t), OwnerId::NONE, 0..u64::MAX, TrackHash::START);
    assert!(
        matches!(
            got,
            Err(ww_tape::ReadError::Block {
                error: ww_tape::BlockError::BadCrc,
                ..
            })
        ),
        "{got:?}"
    );
    let u = (t + 1) % 3;
    let (want, _) = read_fold(&[&bytes[0]], u, &(0..u64::MAX));
    let got = r
        .range_hash(id_of(u), OwnerId::NONE, 0..u64::MAX, TrackHash::START)
        .unwrap();
    assert_eq!(got.hash, want, "track {u}");
    assert!(got.skipped > 0, "the lost index is bytes no block explains");
    let whole = good
        .range_hash(id_of(u), OwnerId::NONE, 0..u64::MAX, TrackHash::START)
        .unwrap();
    assert_eq!(whole.skipped, 0, "a piece read by its index skips nothing");
}

/// A copy of a track's last chunk in a piece walked without its index (here: in the lost index's bytes) takes the
/// chunk's place: folded once, the hash unchanged.
#[test]
fn a_range_hash_folds_a_copied_chunk_once() {
    let (_, bytes) = write(PieceTapes::One, 0x5eed);
    let good = TapeReader::open(&bytes[0][..]).unwrap();
    let p = &good.pieces()[1];
    let t = 2;
    let last = *good
        .track_entries(1, id_of(t))
        .unwrap()
        .iter()
        .rfind(|e| e.kind == ww_tape::BlockKind::Events)
        .unwrap();
    let want = good
        .range_hash(id_of(t), OwnerId::NONE, 0..u64::MAX, TrackHash::START)
        .unwrap();
    let mut copied = index_lost(&bytes[0], 1);
    let from = (p.start + last.offset) as usize;
    let to = (p.start + p.close.as_ref().unwrap().0.final_index.unwrap()) as usize;
    let room = (p.start + p.close_offset.unwrap()) as usize - to;
    assert!(
        room >= last.len as usize,
        "{room} bytes of index, a chunk of {}",
        last.len
    );
    copied.copy_within(from..from + last.len as usize, to);
    let r = TapeReader::open(&copied[..]).unwrap();
    let got = r
        .range_hash(id_of(t), OwnerId::NONE, 0..u64::MAX, TrackHash::START)
        .unwrap();
    assert_eq!(
        (got.hash, got.chunks, got.items),
        (want.hash, want.chunks, want.items)
    );
}

/// Piece `i` of the one file `bytes` with its final index zeroed, so a range hash walks it, and where the zeroed
/// stretch lies in the file.
fn without_final_index(bytes: &[u8], i: usize) -> (Vec<u8>, std::ops::Range<usize>) {
    let r = TapeReader::open(bytes).unwrap();
    let p = &r.pieces()[i];
    let (fin, _) = p.close.as_ref().unwrap();
    let at = (p.start + fin.final_index.unwrap()) as usize;
    let end = (p.start + p.close_offset.unwrap()) as usize;
    let mut out = bytes.to_vec();
    out[at..end].fill(0);
    (out, at..end)
}

/// Track `t`'s chunks in piece `i` of `bytes`: where each starts in the file and its length.
fn chunks_of(bytes: &[u8], i: usize, t: u64) -> Vec<(usize, usize)> {
    let r = TapeReader::open(bytes).unwrap();
    let start = r.pieces()[i].start as usize;
    r.track_entries(i, id_of(t))
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == ww_tape::BlockKind::Events)
        .map(|e| (start + e.offset as usize, e.len as usize))
        .collect()
}

/// A piece walked without its index: a damaged chunk of the track is an error, as by the index; another track's
/// fold goes on with the bytes no block explains counted.
#[test]
fn a_range_hash_without_an_index_reports_damage() {
    let (_, bytes) = write(PieceTapes::One, 0x5eed);
    let all = 0..u64::MAX;
    let t = (0..3)
        .find(|t| !chunks_of(&bytes[0], 1, *t).is_empty())
        .unwrap();
    let (at, len) = chunks_of(&bytes[0], 1, t)[0];
    let (mut damaged, _) = without_final_index(&bytes[0], 1);
    damaged[at + len - 20] ^= 1;
    let r = TapeReader::open(&damaged[..]).unwrap();
    let got = r.range_hash(id_of(t), OwnerId::NONE, all.clone(), TrackHash::START);
    assert!(
        matches!(got, Err(ww_tape::ReadError::Block { offset, .. }) if offset == at as u64),
        "{got:?}"
    );
    let by_index = TapeReader::open(&bytes[0][..]).unwrap();
    for u in (0..3).filter(|u| *u != t) {
        let want = by_index
            .range_hash(id_of(u), OwnerId::NONE, all.clone(), TrackHash::START)
            .unwrap();
        let got = r
            .range_hash(id_of(u), OwnerId::NONE, all.clone(), TrackHash::START)
            .unwrap();
        assert_eq!(
            (got.hash, got.chunks),
            (want.hash, want.chunks),
            "track {u}"
        );
        assert!(got.skipped > 0 && want.skipped == 0, "track {u}: {got:?}");
    }
}

/// A copy of a track's last chunk in a piece walked without its index folds once, as by the index; a copy of an
/// earlier chunk, after chunks past it, overlaps them and is an error.
#[test]
fn a_range_hash_folds_a_chunk_written_again_once() {
    let (_, bytes) = write(PieceTapes::One, 0x5eed);
    let all = 0..u64::MAX;
    let t = (0..3)
        .find(|t| chunks_of(&bytes[0], 1, *t).len() >= 2)
        .unwrap();
    let chunks = chunks_of(&bytes[0], 1, t);
    let want = TapeReader::open(&bytes[0][..])
        .unwrap()
        .range_hash(id_of(t), OwnerId::NONE, all.clone(), TrackHash::START)
        .unwrap();
    let (stripped, zeroed) = without_final_index(&bytes[0], 1);
    for (k, expect_whole) in [(chunks.len() - 1, true), (0, false)] {
        let (at, len) = chunks[k];
        assert!(zeroed.len() > len + 8, "room for a copy");
        let mut copied = stripped.clone();
        let to = zeroed.start + 8;
        copied.copy_within(at..at + len, to);
        let r = TapeReader::open(&copied[..]).unwrap();
        let got = r.range_hash(id_of(t), OwnerId::NONE, all.clone(), TrackHash::START);
        match expect_whole {
            true => {
                let got = got.unwrap();
                assert_eq!((got.hash, got.chunks), (want.hash, want.chunks));
            }
            false => assert!(
                matches!(got, Err(ww_tape::ReadError::Overlap { offset, .. }) if offset == to as u64),
                "{got:?}"
            ),
        }
    }
}

/// An unclosed piece whose checkpoint names one chunk of a track many times, its CRCs good, is refused as a final
/// index naming it so is: the entries don't read, and the range hash reads the chunks instead, each once.
#[test]
fn a_checkpoint_naming_a_chunk_many_times_is_refused() {
    use ww_tape::index::{ENTRY_LEN, IndexBlock, IndexKind};
    use ww_tape::{BlockKind, MemSink, TapeWriter};
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    let tape = w.sink().tape();
    w.set_index_policy(IndexPolicy {
        bytes: 2048,
        interval: None,
        ..IndexPolicy::default()
    })
    .unwrap();
    let defs = base_defs();
    for k in 0..24 {
        let (h, b) = chunk(1, k);
        w.write_chunk(defs, &h, &column(&b)).unwrap();
    }
    let bytes = tape.to_vec().unwrap();
    drop(w);
    let good = TapeReader::open(&bytes[..]).unwrap();
    let want = good
        .range_hash(id_of(1), OwnerId::NONE, 0..u64::MAX, TrackHash::START)
        .unwrap();
    // the newest checkpoint, its chunk entries all made the first one's
    let at = (0..bytes.len() - 4)
        .rev()
        .find(|&i| {
            bytes[i..i + 4] == *b"WWTP"
                && ww_tape::Block::read(&bytes[i..])
                    .ok()
                    .and_then(|b| IndexBlock::read(&b).ok())
                    .is_some_and(|ix| ix.kind() == IndexKind::Checkpoint)
        })
        .expect("a checkpoint");
    let mut crafted = bytes.clone();
    let len = ww_tape::block_len(&crafted[at..]).unwrap();
    let block = &mut crafted[at..at + len];
    let entries_at = IndexBlock::head_len(block).unwrap();
    let mut first: Option<[u8; ENTRY_LEN]> = None;
    let mut changed = 0;
    for e in block[entries_at..len - 4].as_chunks_mut::<ENTRY_LEN>().0 {
        // an entry states its block's kind at byte 12; only events chunks are changed
        if e[12] != BlockKind::Events.to_u8() {
            continue;
        }
        match first {
            None => first = Some(*e),
            Some(f) => {
                e.copy_from_slice(&f);
                changed += 1;
            }
        }
    }
    assert!(changed > 1, "{changed} entries changed");
    let head = crc32c(&[&block[..28], &block[32..entries_at]]);
    block[28..32].copy_from_slice(&head.to_le_bytes());
    let crc = crc32c(&[&block[..len - 4]]);
    block[len - 4..].copy_from_slice(&crc.to_le_bytes());
    let r = TapeReader::open(&crafted[..]).unwrap();
    assert!(matches!(
        r.entries(0),
        Err(ww_tape::ReadError::Index(
            ww_tape::index::IndexError::BadEntries
        ))
    ));
    assert!(r.track_entries(0, id_of(1)).is_err());
    let got = r
        .range_hash(id_of(1), OwnerId::NONE, 0..u64::MAX, TrackHash::START)
        .unwrap();
    assert_eq!((got.hash, got.chunks), (want.hash, want.chunks));
}

/// The index path folds each chunk by the unit hash its entry states, read from an index whose CRCs hold: a chunk
/// marked `FLAG_PAYLOAD_UNCHECKED` behind such an entry (a piece of level `Full` never writes one, and a piece of a
/// lower level marks every entry `ENTRY_UNHASHED`, which no reader takes) folds as the writer stated it. Every read
/// of the chunk refuses the flag, and so does the content hash check: it is caught where the payload is read.
#[test]
fn an_unchecked_chunk_folds_by_its_entry_and_fails_verify() {
    use ww_tape::{BlockKind, ChunkHead};
    let (_, bytes) = write(PieceTapes::One, 0x9a7);
    let good = TapeReader::open(&bytes[0][..]).unwrap();
    let want = good
        .range_hash(id_of(1), OwnerId::NONE, 0..u64::MAX, TrackHash::START)
        .unwrap();
    let p = &good.pieces()[1];
    let e = good
        .track_entries(1, id_of(1))
        .unwrap()
        .into_iter()
        .find(|e| e.kind == BlockKind::Events)
        .unwrap();
    let at = (p.start + e.offset) as usize;
    let len = e.len as usize;
    let mut crafted = bytes[0].clone();
    let b = &mut crafted[at..at + len];
    let head_end = ChunkHead::head_len(b).unwrap();
    b[6] |= ww_tape::FLAG_PAYLOAD_UNCHECKED as u8;
    let head = crc32c(&[&b[..28], &b[32..head_end]]);
    b[28..32].copy_from_slice(&head.to_le_bytes());
    let crc = crc32c(&[&b[..len - 4]]);
    b[len - 4..].copy_from_slice(&crc.to_le_bytes());
    let r = TapeReader::open(&crafted[..]).unwrap();
    let got = r
        .range_hash(id_of(1), OwnerId::NONE, 0..u64::MAX, TrackHash::START)
        .unwrap();
    assert_eq!((got.hash, got.chunks), (want.hash, want.chunks));
    let mut buf = vec![];
    let refused = format!("{:?}", r.read_chunk_head(1, &e, &mut buf).err());
    assert!(refused.contains("PayloadUnchecked"), "{refused}");
    assert!(good.verify(1).map(|v| v.holds).unwrap());
    assert!(
        !r.verify(1).map(|v| v.holds).unwrap(),
        "the content hash takes it as bytes"
    );
}

/// An entry without a unit hash pointed at another track's chunk, its CRCs good, is refused when the chunk is read:
/// the chunk read is compared with the entry, never folded as the track's.
#[test]
fn an_unhashed_entry_naming_another_tracks_chunk_is_refused() {
    use ww_tape::BlockKind;
    use ww_tape::index::IndexEntry;
    let (_, files) = write(PieceTapes::One, 0x5eed);
    let mut bytes = files[0].clone();
    let good = TapeReader::open(&bytes[..]).unwrap();
    let i = good.pieces().len() - 1;
    let chunks = |t: u64| -> Vec<IndexEntry> {
        good.track_entries(i, id_of(t))
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == BlockKind::Events)
            .collect()
    };
    // track t's last chunk entry re-pointed at a chunk of track u that keeps t's list in file order
    let fits = |t: u64, last: &IndexEntry, other: &IndexEntry| {
        good.track_entries(i, id_of(t)).unwrap().iter().all(|x| {
            x.offset == last.offset
                || (x.offset < last.offset && x.offset + u64::from(x.len) <= other.offset)
                || (x.offset > last.offset && other.offset + u64::from(other.len) <= x.offset)
        })
    };
    let (t, last, other) = (0..3u64)
        .flat_map(|t| (0..3u64).filter(move |u| *u != t).map(move |u| (t, u)))
        .find_map(|(t, u)| {
            let last = *chunks(t).last()?;
            let other = chunks(u).into_iter().find(|o| fits(t, &last, o))?;
            Some((t, last, other))
        })
        .expect("a chunk to point at");
    drop(good);
    rewrite_final_entries(&mut bytes, i, t, |_, e| {
        if u64::from_le_bytes(e[0..8].try_into().unwrap()) == last.offset {
            e[0..8].copy_from_slice(&other.offset.to_le_bytes());
            e[8..12].copy_from_slice(&other.len.to_le_bytes());
            e[88..120].fill(0);
        }
    });
    let r = TapeReader::open(&bytes[..]).unwrap();
    let got = r.range_hash(id_of(t), OwnerId::NONE, 0..u64::MAX, TrackHash::START);
    assert!(
        matches!(
            got,
            Err(ww_tape::ReadError::Index(
                ww_tape::index::IndexError::BadEntries
            ))
        ),
        "{got:?}"
    );
    let e = r
        .track_entries(i, id_of(t))
        .unwrap()
        .into_iter()
        .find(|e| e.offset == other.offset)
        .unwrap();
    let mut buf = vec![];
    assert!(matches!(
        r.read_chunk_head(i, &e, &mut buf),
        Err(ww_tape::ReadError::Index(
            ww_tape::index::IndexError::BadEntries
        ))
    ));
}

/// Two entries of other chunks stating the same items, their CRCs and the content hash good, overlap: the second is
/// never taken for a copy of the first.
#[test]
fn two_chunks_stating_the_same_items_overlap() {
    use ww_tape::BlockKind;
    let (_, files) = write(PieceTapes::One, 0x5eed);
    let mut bytes = files[0].clone();
    let r = TapeReader::open(&bytes[..]).unwrap();
    let i = r.pieces().len() - 1;
    let t = (0..3)
        .find(|t| {
            let e = r.track_entries(i, id_of(*t)).unwrap();
            e.iter().filter(|e| e.kind == BlockKind::Events).count() >= 2
        })
        .unwrap();
    drop(r);
    let mut first = [0u8; 8];
    let mut seen = 0;
    rewrite_final_entries(&mut bytes, i, t, |_, e| {
        if e[12] != BlockKind::Events.to_u8() {
            return;
        }
        match seen {
            0 => first.copy_from_slice(&e[32..40]),
            1 => e[32..40].copy_from_slice(&first),
            _ => {}
        }
        seen += 1;
    });
    let r = TapeReader::open(&bytes[..]).unwrap();
    let got = r.range_hash(id_of(t), OwnerId::NONE, 0..u64::MAX, TrackHash::START);
    assert!(
        matches!(got, Err(ww_tape::ReadError::Overlap { .. })),
        "{got:?}"
    );
}

/// Recompute the content hash of piece `i` of `bytes` (its blocks back to back from its start to its close) and write
/// it into its close and tail, their CRCs made good: what a hostile writer does after changing the piece.
pub(crate) fn reseal(bytes: &mut [u8], i: usize) {
    use ww_tape::Block;
    let r = TapeReader::open(&bytes[..]).unwrap();
    let p = r.pieces()[i].clone();
    let fin = (p.start + p.close_offset.unwrap()) as usize;
    let mut fed = Vec::new();
    let mut at = p.start as usize;
    while at < fin {
        let b = Block::read(&bytes[at..]).unwrap();
        let n = b.len();
        match b.chunk() {
            Some(c) => {
                let (po, pe) = (c.payload_offset(), c.payload_offset() + c.payload().len());
                fed.extend_from_slice(&bytes[at..at + po]);
                fed.extend_from_slice(&payload_hash(c.payload()));
                fed.extend_from_slice(&bytes[at + pe..at + n]);
            }
            None => fed.extend_from_slice(&bytes[at..at + n]),
        }
        at += n;
    }
    let h = payload_hash(&fed);
    let flen = ww_tape::block_len(&bytes[fin..]).unwrap();
    bytes[fin + 40..fin + 72].copy_from_slice(&h);
    let crc = crc32c(&[&bytes[fin..fin + flen - 4]]);
    bytes[fin + flen - 4..fin + flen].copy_from_slice(&crc.to_le_bytes());
    let tail = p.end as usize - ww_tape::TAIL_LEN;
    bytes[tail + 56..tail + 88].copy_from_slice(&h);
    let crc = crc32c(&[&bytes[tail..tail + 160]]);
    bytes[tail + 160..tail + 164].copy_from_slice(&crc.to_le_bytes());
}

/// The last piece of a written file, and a track with a chunk in it.
fn last_and_track(bytes: &[u8]) -> (usize, u64) {
    let r = TapeReader::open(bytes).unwrap();
    let i = r.pieces().len() - 1;
    let t = (0..3)
        .find(|t| {
            r.track_entries(i, id_of(*t))
                .unwrap()
                .iter()
                .any(|e| e.kind == ww_tape::BlockKind::Events)
        })
        .unwrap();
    (i, t)
}

/// An index entry that doesn't state its block, every CRC and the content hash made good again: `verify` replays
/// the piece and compares each entry with the block at its offset, so it fails, where the content hash alone holds.
/// A unit hash that isn't the chunk's, a kind the reader doesn't fold, a range no column has.
#[test]
fn verify_compares_each_entry_with_its_block() {
    use ww_tape::BlockKind;
    let (_, files) = write(PieceTapes::One, 0x5eed);
    let (i, t) = last_and_track(&files[0]);
    // resealed unchanged, the piece verifies: what fails below is the change
    let mut same = files[0].clone();
    reseal(&mut same, i);
    assert_eq!(same, files[0]);
    assert!(
        TapeReader::open(&same[..])
            .unwrap()
            .verify(i)
            .map(|v| v.holds)
            .unwrap()
    );
    let first_chunk = |j: &mut Option<usize>, e: &[u8]| {
        let first = j.is_none() && e[12] == BlockKind::Events.to_u8();
        if first {
            *j = Some(0);
        }
        first
    };
    type Change = fn(&mut [u8]);
    let cases: [(&str, Change); 4] = [
        ("unit hash", |e| e[88] ^= 1),
        ("unit hash at 100", |e| e[100] ^= 1),
        ("kind 11", |e| e[12] = 11),
        ("range", |e| {
            e[15] |= 2;
            e[72..80].copy_from_slice(&5u64.to_le_bytes());
            e[80..88].copy_from_slice(&u64::MAX.to_le_bytes());
        }),
    ];
    for (name, change) in cases {
        let mut bytes = files[0].clone();
        let mut seen = None;
        rewrite_final_entries(&mut bytes, i, t, |_, e| {
            if first_chunk(&mut seen, e) {
                change(e);
            }
        });
        reseal(&mut bytes, i);
        let r = TapeReader::open(&bytes[..]).unwrap();
        assert!(!r.verify(i).map(|v| v.holds).unwrap(), "{name}");
    }
    // the chunk of an entry of kind 11 is counted as skipped by the range hash, never dropped unsaid
    let mut bytes = files[0].clone();
    let mut seen = None;
    let mut len = 0;
    rewrite_final_entries(&mut bytes, i, t, |_, e| {
        if first_chunk(&mut seen, e) {
            e[12] = 11;
            len = u32::from_le_bytes(e[8..12].try_into().unwrap());
        }
    });
    let good = TapeReader::open(&files[0][..]).unwrap();
    let want = good
        .range_hash(id_of(t), OwnerId::NONE, 0..u64::MAX, TrackHash::START)
        .unwrap();
    let got = TapeReader::open(&bytes[..])
        .unwrap()
        .range_hash(id_of(t), OwnerId::NONE, 0..u64::MAX, TrackHash::START)
        .unwrap();
    assert_eq!(got.chunks + 1, want.chunks);
    assert_eq!(got.skipped, want.skipped + u64::from(len));
}

/// An entry without a unit hash stating a range its chunk's columns don't have is refused when the range hash reads
/// the chunk.
#[test]
fn a_range_its_chunk_does_not_have_is_refused_on_read() {
    use ww_tape::BlockKind;
    let (_, files) = write(PieceTapes::One, 0x5eed);
    let (i, t) = last_and_track(&files[0]);
    let mut bytes = files[0].clone();
    let mut done = false;
    rewrite_final_entries(&mut bytes, i, t, |_, e| {
        if !done && e[12] == BlockKind::Events.to_u8() {
            done = true;
            e[15] |= 2;
            e[72..80].copy_from_slice(&5u64.to_le_bytes());
            e[80..88].copy_from_slice(&u64::MAX.to_le_bytes());
            e[88..120].fill(0);
        }
    });
    let got = TapeReader::open(&bytes[..]).unwrap().range_hash(
        id_of(t),
        OwnerId::NONE,
        0..u64::MAX,
        TrackHash::START,
    );
    assert!(
        matches!(
            got,
            Err(ww_tape::ReadError::Index(
                ww_tape::index::IndexError::BadEntries
            ))
        ),
        "{got:?}"
    );
}

/// A close row whose track hash isn't its track's fold, its CRC made good: `verify` fails.
#[test]
fn verify_compares_each_close_row_with_its_fold() {
    let (_, files) = write(PieceTapes::One, 0x5eed);
    let (i, t) = last_and_track(&files[0]);
    let mut bytes = files[0].clone();
    let r = TapeReader::open(&bytes[..]).unwrap();
    let p = r.pieces()[i].clone();
    drop(r);
    let row = p
        .close
        .as_ref()
        .unwrap()
        .1
        .iter()
        .find(|r| r.track_id == id_of(t))
        .unwrap();
    let fin = (p.start + p.close_offset.unwrap()) as usize;
    let flen = ww_tape::block_len(&bytes[fin..]).unwrap();
    let at = bytes[fin..fin + flen]
        .windows(32)
        .position(|w| w == row.hash.0)
        .unwrap();
    bytes[fin + at] ^= 1;
    // the close isn't in the content hash: its CRC is all a writer makes good
    let crc = crc32c(&[&bytes[fin..fin + flen - 4]]);
    bytes[fin + flen - 4..fin + flen].copy_from_slice(&crc.to_le_bytes());
    let r = TapeReader::open(&bytes[..]).unwrap();
    assert_ne!(
        r.pieces()[i].close.as_ref().unwrap().1,
        p.close.as_ref().unwrap().1
    );
    assert!(!r.verify(i).map(|v| v.holds).unwrap());
}

/// A track's last chunk taken out of the fold: its entry moved past the blocks walked (onto the close) with another
/// unit hash, the close row stating the fold without it, every CRC and the content hash made good. `verify` fails:
/// the chunk walked has no entry, and the entry names no block walked before the close.
#[test]
fn verify_refuses_a_chunk_its_index_lists_past_the_blocks() {
    use ww_tape::BlockKind;
    let (_, files) = write(PieceTapes::One, 0x5eed);
    let mut bytes = files[0].clone();
    let good = TapeReader::open(&bytes[..]).unwrap();
    let i = good.pieces().len() - 1;
    let p = good.pieces()[i].clone();
    let fin = p.close_offset.unwrap();
    let t = (0..3u64)
        .find(|t| {
            let e = good.track_entries(i, id_of(*t)).unwrap();
            e.len() >= 2
                && e.last().unwrap().kind == BlockKind::Events
                && fin + u64::from(e.last().unwrap().len) <= p.end - p.start
        })
        .unwrap();
    let entries = good.track_entries(i, id_of(t)).unwrap();
    let last = *entries.last().unwrap();
    let mut fold = p
        .head
        .as_ref()
        .unwrap()
        .1
        .iter()
        .find(|s| s.track_id == id_of(t))
        .map_or(TrackHash::START, |s| s.hash);
    let mut buf = vec![];
    for e in entries
        .iter()
        .filter(|e| e.kind == BlockKind::Events && e.offset != last.offset)
    {
        let c = good.read_block(i, e, &mut buf).unwrap().chunk().unwrap();
        fold = fold.chunk(&c, &payload_hash(c.payload()));
    }
    let row = p
        .close
        .as_ref()
        .unwrap()
        .1
        .iter()
        .find(|r| r.track_id == id_of(t))
        .unwrap()
        .hash;
    drop(good);
    rewrite_final_entries(&mut bytes, i, t, |_, e| {
        if u64::from_le_bytes(e[0..8].try_into().unwrap()) == last.offset {
            e[0..8].copy_from_slice(&fin.to_le_bytes());
            e[88] ^= 1;
        }
    });
    let at = (p.start + fin) as usize;
    let flen = ww_tape::block_len(&bytes[at..]).unwrap();
    let k = bytes[at..at + flen]
        .windows(32)
        .position(|w| w == row.0)
        .unwrap();
    bytes[at + k..at + k + 32].copy_from_slice(&fold.0);
    let crc = crc32c(&[&bytes[at..at + flen - 4]]);
    bytes[at + flen - 4..at + flen].copy_from_slice(&crc.to_le_bytes());
    reseal(&mut bytes, i);
    let r = TapeReader::open(&bytes[..]).unwrap();
    assert_eq!(
        r.pieces()[i]
            .close
            .as_ref()
            .unwrap()
            .1
            .iter()
            .find(|r| r.track_id == id_of(t))
            .unwrap()
            .hash,
        fold
    );
    assert!(r.chain().is_whole());
    // the index path folds what the writer stated: every chunk unread, as the writer says
    let g = r
        .range_hash(id_of(t), OwnerId::NONE, 0..u64::MAX, TrackHash::START)
        .unwrap();
    assert_eq!((g.read, g.stated()), (0, g.chunks));
    assert!(!r.verify(i).map(|v| v.holds).unwrap());
}

/// A range hash over pieces whose head doesn't read says so (`RangeHash::headless`): the older layout's file, both
/// heads lost to it, and a file of this layout with one head damaged; 0 over whole heads.
#[test]
fn a_range_hash_over_pieces_without_a_head_says_so() {
    let (_, files) = write(PieceTapes::One, 0x5eed);
    let (i, t) = last_and_track(&files[0]);
    let whole = TapeReader::open(&files[0][..])
        .unwrap()
        .range_hash(id_of(t), OwnerId::NONE, 0..u64::MAX, TrackHash::START)
        .unwrap();
    assert_eq!(whole.headless, 0);
    let mut bytes = files[0].clone();
    let start = TapeReader::open(&bytes[..]).unwrap().pieces()[i].start as usize;
    // a byte of the piece head's rows: its CRC fails
    bytes[start + 100] ^= 1;
    let r = TapeReader::open(&bytes[..]).unwrap();
    assert!(r.pieces()[i].head.is_none());
    let got = r.range_hash(id_of(t), OwnerId::NONE, 0..u64::MAX, TrackHash::START);
    assert_eq!(got.map(|g| g.headless).ok(), Some(1));
    let other =
        std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/it/data/other-layout.wwt"))
            .unwrap();
    let r = TapeReader::open(&other[..]).unwrap();
    let id = (0..r.pieces().len())
        .flat_map(|i| r.entries(i).unwrap_or_default())
        .map(|e| e.track_id)
        .find(|t| *t != RECORDS_TRACK_ID && *t != STATE_TRACK_ID)
        .unwrap();
    let headless = r.pieces().iter().filter(|p| p.head.is_none()).count() as u64;
    assert!(headless > 0);
    let g = r
        .range_hash(id, OwnerId::NONE, 0..u64::MAX, TrackHash::START)
        .unwrap();
    assert!(g.chunks > 0);
    assert_eq!(g.headless, headless);
}
