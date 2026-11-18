//! The running hash of each track ([TrackHash]) and the owner of a track ([TrackDef::owner]), through the public API.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::definitions::{CLOCK, base_defs, state, track};
use crate::pieces::{chunk, column, records};
use shrink_wrap::prelude::*;
use ww_tape::index::IndexPolicy;
use ww_tape::piece::payload_hash;
use ww_tape::writer::NearPolicy;
use ww_tape::{
    BlockKind, ChainOptions, CutPolicy, Definitions, Describe, FileBlocks, Finished, Layout,
    MemTapes, OwnerId, PieceChain, PieceFiles, SyncPolicy, TapeReader, TrackDef, TrackHash,
    WriterOptions,
};

/// How a recording of the same tracks is written.
#[derive(Copy, Clone, Debug)]
struct Shape {
    /// With tracks 2 and 3 and puts on a state track between the calls on tracks 1 and 9.
    interleaved: bool,
    /// A cut once a piece holds this many chunks; 0 never.
    cut: u64,
    files: PieceFiles,
    layout: Layout,
}

fn options(shape: Shape) -> ChainOptions {
    ChainOptions {
        files: shape.files,
        writer: WriterOptions {
            sync: SyncPolicy::None,
            index: IndexPolicy {
                bytes: 1024,
                interval: None,
                ..IndexPolicy::default()
            },
            layout: shape.layout,
            producer: "track_hash".to_owned(),
            near: NearPolicy::default(),
        },
        cut: CutPolicy {
            chunks: if shape.cut == 0 { u64::MAX } else { shape.cut },
            ..CutPolicy::default()
        },
    }
}

/// The bytes of the files the pieces `done` were written to, in the order written.
fn files_of(mem: &MemTapes, done: &[Finished]) -> Vec<u8> {
    let mut paths: Vec<&PathBuf> = vec![];
    for f in done {
        if !paths.contains(&&f.path) {
            paths.push(&f.path);
        }
    }
    paths
        .iter()
        .flat_map(|p| mem.get(p).unwrap().unwrap().to_vec().unwrap())
        .collect()
}

/// Each track's hash after the chain `done`, by id and owner.
fn hashes(done: &[Finished]) -> BTreeMap<(u64, OwnerId), TrackHash> {
    done.last()
        .unwrap()
        .tracks
        .iter()
        .map(|t| ((t.track_id, t.owner), t.hash))
        .collect()
}

/// 12 rounds of a chunk of track 1 and a records block on track 9, written as `shape` says; the finished pieces and
/// the bytes of their files.
fn write(shape: Shape) -> (Vec<Finished>, Vec<u8>) {
    let mut defs = base_defs().clone();
    defs.define(&state(11)).unwrap();
    let mut c = PieceChain::create_on(
        MemTapes::new(),
        Path::new("hash"),
        OwnerId([5; 32]),
        0,
        &[],
        options(shape),
    )
    .unwrap();
    for k in 0..12u64 {
        if shape.interleaved {
            let (h, b) = chunk(1, k);
            c.write_chunk(&defs, &h, &column(&b)).unwrap();
            c.writer()
                .unwrap()
                .put(&defs, 11, k as i64, &[k as u8; 20])
                .unwrap();
        }
        let (h, b) = chunk(0, k);
        c.write_chunk(&defs, &h, &column(&b)).unwrap();
        if shape.interleaved {
            let (h, b) = chunk(2, k);
            c.write_chunk(&defs, &h, &column(&b)).unwrap();
        }
        let w = c.writer().unwrap();
        let id = w.next_id();
        w.write_block(&defs, &records(id, k as i64)).unwrap();
    }
    let mem = c.factory().clone();
    let done = c.finish().unwrap();
    let bytes = files_of(&mem, &done);
    (done, bytes)
}

/// Track 9's id: [crate::pieces::records] writes on it.
fn track_9_id() -> u64 {
    let Describe::Track(t) = track(9, CLOCK) else {
        unreachable!("track() gives a track")
    };
    t.id()
}

/// The hash of track `number` folded by a reader from the good blocks of `bytes`: each chunk of the track by its
/// payload's hash, each record not marked as a repeat.
fn fold(bytes: &[u8], number: u32) -> TrackHash {
    let mut h = TrackHash::START;
    let mut rd = FileBlocks::new(bytes);
    while let Some(b) = rd.next_block() {
        let b = b.unwrap();
        if let Some(c) = b.chunk() {
            if c.head().track_number == number {
                h = h.chunk(&c, &payload_hash(c.payload()));
            }
            continue;
        }
        if b.kind != BlockKind::Records || b.is_repeat() {
            continue;
        }
        let mut buf = vec![];
        let b = b.decode(&mut buf).unwrap();
        for r in b.records() {
            let r = r.unwrap();
            if r.track == number {
                h = h.record(r.at.map_or(0, |a| a.ticks), r.item);
            }
        }
    }
    h
}

