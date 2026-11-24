//! A chunk sender's receipts (MUX-4): each placement names the piece and bytes of its chunk, read back from the file
//! by a reader in the writer's process while the mux writes, before any sync; in a chain of piece files, in one file,
//! and in one file resumed, where a piece starts past the file's start.

use std::path::Path;

use std::sync::atomic::Ordering;

use ww_tape::chunk::ChunkHead;
use ww_tape::index::IndexEntry;
use ww_tape::{
    Block, ByteSource, Integrity, MemPieces, PieceTapes, Sealed, TapeReader, TrackNum, UnitHash,
};
use ww_tape_fs::FileSource;
use ww_tape_mux::{ChunkSender, MuxError, MuxOptions, Placement, Pressure, TapeMux, Unplaced};

use crate::follow::{OWNER, define, events_chunk, options};
use crate::mux::{Rigged, rigged_mux};
use crate::pieces::scratch;

/// Send chunks `from..from + n` of track 1 through `s` with receipts, reading each chunk back at its placement while
/// the mux writes on; the placements.
fn send_and_read(
    s: &ChunkSender<ww_tape_fs::FilePieces>,
    tape: &dyn Fn(u64) -> std::path::PathBuf,
    from: u64,
    n: u64,
) -> Vec<Placement> {
    let mut out = Vec::new();
    for k in from..from + n {
        let (head, columns) = events_chunk(1, k);
        let placing = s
            .send_placed(head, columns, None)
            .unwrap()
            .expect("a waiting sender drops nothing");
        let p = placing.wait().unwrap();
        assert_eq!(p.track, head.track_number);
        assert_eq!(p.first_item, head.first_item);
        assert_eq!(p.count, head.count);
        let file = FileSource::open(&tape(p.piece)).unwrap();
        let mut bytes = vec![0; p.len as usize];
        file.read_at(p.tape_offset(), &mut bytes).unwrap();
        let block = Block::read(&bytes).expect("the placement holds a whole block");
        let chunk = block.chunk().expect("a chunk block");
        assert_eq!(chunk.head(), head, "the chunk sent, at its placement");
        out.push(p);
    }
    out
}

fn piece_path(dir: &Path, seq: u64) -> std::path::PathBuf {
    dir.join(ww_tape_fs::piece_name(seq))
}

#[test]
fn a_placement_reads_back_from_its_piece_file_while_the_mux_writes() {
    let dir = scratch("placing-each");
    let mux =
        ww_tape_fs::mux::create(&dir, OWNER, 0, &[], options(3), MuxOptions::default()).unwrap();
    let senders = define(&mux);
    let placed = send_and_read(&senders[1], &|seq| piece_path(&dir, seq), 0, 8);
    let pieces: Vec<u64> = placed.iter().map(|p| p.piece).collect();
    assert_eq!(pieces, [0, 0, 0, 1, 1, 1, 2, 2], "a cut every 3 chunks");
    assert!(
        placed.iter().all(|p| p.piece_start == 0),
        "each piece starts its own file"
    );
    drop(senders);
    mux.close().unwrap();
}

#[test]
fn a_placement_reads_back_from_one_file_also_after_a_resume() {
    let dir = scratch("placing-one");
    let file = dir.join("chain.wwt");
    let one = || {
        let mut o = options(3);
        o.tapes = PieceTapes::One;
        o
    };
    let mux = ww_tape_fs::mux::create(&file, OWNER, 0, &[], one(), MuxOptions::default()).unwrap();
    let senders = define(&mux);
    let placed = send_and_read(&senders[1], &|_| file.clone(), 0, 7);
    let starts: Vec<bool> = placed.iter().map(|p| p.piece_start > 0).collect();
    assert_eq!(
        starts,
        [false, false, false, true, true, true, true],
        "later pieces start past the first"
    );
    drop(senders);
    mux.close().unwrap();

    let (mux, _) = ww_tape_fs::mux::resume(&file, OWNER, 0, one(), MuxOptions::default()).unwrap();
    let senders = define(&mux);
    let again = send_and_read(&senders[1], &|_| file.clone(), 7, 4);
    let last = placed.last().unwrap();
    assert!(
        again[0].tape_offset() > last.tape_offset(),
        "a resume writes on behind the close"
    );
    drop(senders);
    mux.close().unwrap();
}

#[test]
fn a_receipt_taken_once_gives_nothing_more() {
    let dir = scratch("placing-take");
    let mux =
        ww_tape_fs::mux::create(&dir, OWNER, 0, &[], options(0), MuxOptions::default()).unwrap();
    let senders = define(&mux);
    let (head, columns) = events_chunk(1, 0);
    let mut placing = senders[1]
        .send_placed(head, columns, None)
        .unwrap()
        .expect("a waiting sender drops nothing");
    // the flush is placed behind the chunk: its receipt is out
    senders[1].flush().unwrap();
    let p = placing.try_take().expect("answered").unwrap();
    assert_eq!(p.first_item, head.first_item);
    assert_eq!(placing.try_take(), None, "the answer is given once");
    drop(senders);
    mux.close().unwrap();
}

