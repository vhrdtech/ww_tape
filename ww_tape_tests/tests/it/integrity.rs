//! Pieces of each integrity level ([Integrity]) against the same calls at [Integrity::Full]: the same blocks in the
//! same places, the hashes and (at None) the payload CRCs absent and marked so, and every reader saying what it
//! could not check.

use ww_tape::index::IndexEntry;
use ww_tape::sink::TapeSink;
use ww_tape::{
    BlockKind, Checked, Integrity, ItemNum, MemTapes, OwnerId, PieceTapes, Sealed, TapeReader,
    TrackHash, piece::ChainLink,
};

use crate::range_hash::{id_of, write_at};

/// Each block of `bytes` read by a walk: its offset, length, kind and seq.
fn layout(bytes: &[u8]) -> Vec<(u64, usize, BlockKind, u64)> {
    let mut rd = ww_tape::FileBlocks::with_recovery(bytes, ww_tape::Recovery::Prefix);
    let mut out = vec![];
    loop {
        let at = rd.offset();
        match rd.next_block() {
            Some(Ok(b)) => out.push((at, b.len(), b.kind, b.seq)),
            Some(Err(e)) => panic!("a walk of a piece of any level reads: {e:?}"),
            None => return out,
        }
    }
}

/// Every index entry of every piece of `r`.
fn entries<S: ww_tape::reader::ByteSource>(r: &TapeReader<S>) -> Vec<Vec<IndexEntry>>
where
    S::Error: core::fmt::Debug,
{
    (0..r.pieces().len())
        .map(|i| r.entries(i).unwrap())
        .collect()
}

/// Piece `i` of `r`, of `level`, listing `entries`: it names its level, its structure verifies, its close rows hold
/// no hash, and its events chunks read Unprotected at None, Verified above.
fn check_piece<S: ww_tape::reader::ByteSource>(
    r: &TapeReader<S>,
    i: usize,
    entries: &[IndexEntry],
    level: Integrity,
) where
    S::Error: core::fmt::Debug,
{
    assert_eq!(r.pieces()[i].integrity(), level);
    let v = r.verify(i).unwrap();
    assert!(v.holds, "{level:?} piece {i}");
    assert_eq!(v.integrity, level);
    let (_, rows) = r.pieces()[i].close.clone().unwrap();
    assert!(
        rows.iter()
            .all(|r| r.incomplete && r.hash == TrackHash::START)
    );
    let want = match level {
        Integrity::None => Checked::Unprotected,
        _ => Checked::Verified,
    };
    let mut buf = vec![];
    for e in entries.iter().filter(|e| e.kind == BlockKind::Events) {
        assert_eq!(r.read_block(i, e, &mut buf).unwrap().checked(), want);
    }
}

/// Each track's range hash of `r`, of `level`, is that of `full`, every chunk hashed from its bytes, those of a
/// piece of None counted unprotected.
fn check_ranges<S: ww_tape::reader::ByteSource>(
    r: &TapeReader<S>,
    full: &TapeReader<S>,
    level: Integrity,
) where
    S::Error: core::fmt::Debug,
{
    for t in 0..3 {
        let range = ItemNum::new(0)..ItemNum::new(u64::MAX);
        let want = full
            .range_hash(id_of(t), OwnerId::NONE, range.clone(), TrackHash::START)
            .unwrap();
        let got = r
            .range_hash(id_of(t), OwnerId::NONE, range, TrackHash::START)
            .unwrap();
        assert_eq!((got.hash, got.chunks), (want.hash, want.chunks));
        assert_eq!(got.read, got.chunks, "every chunk hashed from its bytes");
        let unprotected = match level {
            Integrity::None => got.chunks,
            _ => 0,
        };
        assert_eq!(got.unprotected, unprotected);
    }
}

