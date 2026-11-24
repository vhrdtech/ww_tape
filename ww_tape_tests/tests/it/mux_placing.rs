//! A chunk sender's receipts (MUX-4): each placement names the piece and bytes of its chunk, read back from the file
//! by a reader in the writer's process while the mux writes, before any sync; in a chain of piece files, in one file,
//! and in one file resumed, where a piece starts past the file's start.

use std::path::Path;

use ww_tape::{Block, ByteSource, PieceTapes};
use ww_tape_fs::FileSource;
use ww_tape_mux::{ChunkSender, MuxOptions, Placement};

use crate::follow::{OWNER, define, events_chunk, options};
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
