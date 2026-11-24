//! The running hash of each track ([TrackHash]) and the owner of a track ([TrackDef::owner]), through the public API.

use ww_tape::ItemNum;

use std::collections::BTreeMap;
use ww_tape::TrackId;
use ww_tape::{RecordTrack, TrackNum};

use crate::definitions::{CLOCK, base_defs, state, track};
use crate::id;
use crate::pieces::{chunk, column, records};
use ww_tape::index::IndexPolicy;
use ww_tape::piece::payload_hash;
use ww_tape::writer::NearPolicy;
use ww_tape::{
    BlockKind, ChainOptions, ChainPiece, Closed, CutPolicy, Describe, FileBlocks, Layout,
    MemPieces, OwnerId, PieceChain, PieceTapes, SyncPolicy, TapeReader, TrackDef, TrackHash,
    WriterOptions,
};

/// How a recording of the same tracks is written.
#[derive(Copy, Clone, Debug)]
struct Shape {
    /// With tracks 2 and 3 and puts on a state track between the calls on tracks 1 and 9.
    interleaved: bool,
    /// A cut once a piece holds this many chunks; 0 never.
    cut: u64,
    tapes: PieceTapes,
    layout: Layout,
    /// How the blocks of track 9 and the chunks of track 1 reach the writer.
    way: Way,
    /// A crash and a resume after half of the rounds.
    resume: bool,
}

/// How a [Shape] gives its blocks to the writer.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Way {
    /// Each chunk by `write_chunk`, each records block by `write_block`.
    Write,
    /// Track 1's chunk and the first records block of a round deferred, the second written.
    Defer,
    /// Each records block after another one with its id and other bytes, which it replaces.
    Replace,
    /// Each records block adopted.
    Adopt,
}

fn options(shape: Shape) -> ChainOptions {
    ChainOptions {
        tapes: shape.tapes,
        writer: WriterOptions {
            sync: SyncPolicy::None,
            clock: ww_tape::ClockRef::NONE,
            index: IndexPolicy {
                bytes: exact_units::Bytes::new(1024),
                interval: None,
                ..IndexPolicy::default()
            },
            layout: shape.layout,
            producer: "track_hash".to_owned(),
            near: NearPolicy::default(),
            integrity: ww_tape::Integrity::Full,
        },
        cut: CutPolicy {
            chunks: if shape.cut == 0 {
                exact_units::Count::new(u64::MAX)
            } else {
                exact_units::Count::new(shape.cut)
            },
            ..CutPolicy::default()
        },
    }
}

/// The bytes of the tapes the pieces `done` were written to, in the order written.
fn files_of(mem: &MemPieces, done: &[ChainPiece<u64>]) -> Vec<u8> {
    let mut keys: Vec<u64> = vec![];
    for f in done {
        if !keys.contains(&f.tape) {
            keys.push(f.tape);
        }
    }
    keys.iter()
        .flat_map(|k| mem.get(k).unwrap().unwrap().to_vec().unwrap())
        .collect()
}

/// The pieces of the chain `done`, without their tapes.
fn pieces(done: Vec<ChainPiece<u64>>) -> Vec<Closed> {
    done.into_iter().map(|f| f.piece).collect()
}

/// Each track's hash after the chain `done`, by id and owner.
fn hashes(done: &[Closed]) -> BTreeMap<(TrackId, OwnerId), TrackHash> {
    done.last()
        .unwrap()
        .tracks
        .iter()
        .map(|t| ((t.track_id, t.owner), t.hash))
        .collect()
}

