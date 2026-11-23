//! Pieces of each integrity level ([Integrity]) against the same calls at [Integrity::Full]: the same blocks in the
//! same places, the hashes and (at None) the payload CRCs absent and marked so, and every reader saying what it
//! could not check.

use ww_tape::index::IndexEntry;
use ww_tape::{
    BlockKind, Checked, Integrity, ItemNum, OwnerId, PieceTapes, TapeReader, TrackHash,
    piece::ChainLink,
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
