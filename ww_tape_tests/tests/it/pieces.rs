//! Index, close, tail, piece chains and the positional reader, through the public API.

use std::path::{Path, PathBuf};
use ww_tape::TrackId;
use ww_tape::index::RECORDS_TRACK_ID;
use ww_tape::{ClockNum, RecordTrack, TrackNum};

use ww_tape::chunk::ChunkHead;
use ww_tape::index::IndexPolicy;
use ww_tape::piece::{ChainEnd, ChainLink, PieceId, check_chain};
use ww_tape::reader::{Found, OpenOptions};
use ww_tape::{
    BlockId, BlockKind, BlockWriter, ClockTime, CloseReason, Codec, ColumnKind, ColumnSpec,
    EndMode, FileBlocks, OwnerId, PieceChain, Recovery, SessionTag, Slot, SyncPolicy, TapeReader,
    TapeWriter,
};

/// A fresh scratch directory.
pub(crate) fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ww_tape_pieces_{name}_{}", std::process::id()));
    _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub(crate) fn records(id: BlockId, n: i64) -> Vec<u8> {
    let mut buf = [0u8; 256];
    let mut b = BlockWriter::new(&mut buf, BlockKind::Records, id, &[ClockNum::new(1)]).unwrap();
    b.push(
        TrackNum::new(9),
        ClockTime::new(ClockNum::new(1), n),
        &[&[n as u8; 30]],
    )
    .unwrap();
    let len = b.finish();
    buf[..len].to_vec()
}

/// The id [chunk] writes track `t` under: that of track `t + 1` of [crate::definitions::base].
pub(crate) fn chunk_id(t: u64) -> TrackId {
    crate::definitions::track_id(t as u32 + 1)
}

/// Track `t`'s chunk `k`: 50 items with raw starts, on track number `t + 1`.
pub(crate) fn chunk(t: u64, k: u64) -> (ChunkHead, Vec<u8>) {
    let first = k * 50;
    let starts: Vec<u8> = (0..50)
        .flat_map(|i| ((first + i) as i64 * 10 + t as i64).to_le_bytes())
        .collect();
    let head = ChunkHead {
        kind: BlockKind::Events,
        track_id: chunk_id(t),
        track_number: TrackNum::new(t as u32 + 1),
        first_item: first,
        count: 50,
        first_tick: first as i64 * 10 + t as i64,
        last_tick: (first + 49) as i64 * 10 + t as i64,
        latest_end: (first + 49) as i64 * 10 + t as i64,
        time_mode: 0,
        end_mode: EndMode::Absent,
        grid: None,
    };
    (head, starts)
}

pub(crate) fn column(bytes: &[u8]) -> [(ColumnSpec, &[u8]); 1] {
    [(
        ColumnSpec {
            kind: ColumnKind::Starts,
            codec: Codec::RAW,
            param: 0,
            len: bytes.len(),
        },
        bytes,
    )]
}

/// `rounds` × (a chunk of each of 3 tracks, a records block), checkpoints every 2 KiB.
fn write_tape(w: &mut TapeWriter<ww_tape_fs::FileSink>, rounds: u64) {
    w.set_index_policy(IndexPolicy {
        bytes: 2048,
        interval: None,
        ..IndexPolicy::default()
    })
    .expect("a policy with room for a piece head");
    for k in 0..rounds {
        for t in 0..3 {
            let (h, c) = chunk(t, k);
            w.write_chunk(crate::definitions::base_defs(), &h, &column(&c))
                .unwrap();
        }
        w.write_block(
            crate::definitions::base_defs(),
            &records(w.next_id(), k as i64),
        )
        .unwrap();
    }
}

/// Offset, length and kind of every block a FileBlocks walk finds.
fn walk(bytes: &[u8]) -> Vec<(u64, u32, BlockKind)> {
    let mut rd = FileBlocks::with_recovery(bytes, Recovery::Prefix);
    let mut out = vec![];
    while let Some(Ok(b)) = rd.next_block() {
        let (len, kind) = (b.len() as u32, b.kind);
        out.push((rd.offset(), len, kind));
    }
    out
}

fn listed(kind: BlockKind) -> bool {
    !matches!(
        kind,
        BlockKind::Index | BlockKind::Close | BlockKind::Tail | BlockKind::PieceHead
    )
}

