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