/// A chain written at each level lays its blocks where a chain of Full does; its entries say the same but for the
/// unit hash; each piece names its level, verifies its structure and links to the next by id; a chunk reads
/// Unprotected at None and Verified at CrcOnly; and a range's hash, folded from the chunks' bytes, is Full's.
#[test]
fn a_chain_of_each_level_is_full_without_its_hashes() {
    for tapes in [PieceTapes::Each, PieceTapes::One] {
        let (_, full) = write_at(tapes, 0x1e7e1, Integrity::Full);
        for level in [Integrity::CrcOnly, Integrity::None] {
            let (done, bytes) = write_at(tapes, 0x1e7e1, level);
            assert_eq!(bytes.len(), full.len());
            assert!(
                done.iter()
                    .all(|d| d.hash == ww_tape::ContentHash::default())
            );
            for (b, f) in bytes.iter().zip(&full) {
                assert_eq!(layout(b), layout(f), "{level:?} {tapes:?}");
                let (r, fr) = (
                    TapeReader::open(&b[..]).unwrap(),
                    TapeReader::open(&f[..]).unwrap(),
                );
                let (es, fs) = (entries(&r), entries(&fr));
                for (e, f) in es.iter().flatten().zip(fs.iter().flatten()) {
                    assert!(e.unhashed && e.unit.is_none(), "{e:?}");
                    let f = IndexEntry {
                        unit: e.unit,
                        unhashed: true,
                        ..*f
                    };
                    assert_eq!(*e, f);
                }
                for (i, e) in es.iter().enumerate() {
                    check_piece(&r, i, e, level);
                }
                if tapes == PieceTapes::One {
                    let chain = r.chain();
                    assert!(chain.is_whole() && !chain.is_proven(), "{chain:?}");
                    assert!(chain.links.iter().all(|l| *l == ChainLink::Unproven));
                    assert!(fr.chain().is_proven());
                }
                check_ranges(&r, &fr, level);
            }
        }
    }
}

/// A store holding `tapes`, each closed: under keys 0, 1, ..., a tape of several pieces for [PieceTapes::One].
fn store(tapes: &[Vec<u8>], whole: bool) -> MemTapes {
    let mem = MemTapes::new();
    for (k, bytes) in (0u64..).zip(tapes) {
        let mut sink = match whole {
            true => mem.create_whole(&k),
            false => mem.create(&k),
        }
        .unwrap();
        sink.write_all(bytes).unwrap();
        sink.close().unwrap();
    }
    mem
}

/// Sealing a chain of each level tape by tape, in order, gives the bytes of the chain written at Full, and the
/// content hashes Full's pieces close with.
#[test]
fn a_sealed_chain_is_the_chain_written_at_full() {
    for tapes in [PieceTapes::Each, PieceTapes::One] {
        let (full_done, full) = write_at(tapes, 0x5ea1, Integrity::Full);
        for level in [Integrity::Full, Integrity::CrcOnly, Integrity::None] {
            let (_, bytes) = write_at(tapes, 0x5ea1, level);
            assert_eq!(bytes == full, level == Integrity::Full);
            let mem = store(&bytes, tapes == PieceTapes::One);
            let mut sealed: Vec<Sealed> = vec![];
            for k in mem.keys().unwrap() {
                let s = mem.seal(&k, sealed.last()).unwrap().unwrap();
                sealed.extend(s);
            }
            for (k, f) in (0u64..).zip(&full) {
                let got = mem.get(&k).unwrap().unwrap().to_vec().unwrap();
                assert!(got == *f, "{level:?} {tapes:?} tape {k}");
            }
            if level == Integrity::Full {
                assert!(sealed.is_empty(), "a chain of Full stays as it is");
                continue;
            }
            assert_eq!(sealed.len(), full_done.len());
            for (s, d) in sealed.iter().zip(&full_done) {
                assert_eq!(
                    (s.hash, s.ident, s.next, s.was),
                    (d.hash, d.ident, d.next, level)
                );
            }
        }
    }
}