/// Chunks of track 1 sent with receipts through a mux on `mem` of `integrity`, a cut every 3 chunks; the
/// placements and the heads sent.
fn placed_at(mem: &MemPieces, integrity: Integrity) -> Vec<(Placement, ChunkHead)> {
    let mut o = options(3);
    o.writer.integrity = integrity;
    let mux = TapeMux::create(mem.clone(), OWNER, 0, &[], o, MuxOptions::default()).unwrap();
    let senders = define(&mux);
    let out = (0..7)
        .map(|k| {
            let (head, columns) = events_chunk(1, k);
            let placing = senders[1]
                .send_placed(head, columns, None)
                .unwrap()
                .unwrap();
            (placing.wait().unwrap(), head)
        })
        .collect();
    drop(senders);
    mux.close().unwrap();
    out
}

/// The index entry of the block at `p` in its piece of `mem`, after the chunk sent there is checked to be at `p`.
fn entry_at(mem: &MemPieces, p: &Placement, head: &ChunkHead) -> IndexEntry {
    let piece = mem.get(&p.piece).unwrap().unwrap().to_vec().unwrap();
    let at = usize::try_from(p.offset).unwrap();
    let block = Block::read(&piece[at..]).unwrap();
    assert_eq!(block.len(), p.len as usize);
    assert_eq!(block.chunk().unwrap().head(), *head);
    let r = TapeReader::open(piece.as_slice()).unwrap();
    r.entries(0)
        .unwrap()
        .into_iter()
        .find(|e| e.offset == p.offset)
        .expect("the index lists the block")
}

#[test]
fn receipts_hold_at_every_integrity_level_and_through_a_seal() {
    let full = MemPieces::new();
    let want = placed_at(&full, Integrity::Full);
    for (p, head) in &want {
        let e = entry_at(&full, p, head);
        assert_eq!(
            (e.len, e.unit),
            (p.len, p.unit),
            "the receipt is the index's"
        );
        assert_ne!(p.unit, UnitHash::NONE);
    }
    for level in [Integrity::CrcOnly, Integrity::None] {
        let mem = MemPieces::new();
        let placed = placed_at(&mem, level);
        for ((p, head), (f, _)) in placed.iter().zip(&want) {
            let e = entry_at(&mem, p, head);
            assert!(e.unhashed, "{level:?}");
            assert_eq!(
                (e.len, e.unit),
                (p.len, p.unit),
                "{level:?}: the receipt is the index's"
            );
            assert_eq!(
                (p.piece, p.offset, p.len),
                (f.piece, f.offset, f.len),
                "{level:?}"
            );
        }
        // a seal writes over the same bytes: the receipts still place each chunk, under the unit hash of Full
        let mut sealed: Vec<Sealed> = vec![];
        for k in 0..3 {
            sealed.extend(mem.seal(&k, sealed.last()).unwrap().unwrap());
        }
        for ((p, head), (f, _)) in placed.iter().zip(&want) {
            let e = entry_at(&mem, p, head);
            assert!(!e.unhashed, "{level:?} sealed");
            assert_eq!(e.unit, f.unit, "{level:?} sealed: the unit hash of Full");
        }
    }
}

#[test]
fn a_refused_dropped_or_lost_chunk_has_no_placement() {
    let rigged = Rigged::new();
    let mux = rigged_mux(&rigged, 4 << 10);
    let sender = mux.chunks(TrackNum::new(1)).unwrap();
    let send = |k| {
        let (head, columns) = events_chunk(0, k);
        sender.send_placed(head, columns, None).unwrap()
    };
    send(0).unwrap().wait().unwrap();
    // a chunk of another track's id under the sender's number: the writer refuses it, and the sender's next call
    // says why
    let (mut head, columns) = events_chunk(0, 1);
    head.track_id = events_chunk(1, 0).0.track_id;
    let placing = sender.send_placed(head, columns, None).unwrap().unwrap();
    assert_eq!(placing.wait(), Err(Unplaced::Refused));
    assert!(matches!(sender.flush(), Err(MuxError::Refused(_))));

    // a dropping sender whose queue is full sends no chunk and gives no receipt
    let drop_sender = mux
        .chunks(TrackNum::new(2))
        .unwrap()
        .pressure(Pressure::Drop);
    mux.flush().unwrap();
    rigged.rig.set(false);
    let mut receipts = Vec::new();
    let mut dropped = 0;
    for k in 0..40 {
        let (head, columns) = events_chunk(1, k);
        match drop_sender.send_placed(head, columns, None).unwrap() {
            Some(p) => receipts.push(p),
            None => dropped += 1,
        }
    }
    assert!(dropped > 0, "the queue holds 4 KiB");
    rigged.rig.set(true);
    for p in receipts {
        p.wait().unwrap();
    }

    // a write that fails stops the mux: the chunk is lost
    rigged.rig.fail.store(true, Ordering::SeqCst);
    assert_eq!(send(1).unwrap().wait(), Err(Unplaced::Lost));
    assert!(matches!(mux.flush(), Err(MuxError::Failed(_))));
}
