//! Appends on files and with chunks: a file cut at every byte of an append, read and written on through
//! `ww_tape_fs`, and events chunks across an append against the same chunks written in one go.

use std::collections::BTreeSet;
use std::path::Path;

use crate::pieces::{chunk, chunk_id, column, scratch};
use crate::reclose::{close, closes, reopen, write};
use crate::resume::{begin, options};
use ww_tape::reader::ByteSource;
use ww_tape::{ALIGNMENT, BlockKind, MemSink, TapeReader, TapeWriter};
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
    let hash = |bytes: &[u8]| closes(bytes).last().unwrap().1.hash;
    let dir = scratch(name);
    let path = dir.join("p.wwt");
    let mut torn_listed = 0;
    for cut in old.len()..=new.len() {
        std::fs::write(&path, &new[..cut]).unwrap();
        let whole = cut == new.len();
        let r = open(&path);
        assert_eq!(r.pieces().len(), 1, "cut at {cut}");
        let p = &r.pieces()[0];
        let (in_force, _) = p.close.as_ref().expect("a close is in force");
        assert_eq!(
            in_force.hash,
            hash(if whole { &new } else { &old }),
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
        if !whole && listed.len() > 6 {
            torn_listed += 1;
        }
        drop(r);
        let (mut w, resumed) = ww_tape_fs::writer::resume(&path, options(alignment))
            .unwrap_or_else(|e| panic!("cut at {cut}: {e:?}"));
        assert!(resumed.kept_bytes >= old.len() as u64, "cut at {cut}");
        let kept = std::fs::read(&path).unwrap();
        assert_eq!(
            kept[..old.len()],
            old[..],
            "cut at {cut}: the old bytes moved"
        );
        let have = crate::reclose::ticks(&kept);
        write(&mut w, (6..14).filter(|n| !have.contains(n)));
        w.close().unwrap();
        assert!(!lock_name(&path).exists(), "cut at {cut}: lock file left");
        let got = std::fs::read(&path).unwrap();
        assert_eq!(got[..old.len()], old[..], "cut at {cut}");
        let r = open(&path);
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
    assert!(torn_listed > 0, "a torn append lists its whole blocks");
    std::fs::remove_dir_all(&dir).unwrap();
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
