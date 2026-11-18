//! The hash of a range of a track's chunks from the index ([TapeReader::range_hash]) against a fold that reads them.

use std::path::{Path, PathBuf};

use crate::definitions::base_defs;
use crate::pieces::{chunk, column, records};
use ww_tape::index::IndexPolicy;
use ww_tape::piece::payload_hash;
use ww_tape::{
    ChainOptions, CutPolicy, FileBlocks, Finished, MemTapes, OwnerId, PieceChain, PieceFiles,
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
fn id_of(t: u64) -> u64 {
    0x1000 + t
}

/// A chain of 60 chunks on tracks 0 to 2 in a random order, with record blocks between, cut every 7 chunks: the
/// finished pieces and the bytes of each file, in order.
fn write(files: PieceFiles, seed: u64) -> (Vec<Finished>, Vec<Vec<u8>>) {
    let mut rng = Rng(seed);
    let options = ChainOptions {
        files,
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
    let mut c = PieceChain::create_on(
        MemTapes::new(),
        Path::new("range"),
        OwnerId([4; 32]),
        0,
        &[],
        options,
    )
    .unwrap();
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
    let mem = c.factory().clone();
    let done = c.finish().unwrap();
    let mut paths: Vec<&PathBuf> = vec![];
    for f in &done {
        if !paths.contains(&&f.path) {
            paths.push(&f.path);
        }
    }
    let bytes = paths
        .iter()
        .map(|p| mem.get(p).unwrap().unwrap().to_vec().unwrap())
        .collect();
    (done, bytes)
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
    for files in [PieceFiles::One, PieceFiles::Each] {
        let (done, bytes) = write(files, 0x5eed + files as u64);
        assert!(done.len() >= 5, "{files:?}: {} pieces", done.len());
        let all: Vec<&[u8]> = bytes.iter().map(Vec::as_slice).collect();
        // the first pieces gone: from piece 2 of the one file, or from the third file
        let tail: Vec<&[u8]> = match files {
            PieceFiles::One => {
                let r = TapeReader::open(&bytes[0][..]).unwrap();
                vec![&bytes[0][r.pieces()[2].start as usize..]]
            }
            PieceFiles::Each => all[2..].to_vec(),
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
    let (_, bytes) = write(PieceFiles::One, 0xdead);
    let mut damaged = bytes[0].clone();
    let r = TapeReader::open(&damaged[..]).unwrap();
    let p = &r.pieces()[1];
    let (fin, _) = p.finish.as_ref().unwrap();
    let at = (p.start + fin.final_index.unwrap()) as usize;
    let end = (p.start + p.finish_offset.unwrap()) as usize;
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
fn crc32c(parts: &[&[u8]]) -> u32 {
    let crc = crc::Crc::<u32>::new(&crc::CRC_32_ISCSI);
    let mut d = crc.digest();
    for p in parts {
        d.update(p);
    }
    d.finalize()
}

/// The final index of piece `i` of `bytes` with every entry of track `t` replaced by `change` of it, the list's CRC,
/// the head CRC and the block CRC made good again.
fn rewrite_final_entries(
    bytes: &mut [u8],
    i: usize,
    t: u64,
    mut change: impl FnMut(usize, &mut [u8]),
) {
    use ww_tape::index::{ENTRY_LEN, INDEX_FIXED_LEN, IndexBlock, TRACK_LEN};
    let r = TapeReader::open(&bytes[..]).unwrap();
    let p = &r.pieces()[i];
    let at = (p.start + p.finish.as_ref().unwrap().0.final_index.unwrap()) as usize;
    let len = ww_tape::block_len(&bytes[at..]).unwrap();
    let block = &mut bytes[at..at + len];
    let ix = IndexBlock::read_head(block).unwrap();
    let k = ix.tracks().position(|l| l.track_id == id_of(t)).unwrap();
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
    let (_, bytes) = write(PieceFiles::One, 0xd0b1e);
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
    let (fin, _) = p.finish.as_ref().unwrap();
    let at = (p.start + fin.final_index.unwrap()) as usize;
    let end = (p.start + p.finish_offset.unwrap()) as usize;
    let mut out = bytes.to_vec();
    out[at..end].fill(0);
    out
}

/// Without its index a piece is walked: a damaged chunk of the track whose head reads is an error, as by the index,
/// never a hash of the chunks around it; another track's hash is whole, the lost index counted as skipped bytes.
#[test]
fn a_range_hash_without_an_index_reports_a_damaged_chunk() {
    let (_, bytes) = write(PieceFiles::One, 0x5eed);
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
    let (_, bytes) = write(PieceFiles::One, 0x5eed);
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
    let to = (p.start + p.finish.as_ref().unwrap().0.final_index.unwrap()) as usize;
    let room = (p.start + p.finish_offset.unwrap()) as usize - to;
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
    let (fin, _) = p.finish.as_ref().unwrap();
    let at = (p.start + fin.final_index.unwrap()) as usize;
    let end = (p.start + p.finish_offset.unwrap()) as usize;
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
    let (_, bytes) = write(PieceFiles::One, 0x5eed);
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
    let (_, bytes) = write(PieceFiles::One, 0x5eed);
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
