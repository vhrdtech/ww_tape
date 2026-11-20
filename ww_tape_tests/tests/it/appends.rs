//! Appends on files and with chunks: a file cut at every byte of an append, read and written on through
//! `ww_tape_fs`, and events chunks across an append against the same chunks written in one go.

use std::collections::BTreeSet;
use std::path::Path;

use crate::pieces::{chunk, chunk_id, column, scratch};
use crate::reclose::{close, closes, reopen, write};
use crate::resume::{begin, options};
use ww_tape::piece::PIECE_ALIGN;
use ww_tape::reader::ByteSource;
use ww_tape::{ALIGNMENT, BlockKind, BlockWriter, MemSink, TapeReader, TapeWriter};
use ww_tape_fs::{FileSource, lock_name};

/// The ticks of the record blocks of data the entries of piece 0 list, each block once, read back by the reader.
fn listed_ticks<S: ByteSource>(r: &TapeReader<S>) -> Vec<i64>
where
    S::Error: std::fmt::Debug,
{
    let mut out = vec![];
    let mut seen = BTreeSet::new();
    let mut buf = vec![];
    for e in r.entries(0).unwrap() {
        if e.kind != BlockKind::Records || e.track_id != 0 || e.repeat || !seen.insert(e.offset) {
            continue;
        }
        let b = r.read_block(0, &e, &mut buf).unwrap();
        assert_eq!(
            b.kind,
            BlockKind::Records,
            "the entry at {} names it",
            e.offset
        );
        out.push(b.records().next().unwrap().unwrap().at.unwrap().ticks);
    }
    out
}

/// A reader of the file at `path`.
fn open(path: &Path) -> TapeReader<FileSource> {
    TapeReader::open(FileSource::open(path).unwrap()).unwrap()
}