/// The hash of tracks 1 and 9 is the same written alone or among other tracks and puts, in one piece or cut every
/// 1 to 5 chunks, into a file per piece or one file, unaligned or aligned; a reader's fold of the blocks gives it.
#[test]
fn a_track_hash_is_the_same_however_the_track_is_written() {
    let alone = Shape {
        interleaved: false,
        cut: 0,
        files: PieceFiles::Each,
        layout: Layout::UNALIGNED,
    };
    let (done, bytes) = write(alone);
    let want = hashes(&done);
    let keys = [(0x1000, OwnerId::NONE), (track_9_id(), OwnerId::NONE)];
    assert_eq!(want[&keys[0]], fold(&bytes, 1), "track 1 read back");
    assert_eq!(want[&keys[1]], fold(&bytes, 9), "track 9 read back");
    assert_ne!(want[&keys[0]], TrackHash::START);
    assert_ne!(want[&keys[1]], TrackHash::START);

    let mut shapes = vec![Shape {
        interleaved: true,
        ..alone
    }];
    for cut in 1..=5 {
        for files in [PieceFiles::Each, PieceFiles::One] {
            for layout in [Layout::UNALIGNED, Layout::ALIGNED] {
                shapes.push(Shape {
                    interleaved: true,
                    cut,
                    files,
                    layout,
                });
            }
        }
    }
    for shape in shapes {
        let (done, bytes) = write(shape);
        if shape.cut != 0 {
            assert!(done.len() > 1, "{shape:?}: {} pieces", done.len());
        }
        let got = hashes(&done);
        for k in &keys {
            assert_eq!(got[k], want[k], "{shape:?}: track {k:x?}");
        }
        assert_eq!(
            got[&keys[0]],
            fold(&bytes, 1),
            "{shape:?}: track 1 read back"
        );
        assert_eq!(
            got[&keys[1]],
            fold(&bytes, 9),
            "{shape:?}: track 9 read back"
        );
        // puts are repeats: the state track holding only puts has the hash of no data
        let Describe::Track(t11) = state(11) else {
            unreachable!("state() gives a track")
        };
        let puts = got.get(&(t11.id(), OwnerId::NONE));
        assert!(
            puts.is_none_or(|h| *h == TrackHash::START),
            "{shape:?}: {puts:?}"
        );
        // every piece's finish rows say the same as the writer
        let r = TapeReader::open(&bytes[..]).unwrap();
        assert_eq!(r.pieces().len(), done.len(), "{shape:?}");
        let mut rows_seen = 0;
        for (p, f) in r.pieces().iter().zip(&done) {
            let (_, rows) = p.finish.as_ref().unwrap();
            for t in &f.tracks {
                if let Some(row) = rows
                    .iter()
                    .find(|row| (row.track_id, row.owner) == (t.track_id, t.owner))
                {
                    assert_eq!(row.hash, t.hash, "{shape:?}: {t:?}");
                    rows_seen += 1;
                }
            }
        }
        assert!(rows_seen >= done.len(), "{shape:?}: {rows_seen} rows");
    }
}

/// Track `number` with the key of track 20, owned by `owner`.
fn owned(number: u32, owner: [u8; 32]) -> TrackDef<'static> {
    let Describe::Track(t) = track(20, CLOCK) else {
        unreachable!("track() gives a track")
    };
    TrackDef {
        number: UNib32(number),
        owner,
        ..t
    }
}

/// 8 chunks on each track of `tracks`, alternating; the finished chain.
fn write_owned(tracks: &[TrackDef<'static>]) -> Vec<Finished> {
    let mut defs: Definitions = base_defs().clone();
    for t in tracks {
        defs.define(&Describe::Track(t.clone())).unwrap();
    }
    let shape = Shape {
        interleaved: false,
        cut: 5,
        files: PieceFiles::One,
        layout: Layout::UNALIGNED,
    };
    let mut c = PieceChain::create_on(
        MemTapes::new(),
        Path::new("owned"),
        OwnerId([5; 32]),
        0,
        &[],
        options(shape),
    )
    .unwrap();
    for k in 0..8 {
        for (i, t) in tracks.iter().enumerate() {
            let (mut h, b) = chunk(i as u64, k);
            h.track_id = t.id();
            h.track_number = t.number.0;
            c.write_chunk(&defs, &h, &column(&b)).unwrap();
        }
    }
    c.finish().unwrap()
}

/// Two tracks of one identity with other owners are two tracks: each has its own items and hash, the same as
/// written alone; the owner is not part of the id.
#[test]
fn tracks_of_one_key_with_two_owners_are_two_tracks() {
    let (a, b) = ([0xaa; 32], [0xbb; 32]);
    let (ta, tb) = (owned(20, a), owned(21, b));
    assert_eq!(ta.id(), tb.id(), "the owner is not part of the id");
    assert_eq!(ta.id(), owned(20, [0; 32]).id());
    let id = ta.id();

    let both = write_owned(&[ta.clone(), tb.clone()]);
    let rows: Vec<_> = both
        .last()
        .unwrap()
        .tracks
        .iter()
        .filter(|t| t.track_id == id)
        .collect();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0].owner, OwnerId(a));
    assert_eq!(rows[1].owner, OwnerId(b));
    assert_eq!(rows[0].track_number, 20);
    assert_eq!(rows[1].track_number, 21);
    assert_eq!(rows[0].next_item, 8 * 50);
    assert_eq!(rows[1].next_item, 8 * 50);
    assert_ne!(rows[0].hash, rows[1].hash);

    // alone, as its chunks in both: the first track's chunks are chunk(0, k), the second's chunk(1, k)
    let alone_a = hashes(&write_owned(&[ta]));
    assert_eq!(alone_a[&(id, OwnerId(a))], rows[0].hash);
    let mut defs: Definitions = base_defs().clone();
    defs.define(&Describe::Track(tb.clone())).unwrap();
    let mut c = PieceChain::create_on(
        MemTapes::new(),
        Path::new("alone_b"),
        OwnerId([5; 32]),
        0,
        &[],
        options(Shape {
            interleaved: false,
            cut: 0,
            files: PieceFiles::Each,
            layout: Layout::UNALIGNED,
        }),
    )
    .unwrap();
    for k in 0..8 {
        let (mut h, bytes) = chunk(1, k);
        h.track_id = id;
        h.track_number = 21;
        c.write_chunk(&defs, &h, &column(&bytes)).unwrap();
    }
    let alone_b = hashes(&c.finish().unwrap());
    assert_eq!(alone_b[&(id, OwnerId(b))], rows[1].hash);
}