/// A piece written on after its close (a close summary, two closes, an append head), aligned or not, seals to the
/// piece written at Full from the same calls; sealed again, it stays as it is.
#[test]
fn a_piece_written_on_after_its_close_seals_to_full() {
    for alignment in [0, ww_tape::ALIGNMENT] {
        let full = crate::resume::written_on(alignment, Integrity::Full);
        for level in [Integrity::Full, Integrity::CrcOnly, Integrity::None] {
            let bytes = crate::resume::written_on(alignment, level);
            assert_eq!(bytes.len(), full.len());
            assert_eq!(bytes == full, level == Integrity::Full);
            let mem = store(&[bytes], false);
            let s = mem.seal(&0, None).unwrap().unwrap();
            let got = mem.get(&0).unwrap().unwrap().to_vec().unwrap();
            assert!(got == full, "{level:?} alignment {alignment}");
            assert_eq!(
                s.first().map(|s| s.was),
                (level != Integrity::Full).then_some(level)
            );
            mem.seal(&0, None).unwrap().unwrap();
            assert!(mem.get(&0).unwrap().unwrap().to_vec().unwrap() == full);
            let r = TapeReader::open(&full[..]).unwrap();
            assert!(r.verify(0).unwrap().holds);
        }
    }
}

/// A seal refuses a tape still being written, a piece without its tail and a damaged one, and leaves each as it
/// was; a reader holding the tape before a seal reads the old bytes on.
#[test]
fn a_seal_refuses_what_it_cannot_raise_and_leaves_readers_alone() {
    use ww_tape::{SealError, WriteError};
    let bytes = crate::resume::written_on(0, Integrity::CrcOnly);
    let mem = MemTapes::new();
    let mut open = mem.create(&0).unwrap();
    open.write_all(&bytes).unwrap();
    assert!(matches!(
        mem.seal(&0, None),
        Err(WriteError::Sink {
            error: ww_tape::MemError::Unclosed(0),
            ..
        })
    ));
    drop(open);
    let refused = |bytes: &[u8]| {
        let mem = store(&[bytes.to_vec()], false);
        let Err(WriteError::Seal(e)) = mem.seal(&0, None) else {
            panic!("refused")
        };
        assert!(mem.get(&0).unwrap().unwrap().to_vec().unwrap() == bytes);
        e
    };
    assert_eq!(refused(&bytes[..bytes.len() - 8]), SealError::NotClosed);
    // a byte of the payload of the first events chunk, whose CRCs a piece of CrcOnly holds
    let r = TapeReader::open(&bytes[..]).unwrap();
    let e = r.entries(0).unwrap();
    let chunk = e.iter().find(|e| e.kind == BlockKind::Events).unwrap();
    let mut damaged = bytes.clone();
    damaged[(chunk.offset + u64::from(chunk.len) - 40) as usize] ^= 1;
    assert_eq!(
        refused(&damaged),
        SealError::Damaged {
            offset: chunk.offset
        }
    );
    let mem = store(std::slice::from_ref(&bytes), false);
    let held = mem.get(&0).unwrap().unwrap();
    mem.seal(&0, None).unwrap().unwrap();
    assert!(held.to_vec().unwrap() == bytes);
    assert!(mem.get(&0).unwrap().unwrap().to_vec().unwrap() != bytes);
}