/// 12 rounds of a chunk of track 1 and a records block on track 9, written as `shape` says; the closed pieces and
/// the bytes of their files.
fn write(shape: Shape) -> (Vec<Closed>, Vec<u8>) {
    let mut defs = base_defs().clone();
    crate::definitions::hold(&mut defs, core::slice::from_ref(&state(10))).unwrap();
    let mut c =
        PieceChain::create_on(MemPieces::new(), OwnerId([5; 32]), 0, &[], options(shape)).unwrap();
    for k in 0..12u64 {
        if shape.resume && k == 6 {
            c.writer().unwrap().flush_deferred().unwrap();
            let mem = c.store().clone();
            // a crash: the open piece is left as it is
            drop(c);
            (c, _) = PieceChain::resume_on(mem, OwnerId([5; 32]), 0, options(shape)).unwrap();
        }
        if shape.interleaved {
            let (h, b) = chunk(1, k);
            c.write_chunk(&defs, &h, &column(&b)).unwrap();
            c.writer()
                .unwrap()
                .put(&defs, TrackNum::new(10), k as i64, &[k as u8; 20])
                .unwrap();
        }
        let (h, b) = chunk(0, k);
        if shape.way == Way::Defer {
            let block = chunk_block(&h, &b);
            c.writer().unwrap().defer_block(&defs, block).unwrap();
        } else {
            c.write_chunk(&defs, &h, &column(&b)).unwrap();
        }
        if shape.interleaved {
            let (h, b) = chunk(2, k);
            c.write_chunk(&defs, &h, &column(&b)).unwrap();
        }
        let w = c.writer().unwrap();
        for n in [2 * k as i64, 2 * k as i64 + 1] {
            match shape.way {
                Way::Defer if n % 2 == 0 => w.defer_block(&defs, records(id(0), n)).unwrap(),
                Way::Write | Way::Defer => {
                    w.write_block(&defs, &records(w.next_id(), n)).unwrap();
                }
                Way::Replace => {
                    let id = w.next_id();
                    w.write_block(&defs, &records(id, 1000 + n)).unwrap();
                    w.write_block(&defs, &records(id, n)).unwrap();
                }
                Way::Adopt => {
                    w.adopt_block(&defs, &records(id(n as u64), n)).unwrap();
                }
            }
        }
    }
    let mem = c.store().clone();
    let done = c.close().unwrap();
    let bytes = files_of(&mem, &done);
    (pieces(done), bytes)
}

/// The whole chunk block of `head` and its raw starts `starts`, for a writer to number.
fn chunk_block(head: &ww_tape::ChunkHead, starts: &[u8]) -> Vec<u8> {
    let cols = column(starts);
    let specs: Vec<_> = cols.iter().map(|c| c.0).collect();
    let mut block = vec![0u8; ww_tape::chunk::chunk_len(head, &specs, 0)];
    let n = ww_tape::chunk::write_chunk(&mut block, id(0), head, &cols, 0).unwrap();
    block.truncate(n);
    block
}

/// Track 9's id: [crate::pieces::records] writes on it.
fn track_9_id() -> TrackId {
    let Describe::Track(t) = track(9, CLOCK) else {
        unreachable!("track() gives a track")
    };
    t.id()
}