/// `create` over an unclosed file a crash left refuses with `Exists` and keeps its bytes for a resume, and so over
/// an empty one.
#[test]
fn create_keeps_an_unclosed_file() {
    let dir = scratch("create_unclosed");
    let path = dir.join("a.wwt");
    let mut w = ww_tape_fs::writer::create(&path, SyncPolicy::None).unwrap();
    w.write_block(crate::definitions::base_defs(), &records(w.next_id(), 1))
        .unwrap();
    w.sync().unwrap();
    // a crash: the writer goes without closing
    drop(w);
    let before = std::fs::read(&path).unwrap();
    assert!(!before.is_empty());
    let r = ww_tape_fs::writer::create(&path, SyncPolicy::None);
    assert!(
        matches!(
            &r,
            Err(ww_tape::WriteError::Sink { error: ww_tape_fs::FsError::Exists(p), .. }) if *p == path
        ),
        "{:?}",
        r.err()
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    ww_tape_fs::writer::resume(&path, Default::default()).unwrap();
    let empty = dir.join("b.wwt");
    std::fs::write(&empty, b"").unwrap();
    let r = ww_tape_fs::writer::create(&empty, SyncPolicy::None);
    assert!(
        matches!(
            &r,
            Err(ww_tape::WriteError::Sink {
                error: ww_tape_fs::FsError::Exists(_),
                ..
            })
        ),
        "{:?}",
        r.err()
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn closed_file_is_found_by_its_tail() {
    let dir = scratch("tail");
    let path = dir.join("t.wwt");
    let mut w = ww_tape_fs::writer::create(&path, SyncPolicy::None).unwrap();
    write_tape(&mut w, 20);
    let done = w.close().unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(done.len, bytes.len() as u64);

    let r = TapeReader::open(&bytes[..]).unwrap();
    assert_eq!(r.pieces().len(), 1);
    let p = &r.pieces()[0];
    assert_eq!(
        (p.start, p.end, p.found),
        (0, bytes.len() as u64, Found::Tail)
    );
    let (close, ranges) = p.close.as_ref().unwrap();
    assert_eq!(close.reason, CloseReason::Closed);
    assert_eq!(close.hash, done.hash);
    assert!(close.checkpoint.is_some());
    assert!(r.verify(0).map(|v| v.holds).unwrap());
    close_rows_count_every_track(ranges, &done);

    let t1 = index_lists_every_block(&r, &bytes);

    // a flipped byte before the close fails the content hash
    let mut bad = bytes.clone();
    bad[100] ^= 1;
    assert!(
        !TapeReader::open(&bad[..])
            .unwrap()
            .verify(0)
            .map(|v| v.holds)
            .unwrap()
    );

    // the same through a file
    let f = ww_tape_fs::FileSource::open(&path).unwrap();
    let r = TapeReader::open(f).unwrap();
    assert_eq!(r.track_entries(0, chunk_id(1)).unwrap(), t1);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The close rows of [write_tape]'s piece: three tracks of chunks, one of records alone, each with its hash.
fn close_rows_count_every_track(ranges: &[ww_tape::piece::TrackRange], done: &ww_tape::Closed) {
    // three tracks of chunks, one of records alone
    assert_eq!(ranges.len(), 4);
    let rec = ranges
        .iter()
        .find(|t| t.track_number == TrackNum::new(9))
        .unwrap();
    assert_eq!((rec.chunks, rec.items, rec.records), (0, 0, 20));
    let want = (0..20i64).fold(ww_tape::TrackHash::START, |h, n| {
        h.record(n, &[n as u8; 30])
    });
    assert_eq!(rec.hash, want);
    assert!(
        ranges
            .iter()
            .filter(|t| t.chunks > 0)
            .all(|t| t.records == 0)
    );
    assert_eq!(
        done.tracks
            .iter()
            .find(|t| t.track_number == TrackNum::new(9))
            .unwrap()
            .hash,
        want
    );
    let t1 = ranges
        .iter()
        .find(|t| t.track_number == TrackNum::new(2))
        .unwrap();
    assert_eq!(
        (t1.track_id, t1.chunks, t1.first_item, t1.items),
        (chunk_id(1), 20, 0, 1000)
    );
}

/// The index of [write_tape]'s piece lists every block a walk finds; the entries of track 1, read back.
fn index_lists_every_block(r: &TapeReader<&[u8]>, bytes: &[u8]) -> Vec<ww_tape::index::IndexEntry> {
    // the index lists every block a walk finds, but the index, close and tail blocks
    let want: Vec<_> = walk(bytes).into_iter().filter(|b| listed(b.2)).collect();
    let mut got: Vec<_> = r
        .entries(0)
        .unwrap()
        .iter()
        .map(|e| (e.offset, e.len, e.kind))
        .collect();
    // a block of definitions has an entry per record
    got.dedup();
    assert_eq!(got, want);

    // one track from the final index's head and its own entries
    let t1 = r.track_entries(0, chunk_id(1)).unwrap();
    assert_eq!(t1.len(), 20);
    assert!(t1.windows(2).all(|w| w[0].end_item() == w[1].first_item));
    let mut buf = vec![];
    let b = r.read_block(0, &t1[3], &mut buf).unwrap();
    assert_eq!(b.chunk().unwrap().head().first_item, 150);
    let rec = r.track_entries(0, RECORDS_TRACK_ID).unwrap();
    assert_eq!(
        rec.iter().filter(|e| e.kind == BlockKind::Records).count(),
        20
    );
    let defs = r.track_entries(0, ww_tape::index::STATE_TRACK_ID).unwrap();
    assert_eq!(
        defs.len(),
        7,
        "an entry each: the layout, the producer, the clock and the four tracks with data, all close behind it"
    );
    assert!(defs.iter().all(|e| e.repeat && e.state().is_some()));
    t1
}

/// A writer that stopped anywhere: every block up to the cut is found, from the newest checkpoint and a scan of
/// at most the checkpoint interval (plus the blocks that straddle it).
#[test]
fn crash_anywhere_loses_only_the_cut_block() {
    let dir = scratch("crash");
    let path = dir.join("t.wwt");
    let mut w = ww_tape_fs::writer::create(&path, SyncPolicy::None).unwrap();
    write_tape(&mut w, 12);
    // the writer dies here: no close
    drop(w);
    let bytes = std::fs::read(dir.join("t.wwt")).unwrap();
    let blocks = walk(&bytes);
    assert!(blocks.iter().filter(|b| b.2 == BlockKind::Index).count() >= 3);
    for at in (0..=bytes.len()).step_by(7).chain([bytes.len()]) {
        let cut = &bytes[..at];
        let r = TapeReader::open(cut).unwrap();
        let want: Vec<_> = blocks
            .iter()
            .filter(|b| b.0 + u64::from(b.1) <= at as u64 && listed(b.2))
            .map(|b| (b.0, b.1, b.2))
            .collect();
        let mut got: Vec<_> = match r.pieces() {
            [] => vec![],
            [p] => r
                .entries(0)
                .unwrap()
                .iter()
                .map(|e| (e.offset + p.start, e.len, e.kind))
                .collect(),
            more => panic!("one piece, got {}", more.len()),
        };
        // a block of definitions has an entry per record
        got.dedup();
        assert_eq!(got, want, "cut at {at}");
        if let Some(p) = r.pieces().first() {
            assert!(p.close.is_none());
            let whole = blocks.iter().any(|b| b.0 + u64::from(b.1) == at as u64);
            assert_eq!(p.cut.is_some(), !whole && at > 0, "cut at {at}");
            if let Found::Checkpoint { scanned } = p.found {
                assert!(scanned <= 2048 + 4 * 600, "scanned {scanned} at {at}");
            }
        }
    }
    let r = TapeReader::open(&bytes[..]).unwrap();
    assert!(matches!(r.pieces()[0].found, Found::Checkpoint { .. }));
    assert!(r.verify(0).map(|v| v.holds).is_err(), "no close, no hash");
    // a short scan back finds no checkpoint: the whole file is scanned instead
    let r = TapeReader::open_with(
        &bytes[..],
        OpenOptions {
            scan_back_max: 16,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(matches!(r.pieces()[0].found, Found::Scan { .. }));
    let mut offsets: Vec<u64> = r.entries(0).unwrap().iter().map(|e| e.offset).collect();
    offsets.dedup();
    assert_eq!(offsets.len(), blocks.iter().filter(|b| listed(b.2)).count());
    std::fs::remove_dir_all(&dir).unwrap();
}

/// In an aligned file the checkpoints go into the gaps before chunk payloads, which stay on 4 KiB boundaries.
#[test]
fn aligned_file_places_checkpoints_in_gaps() {
    let dir = scratch("aligned");
    let path = dir.join("t.wwt");
    let mut w = ww_tape_fs::writer::create(&path, SyncPolicy::None).unwrap();
    w.set_alignment(ww_tape::ALIGNMENT).unwrap();
    write_tape(&mut w, 10);
    w.close().unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let mut rd = FileBlocks::new(&bytes[..]);
    let mut checkpoints = 0;
    while let Some(b) = rd.next_block() {
        let b = b.unwrap();
        let payload = b.chunk().map(|c| c.payload_offset() as u64);
        checkpoints += usize::from(b.kind == BlockKind::Index);
        if let Some(p) = payload {
            assert_eq!((rd.offset() + p) % 4096, 0);
        }
    }
    assert!(checkpoints >= 3);
    let r = TapeReader::open(&bytes[..]).unwrap();
    assert!(r.verify(0).map(|v| v.holds).unwrap());
    assert_eq!(r.track_entries(0, chunk_id(2)).unwrap().len(), 10);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Write chain `n` of a owner, pieces cut every 4 chunks; the piece files in order.
fn write_chain(dir: &Path, n: u64) -> Vec<PathBuf> {
    let mut chain = ww_tape_fs::chain::create(
        dir,
        OwnerId([0x42; 32]),
        n,
        &[],
        ww_tape::ChainOptions {
            tapes: ww_tape::PieceTapes::Each,
            writer: ww_tape::WriterOptions {
                sync: SyncPolicy::None,
                index: IndexPolicy {
                    bytes: 2048,
                    interval: None,
                    ..IndexPolicy::default()
                },
                ..Default::default()
            },
            cut: ww_tape::CutPolicy {
                chunks: 4,
                ..Default::default()
            },
        },
    )
    .unwrap();
    for k in 0..6 {
        for t in 0..2 {
            let (h, c) = chunk(t, k);
            chain
                .write_chunk(crate::definitions::base_defs(), &h, &column(&c))
                .unwrap();
        }
    }
    chain
        .close()
        .unwrap()
        .into_iter()
        .map(|f| ww_tape_fs::Files::new(dir).path_of(f.tape))
        .collect()
}

/// A chain whose piece lists all the state entries it may cuts to a new piece and puts there (writing side review
/// 2, point 8).
#[test]
fn a_chain_cuts_when_its_piece_is_full_of_state_entries() {
    let dir = scratch("state_full");
    let mut chain = ww_tape_fs::chain::create(
        &dir,
        OwnerId([0x43; 32]),
        0,
        &[],
        ww_tape::ChainOptions {
            tapes: ww_tape::PieceTapes::Each,
            writer: ww_tape::WriterOptions {
                sync: SyncPolicy::None,
                index: IndexPolicy {
                    // a piece's head, the clock, the track and its first item take 5
                    state_entries_max: 8,
                    ..IndexPolicy::default()
                },
                ..Default::default()
            },
            cut: ww_tape::CutPolicy::default(),
        },
    )
    .unwrap();
    let mut defs = crate::definitions::base_defs().clone();
    crate::definitions::hold(
        &mut defs,
        core::slice::from_ref(&crate::definitions::state(5)),
    )
    .unwrap();
    for k in 0..6u8 {
        assert!(
            chain
                .put(&defs, TrackNum::new(5), i64::from(k), &[k])
                .unwrap()
        );
    }
    let pieces: Vec<PathBuf> = chain
        .close()
        .unwrap()
        .into_iter()
        .map(|f| ww_tape_fs::Files::new(&dir).path_of(f.tape))
        .collect();
    assert_eq!(pieces.len(), 2);
    let bytes = std::fs::read(&pieces[1]).unwrap();
    let r = TapeReader::open(&bytes[..]).unwrap();
    let found = r
        .state_at(
            0,
            Slot::State {
                track: TrackNum::new(5),
            },
            u64::MAX,
        )
        .unwrap();
    assert_eq!(found.item.unwrap().item, [5]);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Two chains of one owner (an events chain and a series chain) have their own piece ids and session tags, so their
/// pieces read one after the other.
#[test]
fn chains_of_one_result_are_apart() {
    let (d0, d1) = (scratch("chain0"), scratch("chain1"));
    let (c0, c1) = (write_chain(&d0, 0), write_chain(&d1, 1));
    let (a, b) = (
        std::fs::read(&c0[0]).unwrap(),
        std::fs::read(&c1[0]).unwrap(),
    );
    let r = TapeReader::open(&a[..]).unwrap();
    let head = |bytes: &[u8]| {
        TapeReader::open(bytes).unwrap().pieces()[0]
            .head
            .clone()
            .unwrap()
            .0
    };
    let (h0, h1) = (head(&a), head(&b));
    assert_eq!(h0.ident.id, PieceId::first_of(&OwnerId([0x42; 32]), 0));
    assert_eq!(h1.ident.id, PieceId::first_of(&OwnerId([0x42; 32]), 1));
    assert_ne!(
        SessionTag::of_piece(&h0.ident.id),
        SessionTag::of_piece(&h1.ident.id)
    );
    assert!(r.verify(0).map(|v| v.holds).unwrap());
    let both = [a.clone(), b.clone()].concat();
    let mut rd = FileBlocks::new(&both[..]);
    let mut n = 0;
    while let Some(blk) = rd.next_block() {
        blk.unwrap();
        n += 1;
    }
    assert_eq!(n, walk(&a).len() + walk(&b).len());
    std::fs::remove_dir_all(&d0).unwrap();
    std::fs::remove_dir_all(&d1).unwrap();
}

#[test]
fn chain_of_pieces_links_both_ways() {
    let dir = scratch("chain");
    let paths = write_chain(&dir, 0);
    assert_eq!(paths.len(), 3);
    let again = scratch("chain_again");
    let paths2 = write_chain(&again, 0);
    for (a, b) in paths.iter().zip(&paths2) {
        assert_eq!(
            std::fs::read(a).unwrap(),
            std::fs::read(b).unwrap(),
            "same input, same bytes"
        );
    }
    std::fs::remove_dir_all(&again).unwrap();

    let files: Vec<Vec<u8>> = paths.iter().map(|p| std::fs::read(p).unwrap()).collect();
    let mut summaries = vec![];
    for (i, f) in files.iter().enumerate() {
        summaries.push(piece_reads_alone_and_links(i, f, i + 1 < files.len()));
    }
    assert!(check_chain(&summaries).is_whole());

    concatenated_pieces_read_as_one(&files);

    lost_pieces_and_tails_leave_the_rest(&files);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Piece `i` of [write_chain] reads alone, carries the definitions, continues the items of the piece before and names
/// the next piece's id when `has_next`; its summary for the chain check.
fn piece_reads_alone_and_links(i: usize, f: &[u8], has_next: bool) -> ww_tape::piece::PieceSummary {
    let r = TapeReader::open(f).unwrap();
    assert_eq!(r.pieces().len(), 1, "piece {i} reads alone");
    assert!(r.verify(0).map(|v| v.holds).unwrap());
    let p = &r.pieces()[0];
    let (head, starts) = p.head.as_ref().unwrap();
    assert_eq!(head.ident.seq, i as u64);
    // every piece carries the definitions
    assert!(
        !r.track_entries(0, ww_tape::index::STATE_TRACK_ID)
            .unwrap()
            .is_empty()
    );
    // items continue: the head's starts are where the previous piece's ranges ended
    let (_, ranges) = p.close.as_ref().unwrap();
    for s in starts {
        let rg = ranges.iter().find(|r| r.track_id == s.track_id).unwrap();
        assert_eq!(rg.first_item, s.next_item);
        assert!(rg.first_tick >= s.from_tick);
    }
    // the next id comes from this id and the content hash in the close, exactly
    let (close, _) = p.close.as_ref().unwrap();
    if has_next {
        let next = PieceId::after(head.ident.id, &close.hash);
        assert_eq!(close.next, Some(next));
        assert_eq!(p.tail.as_ref().unwrap().next, Some(next));
    }
    p.summary().unwrap()
}

/// Concatenated, the pieces of a chain are one tape: found by their tails, one small read each.
fn concatenated_pieces_read_as_one(files: &[Vec<u8>]) {
    // concatenated, the pieces are one tape: found by their tails, one small read each
    let all = files.concat();
    let r = TapeReader::open(&all[..]).unwrap();
    assert_eq!(r.pieces().len(), 3);
    assert!(r.pieces().iter().all(|p| p.found == Found::Tail));
    assert!(r.chain().is_whole());
    assert!((0..3).all(|i| r.verify(i).map(|v| v.holds).unwrap()));
    let t0: usize = (0..3)
        .map(|i| r.track_entries(i, chunk_id(0)).unwrap().len())
        .sum();
    assert_eq!(t0, 6);
    // and a stream reader walks it without a cut
    let mut rd = FileBlocks::new(&all[..]);
    let mut n = 0;
    while let Some(b) = rd.next_block() {
        b.unwrap();
        n += 1;
    }
    assert!(n > 20);
}

/// A chain without its first piece is head cut, not damaged; a last piece without its tail still gives its blocks.
fn lost_pieces_and_tails_leave_the_rest(files: &[Vec<u8>]) {
    // the first piece deleted (retention): head cut, not damage
    let rest = files[1..].concat();
    let r = TapeReader::open(&rest[..]).unwrap();
    let c = r.chain();
    assert!(c.head_cut);
    assert_eq!(c.links, [ChainLink::Linked]);
    assert_eq!(
        c.end,
        ChainEnd::Closed {
            reason: CloseReason::Closed
        }
    );

    // the last piece lost its tail in a crash: its blocks are still found
    let mut cut = files.concat();
    cut.truncate(cut.len() - 300);
    let r = TapeReader::open(&cut[..]).unwrap();
    assert_eq!(r.pieces().len(), 3);
    assert_eq!(r.chain().end, ChainEnd::Unclosed);
    assert!(
        r.entries(2)
            .unwrap()
            .iter()
            .any(|e| e.kind == BlockKind::Events)
    );
}

/// A piece in the middle of a chain whose head is damaged stays in the chain by the ident its close carries: linked
/// one way from the piece before, not a gap.
#[test]
fn a_chain_keeps_a_piece_whose_head_is_lost() {
    let dir = scratch("head_lost");
    let paths = write_chain(&dir, 0);
    let files: Vec<Vec<u8>> = paths.iter().map(|p| std::fs::read(p).unwrap()).collect();
    let mut all = files.concat();
    all[files[0].len() + 100] ^= 0x10;
    let r = TapeReader::open(&all[..]).unwrap();
    assert_eq!(r.pieces().len(), 3);
    assert!(r.pieces()[1].head.is_none());
    let c = r.chain();
    assert_eq!(c.order, [0, 1, 2]);
    assert_eq!(c.links, [ChainLink::HeadLost, ChainLink::Linked]);
    assert_eq!(
        c.end,
        ChainEnd::Closed {
            reason: CloseReason::Closed
        }
    );
    assert!(!c.is_whole());
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Closed pieces are multiples of 4 KiB long, so they clone back to back into one file: `std::io::copy` between
/// files is `copy_file_range` on Linux, which shares extents on btrfs and XFS and copies elsewhere.
#[test]
fn closed_pieces_clone_back_to_back() {
    let dir = scratch("clone");
    let paths = write_chain(&dir, 0);
    assert_eq!(paths.len(), 3);
    let joined = dir.join("joined.wwt");
    let mut out = std::fs::File::create_new(&joined).unwrap();
    let mut starts = vec![];
    for p in &paths {
        let len = std::fs::metadata(p).unwrap().len();
        assert_eq!(len % 4096, 0, "{} is {len} bytes", p.display());
        starts.push(out.metadata().unwrap().len());
        std::io::copy(&mut std::fs::File::open(p).unwrap(), &mut out).unwrap();
    }
    drop(out);
    let all = std::fs::read(&joined).unwrap();
    let r = TapeReader::open(&all[..]).unwrap();
    assert_eq!(
        r.pieces().iter().map(|p| p.start).collect::<Vec<_>>(),
        starts
    );
    assert!(r.pieces().iter().all(|p| p.found == Found::Tail));
    assert!(r.chain().is_whole());
    assert!((0..3).all(|i| r.verify(i).map(|v| v.holds).unwrap()));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// One file of closed pieces with an unclosed one after them (being written, or cut off by a crash): the
/// closed ones are found by their tails, the last one from its checkpoints and a scan, wherever it was cut.
#[test]
fn closed_pieces_then_an_unclosed_one() {
    let dir = scratch("then_open");
    let files: Vec<Vec<u8>> = write_chain(&dir, 0)
        .iter()
        .map(|p| std::fs::read(p).unwrap())
        .collect();
    let done = files[..2].concat();
    let last = &files[2];
    let blocks = walk(last);
    let (close_at, _, _) = *blocks
        .iter()
        .find(|(_, _, k)| *k == BlockKind::Close)
        .unwrap();
    // at every block boundary before the tail, and inside the padded close
    let mut cuts: Vec<u64> = blocks
        .iter()
        .map(|(at, _, _)| *at)
        .filter(|at| *at > 0)
        .collect();
    cuts.extend([close_at + 200, last.len() as u64 - 100]);
    for cut in cuts {
        let all = [&done[..], &last[..cut as usize]].concat();
        let r = TapeReader::open(&all[..]).unwrap();
        let p = r.pieces();
        assert_eq!(p.len(), 3, "cut at {cut}");
        assert!(
            p[..2]
                .iter()
                .all(|p| p.found == Found::Tail && p.tail.is_some()),
            "cut at {cut}: {:?}",
            p.iter()
                .map(|p| (p.start, p.found, p.end))
                .collect::<Vec<_>>()
        );
        assert_eq!(p[2].start, done.len() as u64, "cut at {cut}");
        assert!(p[2].tail.is_none(), "cut at {cut}");
        // cut inside its tail, the piece has its close record, read; without its tail the close is not whole, and
        // the chain ends unclosed, as a resume takes it
        let ended = cut > close_at + 200;
        assert_eq!(p[2].close.is_some(), ended, "cut at {cut}");
        assert!(!p[2].is_closed(), "cut at {cut}");
        assert_eq!(r.chain().end, ChainEnd::Unclosed, "cut at {cut}");
        assert!((0..2).all(|i| r.verify(i).map(|v| v.holds).unwrap()));
        let events = r
            .entries(2)
            .unwrap()
            .iter()
            .filter(|e| e.kind == BlockKind::Events)
            .count();
        let written = blocks
            .iter()
            .filter(|(at, len, k)| *k == BlockKind::Events && at + u64::from(*len) <= cut)
            .count();
        assert_eq!(events, written, "cut at {cut}");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The golden file: a closed two-piece chain, one block per line in hex (`WW_TAPE_GOLDEN=update` rewrites it).
#[test]
fn index_golden() {
    use ww_tape::{Definitions, Describe};
    let dir = scratch("golden");
    let mut chain = ww_tape_fs::chain::create(
        &dir,
        OwnerId([7; 32]),
        0,
        &[],
        ww_tape::ChainOptions {
            tapes: ww_tape::PieceTapes::Each,
            writer: ww_tape::WriterOptions {
                sync: SyncPolicy::None,
                index: IndexPolicy {
                    bytes: 1000,
                    interval: None,
                    ..IndexPolicy::default()
                },
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .unwrap();
    // track 3 with relations (a range relation among them) and an owner of its own, beside the base tracks
    let Describe::Track(t) = crate::relations::decoded() else {
        unreachable!("decoded() gives a track")
    };
    let related = Describe::Track(ww_tape::TrackDef {
        owner: [9; 32],
        ..t
    });
    let mut defs = Definitions::new();
    let mut all = crate::definitions::base(ww_tape::Layout::UNALIGNED)[2..].to_vec();
    for d in &mut all {
        if matches!(d, Describe::Track(t) if t.number.get() == 3) {
            *d = related.clone();
        }
    }
    crate::definitions::hold(&mut defs, &all).unwrap();
    let mut source = 100;
    let mut related_chunk = |chain: &mut PieceChain<_>, first: u64| {
        let sources: Vec<u64> = (0..4).map(|i| source + i / 2).collect();
        source += 2;
        let (h, cols) = crate::relations::relation_chunk(first, &sources, &[7, 7, 8, 9]);
        let cols: Vec<_> = cols.iter().map(|(s, b)| (*s, b.as_slice())).collect();
        chain.write_chunk(&defs, &h, &cols).unwrap();
    };
    for k in 0..2 {
        let (h, c) = chunk(0, k);
        chain.write_chunk(&defs, &h, &column(&c)).unwrap();
        let id = chain.writer().unwrap().next_id();
        chain.write_block(&defs, &records(id, k as i64)).unwrap();
        related_chunk(&mut chain, k * 4);
    }
    chain.cut_now(&defs).unwrap();
    let (h, c) = chunk(0, 2);
    chain.write_chunk(&defs, &h, &column(&c)).unwrap();
    related_chunk(&mut chain, 8);
    let paths: Vec<PathBuf> = chain
        .close()
        .unwrap()
        .into_iter()
        .map(|f| ww_tape_fs::Files::new(&dir).path_of(f.tape))
        .collect();
    let mut hex = String::new();
    for p in &paths {
        let bytes = std::fs::read(p).unwrap();
        let mut rd = FileBlocks::new(&bytes[..]);
        while let Some(b) = rd.next_block() {
            for byte in b.unwrap().bytes() {
                hex.push_str(&format!("{byte:02x}"));
            }
            hex.push('\n');
        }
    }
    std::fs::remove_dir_all(&dir).unwrap();
    let golden = Path::new(env!("CARGO_MANIFEST_DIR")).join("../ww_tape/golden/v1-index.hex");
    if std::env::var("WW_TAPE_GOLDEN").as_deref() == Ok("update") {
        std::fs::write(&golden, &hex).unwrap();
    }
    let want = std::fs::read_to_string(&golden)
        .expect("golden/v1-index.hex: run with WW_TAPE_GOLDEN=update");
    assert!(
        hex == want,
        "the format changed: bytes differ from golden/v1-index.hex"
    );
}

/// The golden incomplete chain: a piece that a resume kept with a damaged records block of track 9, the piece cut after
/// it and its close; its track's close row marks the hash incomplete (flags at row byte 128), the next piece's head
/// row carries the mark (row byte 12). In hex (`WW_TAPE_GOLDEN=update` rewrites it).
#[test]
fn incomplete_golden() {
    use ww_tape::{ChainOptions, Describe, WriterOptions};
    let dir = scratch("golden_incomplete");
    let owner = OwnerId([7; 32]);
    let options = || ChainOptions {
        tapes: ww_tape::PieceTapes::Each,
        writer: WriterOptions {
            sync: SyncPolicy::None,
            index: IndexPolicy {
                bytes: 300,
                interval: None,
                ..IndexPolicy::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let defs = crate::definitions::base_defs();
    let mut chain = ww_tape_fs::chain::create(&dir, owner, 0, &[], options()).unwrap();
    for n in 0..8 {
        let id = chain.writer().unwrap().next_id();
        chain.write_block(defs, &records(id, n)).unwrap();
    }
    drop(chain);
    let first = dir.join(ww_tape_fs::piece_name(0));
    let mut bytes = std::fs::read(&first).unwrap();
    let listed = TapeReader::open(&bytes[..])
        .unwrap()
        .entries(0)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == BlockKind::Records && !e.repeat)
        .unwrap();
    bytes[(listed.offset + u64::from(listed.len)) as usize - 8] ^= 1;
    std::fs::write(&first, &bytes).unwrap();
    let (mut chain, _) = ww_tape_fs::chain::resume(&dir, owner, 0, options()).unwrap();
    for n in 8..10 {
        let id = chain.writer().unwrap().next_id();
        chain.write_block(defs, &records(id, n)).unwrap();
    }
    chain.cut_now(defs).unwrap();
    let id = chain.writer().unwrap().next_id();
    chain.write_block(defs, &records(id, 10)).unwrap();
    let paths: Vec<PathBuf> = chain
        .close()
        .unwrap()
        .into_iter()
        .map(|f| ww_tape_fs::Files::new(&dir).path_of(f.tape))
        .collect();
    assert_eq!(paths.len(), 2);
    let Describe::Track(t9) = crate::definitions::track(9, crate::definitions::CLOCK) else {
        unreachable!("track() gives a track")
    };
    let pieces: Vec<Vec<u8>> = paths.iter().map(|p| std::fs::read(p).unwrap()).collect();
    // the close row of piece 0 and the head row of piece 1 at their byte offsets
    let first = TapeReader::open(&pieces[0][..]).unwrap();
    let rows = &first.pieces()[0].close.as_ref().unwrap().1;
    let row = rows.iter().position(|t| t.track_id == t9.id()).unwrap();
    assert!(rows[row].incomplete);
    let tail = ww_tape::Tail::read(&pieces[0][pieces[0].len() - ww_tape::TAIL_LEN..]).unwrap();
    let f = &pieces[0][tail.close_offset as usize + 192 + row * 136..][..136];
    assert_eq!(f[128..132], 1u32.to_le_bytes());
    let second = TapeReader::open(&pieces[1][..]).unwrap();
    let starts = &second.pieces()[0].head.as_ref().unwrap().1;
    let row = starts.iter().position(|t| t.track_id == t9.id()).unwrap();
    assert!(starts[row].incomplete);
    let h = &pieces[1][152 + row * 96..][..96];
    assert_eq!(h[12..16], 1u32.to_le_bytes());
    // the damaged block doesn't read as a block: 64 bytes per line, a blank line between pieces
    let mut hex = String::new();
    for bytes in &pieces {
        for line in bytes.chunks(64) {
            for byte in line {
                hex.push_str(&format!("{byte:02x}"));
            }
            hex.push('\n');
        }
        hex.push('\n');
    }
    std::fs::remove_dir_all(&dir).unwrap();
    let golden = Path::new(env!("CARGO_MANIFEST_DIR")).join("../ww_tape/golden/v1-incomplete.hex");
    if std::env::var("WW_TAPE_GOLDEN").as_deref() == Ok("update") {
        std::fs::write(&golden, &hex).unwrap();
    }
    let want = std::fs::read_to_string(&golden)
        .expect("golden/v1-incomplete.hex: run with WW_TAPE_GOLDEN=update");
    assert!(
        hex == want,
        "the format changed: bytes differ from golden/v1-incomplete.hex"
    );
}

#[test]
fn every_block_of_a_chain_carries_its_session() {
    let dir = scratch("sessions");
    let paths = write_chain(&dir, 0);
    assert!(paths.len() > 1, "the chain was cut");
    let session = SessionTag::of_piece(&PieceId::first(&OwnerId([0x42; 32])));
    assert!(session.is_session());
    let mut seqs = vec![];
    for (i, p) in paths.iter().enumerate() {
        let bytes = std::fs::read(p).unwrap();
        let r = TapeReader::open(&bytes[..]).unwrap();
        let (head, _) = r.pieces()[0].head.clone().unwrap();
        // a cut names the session going on; the first piece follows nothing
        let before = if i == 0 { SessionTag::NONE } else { session };
        assert_eq!(head.prev_session, before);
        let mut rd = FileBlocks::new(&bytes[..]);
        while let Some(b) = rd.next_block() {
            let b = b.unwrap();
            assert_eq!(
                b.session,
                session,
                "{:?} at {} of {}",
                b.kind,
                rd.offset(),
                p.display()
            );
            if listed(b.kind) {
                seqs.push(b.seq);
            }
        }
    }
    // puts carry the seq before them; data blocks count on across pieces
    seqs.dedup();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The blocks of definitions in `bytes` as a walk finds them: offset, and whether put ([ww_tape::FLAG_REPEAT]).
fn definitions_at(bytes: &[u8]) -> Vec<(u64, bool)> {
    crate::definitions::record_blocks(bytes)
        .into_iter()
        .filter(|b| b.first_track == Some(0))
        .map(|b| (b.offset, b.repeat))
        .collect()
}

/// Every piece of the chain holds only what the writer put: the layout and the producer after its head, each
/// track's definitions before its data.
#[test]
fn definitions_are_put_in_each_piece() {
    let dir = scratch("puts");
    let paths = write_chain(&dir, 0);
    let marks: Vec<Vec<bool>> = paths
        .iter()
        .map(|p| {
            let bytes = std::fs::read(p).unwrap();
            definitions_at(&bytes).into_iter().map(|d| d.1).collect()
        })
        .collect();
    assert!(
        marks.iter().all(|m| !m.is_empty() && m.iter().all(|r| *r)),
        "{marks:?}"
    );
    for p in &paths[1..] {
        let bytes = std::fs::read(p).unwrap();
        let r = TapeReader::open(&bytes[..]).unwrap();
        let defs = r.definitions_at(0, u64::MAX).unwrap();
        for slot in [
            Slot::Layout,
            Slot::Producer,
            Slot::Clock {
                number: ClockNum::new(1),
            },
        ] {
            assert!(defs.items.contains_key(&slot), "{p:?}: {slot:?}");
        }
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Track 0's chunk `k` of 8192 items: 64 KiB of raw starts.
fn big_chunk(k: u64) -> (ChunkHead, Vec<u8>) {
    let n = 8192u64;
    let first = k * n;
    let starts: Vec<u8> = (first..first + n)
        .flat_map(|i| (i as i64).to_le_bytes())
        .collect();
    let head = ChunkHead {
        kind: BlockKind::Events,
        track_id: chunk_id(0),
        track_number: TrackNum::new(1),
        first_item: first,
        count: n as u32,
        first_tick: first as i64,
        last_tick: (first + n - 1) as i64,
        latest_end: (first + n - 1) as i64,
        time_mode: 0,
        end_mode: EndMode::Absent,
        grid: None,
    };
    (head, starts)
}

/// A piece with a damaged block of definitions still has them, from the copies the writer put on later uses; the
/// damaged block is reported where it was the newest; found through the checkpoints of an unclosed piece and
/// through the final index of a closed one.
#[test]
fn a_damaged_block_of_definitions_reads_from_their_copies() {
    for closed in [false, true] {
        let dir = scratch(&format!("damaged_describe_{closed}"));
        let path = dir.join("p.wwt");
        let mut w = ww_tape_fs::writer::create(&path, SyncPolicy::None).unwrap();
        let owner = OwnerId([7; 32]);
        let head = ww_tape::PieceHead {
            ident: ww_tape::PieceIdent {
                id: PieceId::first(&owner),
                owner,
                seq: 0,
            },
            prev: None,
            prev_session: SessionTag::NONE,
            integrity: ww_tape::Integrity::Full,
        };
        w.begin_piece(&head, &[]).unwrap();
        for k in 0..64 {
            let (h, c) = big_chunk(k);
            w.write_chunk(crate::definitions::base_defs(), &h, &column(&c))
                .unwrap();
        }
        let bytes = if closed {
            w.close().unwrap();
            std::fs::read(&path).unwrap()
        } else {
            drop(w);
            std::fs::read(dir.join("p.wwt")).unwrap()
        };
        assert!(definitions_at(&bytes).iter().all(|d| d.1), "all put");
        // track 1's definition put before its first chunk, then copied on a use one copy spacing (1 MiB) on
        let describe = crate::definitions::writes_of(
            &bytes,
            Slot::Track {
                number: TrackNum::new(1),
            },
        );
        assert!(describe.len() >= 2, "{describe:?}");
        assert!((1 << 20..2 << 20).contains(&describe[1]), "{describe:?}");

        let mut damaged = bytes.clone();
        damaged[describe[0] as usize + 40] ^= 1;
        let whole = TapeReader::open(&bytes[..])
            .unwrap()
            .definitions_at(0, u64::MAX)
            .unwrap();
        let rd = TapeReader::open(&damaged[..]).unwrap();
        assert_eq!(rd.pieces().len(), 1);
        let defs = rd.definitions_at(0, u64::MAX).unwrap();
        assert!(
            defs.is_whole(),
            "the copies are the newest: {:?}",
            defs.lost
        );
        assert_eq!(defs.items(), whole.items(), "closed {closed}");
        // just after the damaged block, before any copy, the definitions of track 1 are lost
        let early = rd
            .state_at(
                0,
                Slot::Track {
                    number: TrackNum::new(1),
                },
                describe[0] + 1,
            )
            .unwrap();
        assert_eq!(early.item, None);
        assert_eq!(early.lost.len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

/// The near policy set on a writer carries over to the next piece of a cut.
#[test]
fn the_near_policy_carries_over_a_cut() {
    let dir = scratch("near_cut");
    let mut w = ww_tape_fs::writer::create(&dir.join("a.wwt"), SyncPolicy::None).unwrap();
    w.set_index_policy(IndexPolicy::NONE)
        .expect("a policy with room for a piece head");
    let policy = ww_tape::writer::NearPolicy {
        copy_spacing: 64 << 10,
        ..Default::default()
    };
    w.set_near_policy(policy);
    let owner = OwnerId([8; 32]);
    let head = ww_tape::PieceHead {
        ident: ww_tape::PieceIdent {
            id: PieceId::first(&owner),
            owner,
            seq: 0,
        },
        prev: None,
        prev_session: SessionTag::NONE,
        integrity: ww_tape::Integrity::Full,
    };
    w.begin_piece(&head, &[]).unwrap();
    let (_, mut next) =
        ww_tape_fs::writer::cut(w, &ww_tape::Definitions::new(), &dir.join("b.wwt")).unwrap();
    assert_eq!(next.options().near, policy);
    for k in 0..4 {
        let (h, c) = big_chunk(k);
        next.write_chunk(crate::definitions::base_defs(), &h, &column(&c))
            .unwrap();
    }
    next.close().unwrap();
    let bytes = std::fs::read(dir.join("b.wwt")).unwrap();
    let puts = crate::definitions::puts(&bytes);
    // the head's layout and producer, track 1's definitions before its first chunk, then their copies 64 KiB on
    assert!(puts.len() >= 3, "{puts:?}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A block written again with other bytes (the order a writer makes: records, a full repeat, the records again)
/// takes the place of the first in the index as in a walk, whether the piece is read by its final index or scanned.
#[test]
fn a_replaced_block_leaves_the_index() {
    let dir = scratch("replaced");
    let path = dir.join("r.wwt");
    let mut w = ww_tape_fs::writer::create(&path, SyncPolicy::None).unwrap();
    let id = w.next_id();
    w.write_block(crate::definitions::base_defs(), &records(id, 1))
        .unwrap();
    w.write_block(crate::definitions::base_defs(), &records(id, 2))
        .unwrap();
    // the same bytes again: dropped, never listed
    w.write_block(crate::definitions::base_defs(), &records(id, 2))
        .unwrap();
    for n in 3..6 {
        w.write_block(crate::definitions::base_defs(), &records(w.next_id(), n))
            .unwrap();
    }
    let unclosed = std::fs::read(w.sink().path()).unwrap();
    w.close().unwrap();
    let closed = std::fs::read(&path).unwrap();
    for bytes in [unclosed, closed] {
        // the blocks a walk ends up with: a replacement takes the place of the last counted block
        let mut rd = FileBlocks::new(&bytes[..]);
        let (mut walked, mut last, mut replaced, mut seqs) = (vec![], None, 0, vec![]);
        while let Some(b) = rd.next_block() {
            match b {
                Ok(b) if listed(b.kind) => {
                    if b.kind == BlockKind::Records
                        && b.first_track() != Some(RecordTrack::Definitions)
                    {
                        seqs.push(b.seq);
                    }
                    if !b.is_repeat() {
                        last = Some(walked.len());
                    }
                    walked.push(rd.offset());
                }
                Ok(_) => {}
                Err(ww_tape::FileError::Replaced { .. }) => {
                    walked.remove(last.take().unwrap());
                    seqs.pop();
                    replaced += 1;
                }
                Err(e) => panic!("{e}"),
            }
        }
        assert_eq!(replaced, 1);
        let r = TapeReader::open(&bytes[..]).unwrap();
        let mut listed: Vec<u64> = r.entries(0).unwrap().iter().map(|e| e.offset).collect();
        listed.dedup();
        assert_eq!(listed, walked);
        assert_eq!(seqs, [id.seq, id.seq + 1, id.seq + 2, id.seq + 3]);
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Definitions in many small blocks, at the head and mid-piece: every block of data, in this piece and the next of a
/// cut, has the definitions it needs close behind it.
#[test]
fn every_block_of_data_has_its_definitions_behind_it() {
    let dir = scratch("behind");
    let mut w = ww_tape_fs::writer::create(&dir.join("a.wwt"), SyncPolicy::None).unwrap();
    w.set_index_policy(IndexPolicy::NONE)
        .expect("a policy with room for a piece head");
    let owner = OwnerId([9; 32]);
    let head = ww_tape::PieceHead {
        ident: ww_tape::PieceIdent {
            id: PieceId::first(&owner),
            owner,
            seq: 0,
        },
        prev: None,
        prev_session: SessionTag::NONE,
        integrity: ww_tape::Integrity::Full,
    };
    w.begin_piece(&head, &[]).unwrap();
    let mut defs = crate::definitions::base_defs().clone();
    for n in 0..50 {
        w.write_block(&defs, &records(w.next_id(), n)).unwrap();
    }
    crate::definitions::hold(
        &mut defs,
        core::slice::from_ref(&crate::definitions::track(10, crate::definitions::CLOCK)),
    )
    .unwrap();
    for n in 50..100 {
        w.write_block(&defs, &records(w.next_id(), n)).unwrap();
    }
    let (_, mut next) = ww_tape_fs::writer::cut(w, &defs, &dir.join("b.wwt")).unwrap();
    for n in 100..110 {
        next.write_block(&defs, &records(next.next_id(), n))
            .unwrap();
    }
    next.close().unwrap();
    for path in [dir.join("a.wwt"), dir.join("b.wwt")] {
        let bytes = std::fs::read(&path).unwrap();
        let r = TapeReader::open(&bytes[..]).unwrap();
        let data: Vec<u64> = r
            .track_entries(0, RECORDS_TRACK_ID)
            .unwrap()
            .iter()
            .map(|e| e.offset)
            .collect();
        assert!(!data.is_empty());
        for at in data {
            for slot in [
                Slot::Track {
                    number: TrackNum::new(9),
                },
                Slot::Clock {
                    number: ClockNum::new(1),
                },
            ] {
                let found = r.state_at(0, slot, at).unwrap();
                assert!(found.item.is_some(), "{path:?} at {at}: {slot:?}");
            }
        }
        // track 10 has no data: its definition is in no piece
        let ten = r.definitions_at(0, u64::MAX).unwrap();
        assert!(!ten.items.contains_key(&Slot::Track {
            number: TrackNum::new(10)
        }));
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A chain of pieces in memory; the keys of its tapes.
pub(crate) fn chain_in_memory(mem: &ww_tape::MemTapes) -> Vec<u64> {
    let mut chain = PieceChain::create_on(
        mem.clone(),
        OwnerId([0x44; 32]),
        0,
        &[],
        ww_tape::ChainOptions {
            tapes: ww_tape::PieceTapes::Each,
            writer: ww_tape::WriterOptions {
                sync: SyncPolicy::None,
                ..Default::default()
            },
            cut: ww_tape::CutPolicy {
                chunks: 2,
                ..Default::default()
            },
        },
    )
    .unwrap();
    let mut defs = crate::definitions::base_defs().clone();
    crate::definitions::hold(
        &mut defs,
        core::slice::from_ref(&crate::definitions::state(5)),
    )
    .unwrap();
    for k in 0..8u8 {
        assert!(
            chain
                .put(&defs, TrackNum::new(5), i64::from(k), &[k])
                .unwrap()
        );
        let (h, c) = chunk(0, u64::from(k));
        chain.write_chunk(&defs, &h, &column(&c)).unwrap();
    }
    chain.close().unwrap().into_iter().map(|f| f.tape).collect()
}

/// A chain in memory whose first pieces were removed reads as a head cut, as the same chain in files does, and each
/// remaining piece gives its definitions and its newest state from its own head.
#[test]
fn a_chain_in_memory_without_its_first_pieces_is_head_cut() {
    let mem = ww_tape::MemTapes::new();
    let paths = chain_in_memory(&mem);
    assert!(paths.len() >= 4, "{} pieces", paths.len());
    let bytes: Vec<Vec<u8>> = paths
        .iter()
        .map(|p| mem.get(p).unwrap().unwrap().to_vec().unwrap())
        .collect();
    // whole: no head cut
    let all = bytes.concat();
    let r = TapeReader::open(&all[..]).unwrap();
    assert!(r.chain().is_whole());

    // a reader that holds the first piece keeps reading it after its removal
    let held = mem.get(&paths[0]).unwrap().unwrap();
    for p in &paths[..2] {
        assert!(mem.remove(p).unwrap());
        assert!(mem.get(p).unwrap().is_none());
    }
    assert_eq!(held.to_vec().unwrap(), bytes[0]);
    let left: Vec<Vec<u8>> = mem
        .keys()
        .unwrap()
        .iter()
        .map(|p| mem.get(p).unwrap().unwrap().to_vec().unwrap())
        .collect();
    assert_eq!(left, bytes[2..]);

    let rest = left.concat();
    let r = TapeReader::open(&rest[..]).unwrap();
    let c = r.chain();
    assert!(c.head_cut);
    assert!(c.links.iter().all(|l| *l == ChainLink::Linked));
    assert_eq!(
        c.end,
        ChainEnd::Closed {
            reason: CloseReason::Closed
        }
    );
    // every remaining piece alone knows the tracks and its newest state
    let mut newest_per_piece = Vec::new();
    for i in 0..left.len() {
        let defs = r.definitions_at(i, u64::MAX).unwrap();
        assert!(
            defs.items.contains_key(&Slot::Track {
                number: TrackNum::new(1)
            }),
            "piece {i}: {:?}",
            defs.items.keys().collect::<Vec<_>>()
        );
        let newest = r
            .state_at(
                i,
                Slot::State {
                    track: TrackNum::new(5),
                },
                u64::MAX,
            )
            .unwrap()
            .item
            .unwrap_or_else(|| panic!("piece {i} has no state"));
        newest_per_piece.push(newest.item[0]);
    }
    // the first pieces' puts are gone, yet every piece holds one of its own, and the last the newest
    assert!(newest_per_piece.iter().all(|k| *k >= 2));
    assert!(newest_per_piece.windows(2).all(|w| w[0] <= w[1]));
    assert_eq!(newest_per_piece.last(), Some(&7));
}

/// Removing a piece: one still being written is refused, one that isn't there says so, and a reader holding a
/// removed piece reads it on.
#[test]
fn mem_tapes_remove_a_closed_piece_only() {
    let mem = ww_tape::MemTapes::new();
    let mut chain = PieceChain::create_on(
        mem.clone(),
        OwnerId([0x45; 32]),
        0,
        &[],
        ww_tape::ChainOptions::default(),
    )
    .unwrap();
    let open = 0;
    assert_eq!(mem.remove(&open), Err(ww_tape::MemError::Unclosed(open)));
    assert!(mem.get(&open).unwrap().is_some(), "refused: still there");
    assert!(!mem.remove(&1).unwrap());
    let reader = mem.get(&open).unwrap().unwrap();
    chain.writer().unwrap();
    let done = chain.close().unwrap();
    assert_eq!(done.len(), 1);
    assert!(mem.remove(&open).unwrap());
    assert!(!mem.remove(&open).unwrap());
    assert!(mem.keys().unwrap().is_empty());
    assert!(TapeReader::open(&reader.to_vec().unwrap()[..]).is_ok());
    // the name is free again
    assert!(ww_tape::Tapes::create(&mut mem.clone(), &open).is_ok());
}

/// A refusal of a memory store names the tape by its key, as a value of the key's type.
#[test]
fn a_memory_store_refusal_names_its_key() {
    let mem: ww_tape::MemTapes = ww_tape::MemTapes::new();
    let _sink = mem.create(&7).unwrap();
    let exists = mem.create(&7).unwrap_err();
    assert_eq!(exists, ww_tape::MemError::Exists(7));
    assert_eq!(exists.to_string(), "the memory tape 7 exists");
    assert_eq!(mem.reopen(&7).err(), Some(ww_tape::MemError::Locked(7)));
    let named: ww_tape::MemTapes<(u8, &str)> = ww_tape::MemTapes::new();
    let _sink = named.create(&(1, "run")).unwrap();
    assert_eq!(
        named.remove(&(1, "run")),
        Err(ww_tape::MemError::Unclosed((1, "run")))
    );
}