/// Files of a chain of each level, sealed in place file by file, hold the bytes of the chain written at Full; a
/// file whose blocks were sealed but not its head (a crash before the head's write) seals to Full too; a file a
/// writer holds or with an unclosed piece is refused and left as it is.
#[test]
fn files_seal_in_place_to_the_chain_written_at_full() {
    use ww_tape::{SealError, WriteError};
    let dir = crate::pieces::scratch("seal_in_place");
    for tapes in [PieceTapes::Each, PieceTapes::One] {
        let (_, full) = write_at(tapes, 0x5ea1, Integrity::Full);
        for level in [Integrity::CrcOnly, Integrity::None] {
            let (_, bytes) = write_at(tapes, 0x5ea1, level);
            let paths: Vec<_> = bytes
                .iter()
                .enumerate()
                .map(|(i, b)| {
                    let p = dir.join(format!("{level:?}-{tapes:?}-{i}.wwt"));
                    std::fs::write(&p, b).unwrap();
                    p
                })
                .collect();
            let mut last: Option<Sealed> = None;
            for p in &paths {
                let s = ww_tape_fs::writer::seal(p, last.as_ref()).unwrap();
                last = s.last().cloned().or(last);
            }
            for (p, f) in paths.iter().zip(&full) {
                let got = std::fs::read(p).unwrap();
                let first = got.iter().zip(f).position(|(a, b)| a != b);
                assert!(
                    got == *f,
                    "{level:?} {tapes:?} {} {} {first:?}",
                    got.len(),
                    f.len()
                );
            }
            // the first file sealed but for its head: sealed again, it is Full's
            let head = TapeReader::open(&bytes[0][..]).unwrap().entries(0).unwrap();
            let head_len = head.first().map_or(0, |e| e.offset) as usize;
            let mut crashed = full[0].clone();
            crashed[..head_len].copy_from_slice(&bytes[0][..head_len]);
            assert_eq!(
                TapeReader::open(&crashed[..]).unwrap().pieces()[0].integrity(),
                level
            );
            std::fs::write(&paths[0], &crashed).unwrap();
            ww_tape_fs::writer::seal(&paths[0], None).unwrap();
            assert!(std::fs::read(&paths[0]).unwrap() == full[0]);
        }
    }
    let (_, bytes) = write_at(PieceTapes::Each, 0x5ea1, Integrity::None);
    let path = dir.join("held.wwt");
    std::fs::write(&path, &bytes[0]).unwrap();
    let held = ww_tape_fs::FileSink::reopen(&path).unwrap().unwrap();
    assert!(matches!(
        ww_tape_fs::writer::seal(&path, None),
        Err(WriteError::Sink {
            error: ww_tape_fs::FsError::Locked(_),
            ..
        })
    ));
    drop(held);
    let cut = &bytes[0][..bytes[0].len() - 8];
    std::fs::write(&path, cut).unwrap();
    assert!(matches!(
        ww_tape_fs::writer::seal(&path, None),
        Err(WriteError::Seal(SealError::NotClosed))
    ));
    assert!(std::fs::read(&path).unwrap() == cut);
    _ = std::fs::remove_dir_all(&dir);
}

/// A piece of `level`: two chunks and a records block, closed with a summary, written on after its close with a
/// chunk and closed again.
fn golden_piece(level: Integrity) -> Vec<u8> {
    use crate::pieces::{chunk, column};
    let defs = crate::definitions::base_defs();
    let mut w = crate::resume::begin_at(0, level);
    for k in 0..2 {
        let (h, b) = chunk(0, k);
        w.write_chunk(defs, &h, &column(&b)).unwrap();
    }
    crate::reclose::write(&mut w, [7]);
    let tape = w.sink().tape();
    w.close_with_summary(ww_tape::piece::CloseReason::Closed, b"one")
        .unwrap();
    let (mut w, _) = crate::reclose::reopen(&tape.to_vec().unwrap(), 0);
    let (h, b) = chunk(0, 2);
    w.write_chunk(defs, &h, &column(&b)).unwrap();
    crate::reclose::close(w)
}

/// One golden piece per level pins the level byte, the unchecked chunks, the unhashed entries and the absent hashes
/// (`WW_TAPE_GOLDEN=update` rewrites them); sealed, each lower one is Full's.
#[test]
fn golden_levels() {
    let full = golden_piece(Integrity::Full);
    for (level, name) in [
        (Integrity::Full, "v1-level-full.hex"),
        (Integrity::CrcOnly, "v1-level-crc-only.hex"),
        (Integrity::None, "v1-level-none.hex"),
    ] {
        let bytes = golden_piece(level);
        crate::reclose::golden(&bytes, name);
        let mem = store(&[bytes], false);
        mem.seal(&0, None).unwrap().unwrap();
        assert!(mem.get(&0).unwrap().unwrap().to_vec().unwrap() == full);
    }
}