/// The hash of track `number` folded by a reader from the good blocks of `bytes`: each chunk of the track by its
/// payload's hash, each record not marked as a repeat; a block replaced by the next one is taken back.
fn fold(bytes: &[u8], number: u32) -> TrackHash {
    let mut h = TrackHash::START;
    // the hash before the last counted block
    let mut before = h;
    let mut rd = FileBlocks::new(bytes);
    while let Some(b) = rd.next_block() {
        let b = match b {
            Err(ww_tape::FileError::Replaced { .. }) => {
                h = before;
                continue;
            }
            b => b.unwrap(),
        };
        if !b.is_repeat() && !ww_tape::index::is_structural(b.kind) {
            before = h;
        }
        if let Some(c) = b.chunk() {
            if c.head().track_number == TrackNum::new(number) {
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
            if r.track == RecordTrack::from_u32(number) {
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
        tapes: PieceTapes::Each,
        layout: Layout::UNALIGNED,
        way: Way::Write,
        resume: false,
    };
    let (done, bytes) = write(alone);
    let want = hashes(&done);
    let keys = [
        (crate::pieces::chunk_id(0), OwnerId::NONE),
        (track_9_id(), OwnerId::NONE),
    ];
    assert_eq!(want[&keys[0]], fold(&bytes, 1), "track 1 read back");
    assert_eq!(want[&keys[1]], fold(&bytes, 9), "track 9 read back");
    assert_ne!(want[&keys[0]], TrackHash::START);
    assert_ne!(want[&keys[1]], TrackHash::START);

    for shape in shapes(alone) {
        same_hash_in(shape, &want, &keys);
    }
}

/// The shapes a track is written in besides `alone`: interleaved, cut every 1 to 5 chunks, a file per piece or one,
/// either layout; and deferred, replaced and adopted blocks and a resume, in one piece and cut.
fn shapes(alone: Shape) -> Vec<Shape> {
    let mut shapes = vec![Shape {
        interleaved: true,
        ..alone
    }];
    for cut in 1..=5 {
        for files in [PieceTapes::Each, PieceTapes::One] {
            for layout in [Layout::UNALIGNED, Layout::ALIGNED] {
                shapes.push(Shape {
                    interleaved: true,
                    cut,
                    tapes: files,
                    layout,
                    ..alone
                });
            }
        }
    }
    // deferred, replaced and adopted blocks, and a resume, in one piece and cut, in either layout
    for way in [Way::Write, Way::Defer, Way::Replace, Way::Adopt] {
        for (cut, files) in [
            (0, PieceTapes::Each),
            (3, PieceTapes::Each),
            (3, PieceTapes::One),
        ] {
            for layout in [Layout::UNALIGNED, Layout::ALIGNED] {
                for resume in [false, true] {
                    shapes.push(Shape {
                        interleaved: true,
                        cut,
                        tapes: files,
                        layout,
                        way,
                        resume,
                    });
                }
            }
        }
    }
    shapes
}

/// Tracks 1 and 9 written in `shape` have the hashes `want` of `keys`, by the writer, a reader's fold and every
/// piece's close rows.
fn same_hash_in(
    shape: Shape,
    want: &BTreeMap<(TrackId, OwnerId), TrackHash>,
    keys: &[(TrackId, OwnerId); 2],
) {
    let (done, bytes) = write(shape);
    if shape.cut != 0 {
        assert!(done.len() > 1, "{shape:?}: {} pieces", done.len());
    }
    let got = hashes(&done);
    for k in keys {
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
    let Describe::Track(t10) = state(10) else {
        unreachable!("state() gives a track")
    };
    let puts = got.get(&(t10.id(), OwnerId::NONE));
    assert!(
        puts.is_none_or(|h| *h == TrackHash::START),
        "{shape:?}: {puts:?}"
    );
    // every piece's close rows say the same as the writer
    let r = TapeReader::open(&bytes[..]).unwrap();
    assert_eq!(r.pieces().len(), done.len(), "{shape:?}");
    let mut rows_seen = 0;
    for (p, f) in r.pieces().iter().zip(&done) {
        let (_, rows) = p.close.as_ref().unwrap();
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

/// Track `number` with the key of track 20, owned by `owner`.
fn owned(number: u32, owner: [u8; 32]) -> TrackDef<'static> {
    let Describe::Track(t) = track(20, CLOCK) else {
        unreachable!("track() gives a track")
    };
    TrackDef {
        number: TrackNum::new(number),
        owner,
        ..t
    }
}

/// 8 chunks on each track of `tracks`, alternating; the closed chain.
fn write_owned(tracks: &[TrackDef<'static>]) -> Vec<Closed> {
    let mut all = crate::definitions::base(Layout::UNALIGNED)[2..].to_vec();
    all.extend(tracks.iter().map(|t| Describe::Track(t.clone())));
    let defs = crate::definitions::held(&all);
    let shape = Shape {
        interleaved: false,
        cut: 5,
        tapes: PieceTapes::One,
        layout: Layout::UNALIGNED,
        way: Way::Write,
        resume: false,
    };
    let mut c =
        PieceChain::create_on(MemPieces::new(), OwnerId([5; 32]), 0, &[], options(shape)).unwrap();
    for k in 0..8 {
        for (i, t) in tracks.iter().enumerate() {
            let (mut h, b) = chunk(i as u64, k);
            h.track_id = t.id();
            h.track_number = t.number;
            c.write_chunk(&defs, &h, &column(&b)).unwrap();
        }
    }
    pieces(c.close().unwrap())
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
    assert_eq!(rows[0].track_number, TrackNum::new(20));
    assert_eq!(rows[1].track_number, TrackNum::new(21));
    assert_eq!(rows[0].next_item, ItemNum::new(8 * 50));
    assert_eq!(rows[1].next_item, ItemNum::new(8 * 50));
    assert_ne!(rows[0].hash, rows[1].hash);

    // alone, as its chunks in both: the first track's chunks are chunk(0, k), the second's chunk(1, k)
    let alone_a = hashes(&write_owned(&[ta]));
    assert_eq!(alone_a[&(id, OwnerId(a))], rows[0].hash);
    let mut all = crate::definitions::base(Layout::UNALIGNED)[2..].to_vec();
    all.push(Describe::Track(tb.clone()));
    let defs = crate::definitions::held(&all);
    let mut c = PieceChain::create_on(
        MemPieces::new(),
        OwnerId([5; 32]),
        0,
        &[],
        options(Shape {
            interleaved: false,
            cut: 0,
            tapes: PieceTapes::Each,
            layout: Layout::UNALIGNED,
            way: Way::Write,
            resume: false,
        }),
    )
    .unwrap();
    for k in 0..8 {
        let (mut h, bytes) = chunk(1, k);
        h.track_id = id;
        h.track_number = TrackNum::new(21);
        c.write_chunk(&defs, &h, &column(&bytes)).unwrap();
    }
    let alone_b = hashes(&pieces(c.close().unwrap()));
    assert_eq!(alone_b[&(id, OwnerId(b))], rows[1].hash);
}