/// A piece closed twice, the file cut at every byte of the append: the reader of the file takes the old close, no
/// damage, and lists the old items and a prefix of the new ones. `ww_tape_fs::writer::resume` takes the file without
/// moving a byte before the old tail, writes the lost items on and closes: the file reads all of them, verifies and
/// holds no lock file.
fn every_byte_cut(alignment: u32, name: &str) {
    let mut w = begin(alignment);
    write(&mut w, 0..6);
    let old = close(w);
    let (mut w, _) = reopen(&old, alignment);
    write(&mut w, 6..12);
    let new = close(w);
    let dir = scratch(name);
    let path = dir.join("p.wwt");
    let mut torn_listed = 0;
    for cut in old.len()..=new.len() {
        std::fs::write(&path, &new[..cut]).unwrap();
        let whole = cut == new.len();
        let listed = torn_file_reads_the_old_close(&path, cut, &old, &new);
        if !whole && listed > 6 {
            torn_listed += 1;
        }
        torn_file_is_written_on(&path, cut, whole, &old, alignment);
    }
    assert!(torn_listed > 0, "a torn append lists its whole blocks");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The content hash the last close of `bytes` states.
fn last_close_hash(bytes: &[u8]) -> ww_tape::ContentHash {
    closes(bytes).last().unwrap().1.hash
}

/// The file at `path`, `new` cut at `cut`, reads the close in force of `old` (of `new` when whole), no damage, the old
/// items and a prefix of the new ones. Returns how many items it lists.
fn torn_file_reads_the_old_close(path: &Path, cut: usize, old: &[u8], new: &[u8]) -> usize {
    let whole = cut == new.len();
    let r = open(path);
    assert_eq!(r.pieces().len(), 1, "cut at {cut}");
    let p = &r.pieces()[0];
    let (in_force, _) = p.close.as_ref().expect("a close is in force");
    assert_eq!(
        in_force.hash,
        last_close_hash(if whole { new } else { old }),
        "cut at {cut}"
    );
    assert!(
        p.gaps.is_empty() && p.replaced.is_empty(),
        "cut at {cut}: damage"
    );
    assert!(
        p.head_error.is_none() && p.close_error.is_none(),
        "cut at {cut}"
    );
    assert_eq!(
        p.closed_end(),
        Some(if whole { new.len() } else { old.len() } as u64),
        "cut at {cut}"
    );
    assert!(r.verify(0).unwrap().holds, "cut at {cut}");
    let listed = listed_ticks(&r);
    assert!(
        listed.starts_with(&[0, 1, 2, 3, 4, 5]),
        "cut at {cut}: {listed:?}"
    );
    assert!(
        (0..12).collect::<Vec<_>>().starts_with(&listed),
        "cut at {cut}: {listed:?}"
    );
    listed.len()
}

/// A resume of the file at `path` keeps every byte of `old`, writes the lost items on and closes: the file reads all
/// of them, verifies and holds no lock file.
fn torn_file_is_written_on(path: &Path, cut: usize, whole: bool, old: &[u8], alignment: u32) {
    let (mut w, resumed) = ww_tape_fs::writer::resume(path, options(alignment))
        .unwrap_or_else(|e| panic!("cut at {cut}: {e:?}"));
    assert!(resumed.kept_bytes >= old.len() as u64, "cut at {cut}");
    let kept = std::fs::read(path).unwrap();
    assert_eq!(
        kept[..old.len()],
        old[..],
        "cut at {cut}: the old bytes moved"
    );
    let have = crate::reclose::ticks(&kept);
    write(&mut w, (6..14).filter(|n| !have.contains(n)));
    w.close().unwrap();
    assert!(!lock_name(path).exists(), "cut at {cut}: lock file left");
    let got = std::fs::read(path).unwrap();
    assert_eq!(got[..old.len()], old[..], "cut at {cut}");
    let r = open(path);
    assert_eq!(
        listed_ticks(&r),
        (0..14).collect::<Vec<_>>(),
        "cut at {cut}"
    );
    assert!(r.verify(0).unwrap().holds, "cut at {cut}");
    assert!(r.pieces()[0].is_closed(), "cut at {cut}");
    assert_eq!(
        r.closes(0).unwrap().len(),
        if whole { 3 } else { 2 },
        "cut at {cut}"
    );
}

#[test]
fn a_file_cut_at_every_byte_of_an_append_reads_and_is_written_on() {
    every_byte_cut(0, "every_byte_cut");
}

#[test]
fn an_aligned_file_cut_at_every_byte_of_an_append_reads_and_is_written_on() {
    every_byte_cut(ALIGNMENT, "every_byte_cut_aligned");
}

/// Give `w` a chunk of each of tracks 0 and 1 for each of `ks`.
fn put_chunks(w: &mut TapeWriter<MemSink>, ks: std::ops::Range<u64>) {
    let defs = crate::definitions::base_defs();
    for k in ks {
        for t in 0..2 {
            let (h, c) = chunk(t, k);
            w.write_chunk(defs, &h, &column(&c)).unwrap();
        }
    }
}

/// Events chunks written in two stretches, closed after each, read as the same chunks written in one go: the close
/// rows, the track hashes and the chunks each track lists. With the last tail torn, the old close lists its chunks.
#[test]
fn events_chunks_across_an_append_read_as_written_in_one_go() {
    let mut w = begin(0);
    put_chunks(&mut w, 0..6);
    let tape = w.sink().tape();
    let one_go = w.close().unwrap();
    let whole = tape.to_vec().unwrap();
    let mut w = begin(0);
    put_chunks(&mut w, 0..3);
    let first = close(w);
    let (mut w, _) = reopen(&first, 0);
    put_chunks(&mut w, 3..6);
    let tape = w.sink().tape();
    let twice = w.close().unwrap();
    let re = tape.to_vec().unwrap();
    assert_eq!(
        re[..first.len()],
        first[..],
        "no byte before the old tail moves"
    );
    let hashes = |c: &ww_tape::Closed| {
        c.tracks
            .iter()
            .map(|t| (t.track_id, t.next_item, t.hash))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        hashes(&twice),
        hashes(&one_go),
        "track hashes go on over the append"
    );
    let read = |bytes: &[u8]| {
        let r = TapeReader::open(bytes).unwrap();
        assert!(r.verify(0).unwrap().holds);
        let p = &r.pieces()[0];
        let rows: Vec<_> = p
            .close
            .as_ref()
            .unwrap()
            .1
            .iter()
            .map(|t| (t.track_number, t.chunks, t.first_item, t.items, t.hash))
            .collect();
        let listed: Vec<Vec<u64>> = (0..2)
            .map(|t| {
                r.track_entries(0, chunk_id(t))
                    .unwrap()
                    .iter()
                    .map(|e| e.first_item)
                    .collect()
            })
            .collect();
        (r.closes(0).unwrap().len(), rows, listed)
    };
    let (n_whole, rows_whole, listed_whole) = read(&whole);
    let (n_re, rows_re, listed_re) = read(&re);
    assert_eq!((n_whole, n_re), (1, 2));
    assert_eq!(rows_re, rows_whole, "close rows");
    assert_eq!(listed_re, listed_whole, "chunks each track lists");
    assert_eq!(listed_re[0], (0..6).map(|k| k * 50).collect::<Vec<_>>());
    let torn = &re[..re.len() - 10];
    let r = TapeReader::open(torn).unwrap();
    assert!(!r.pieces()[0].is_closed());
    let firsts: Vec<_> = r
        .track_entries(0, chunk_id(0))
        .unwrap()
        .iter()
        .map(|e| e.first_item)
        .collect();
    assert!(firsts.starts_with(&[0, 50, 100]), "{firsts:?}");
}

/// A closed piece opened to write and closed again with nothing appended writes nothing: not an append head, not a
/// byte. On a file its bytes and its modification time stay as they were; in memory the bytes do. An append of one
/// small block costs one 4 KiB step.
#[test]
fn a_close_with_nothing_appended_writes_nothing() {
    for alignment in [0, ALIGNMENT] {
        let mut w = begin(alignment);
        write(&mut w, 0..6);
        let old = close(w);
        let (w, resumed) = reopen(&old, alignment);
        assert!(resumed.after_close.is_some());
        assert_eq!(close(w), old, "alignment {alignment}: in memory");
        let (mut w, _) = reopen(&old, alignment);
        write(&mut w, 6..7);
        assert_eq!(
            close(w).len() - old.len(),
            PIECE_ALIGN as usize,
            "alignment {alignment}: the smallest append costs one 4 KiB step"
        );
        let dir = scratch(&format!("nothing_appended_{alignment}"));
        let path = dir.join("p.wwt");
        std::fs::write(&path, &old).unwrap();
        let then =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(then)
            .unwrap();
        let (w, _) = ww_tape_fs::writer::resume(&path, options(alignment)).unwrap();
        let closed = w.close().unwrap();
        assert_eq!(
            closed.hash,
            closes(&old).last().unwrap().1.hash,
            "the close in force stays"
        );
        assert_eq!(closed.len, old.len() as u64);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            old,
            "alignment {alignment}: file bytes"
        );
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(
            modified, then,
            "alignment {alignment}: file modification time"
        );
        assert!(!lock_name(&path).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

/// A block deferred after a resume of a closed aligned piece waits for a gap, and is still waiting at the close: the
/// close appends it, then a new close, and never drops it. A records block with no record needs no definition, so
/// nothing at all is written before that close.
#[test]
fn a_deferred_block_waiting_at_the_close_is_appended() {
    let mut w = begin(ALIGNMENT);
    write(&mut w, 0..6);
    let old = close(w);
    for empty in [false, true] {
        let (mut w, _) = reopen(&old, ALIGNMENT);
        let block = if empty {
            let mut buf = [0u8; 256];
            let b = BlockWriter::new(&mut buf, BlockKind::Records, w.next_id(), &[1]).unwrap();
            let len = b.finish();
            buf[..len].to_vec()
        } else {
            crate::pieces::records(w.next_id(), 6)
        };
        let tape = w.sink().tape();
        w.defer_block(crate::definitions::base_defs(), block)
            .unwrap();
        let written = tape.to_vec().unwrap().len();
        assert_eq!(
            written == old.len(),
            empty,
            "empty {empty}: bytes before the close"
        );
        let got = close(w);
        assert_eq!(got[..old.len()], old[..], "empty {empty}");
        if empty {
            assert_eq!(
                got.len() - old.len(),
                PIECE_ALIGN as usize,
                "one 4 KiB step"
            );
        } else {
            assert_eq!(crate::reclose::ticks(&got), (0..7).collect::<Vec<_>>());
        }
        assert_eq!(
            closes(&got).len(),
            2,
            "empty {empty}: the deferred block is appended"
        );
    }
}

/// `PieceInfo::closed_end` takes only a tail that ends the close in force: given an older tail, it names no end.
#[test]
fn closed_end_takes_only_a_tail_that_ends_its_close() {
    let mut w = begin(0);
    write(&mut w, 0..6);
    let old = close(w);
    let (mut w, _) = reopen(&old, 0);
    write(&mut w, 6..9);
    let new = close(w);
    let older = TapeReader::open(&old[..]).unwrap().pieces()[0]
        .tail
        .unwrap();
    let r = TapeReader::open(&new[..]).unwrap();
    let mut p = r.pieces()[0].clone();
    assert_eq!(p.closed_end(), Some(new.len() as u64));
    p.tail = Some(older);
    assert_eq!(p.closed_end(), None, "an older tail ends no close in force");
    assert!(!p.is_closed());
}
