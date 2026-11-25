//! [ww_tape_fs::check]: verifying and range-hashing many pieces at once gives the same results one piece at a time
//! and with the `parallel` feature, pieces spread over rayon (FS-2).

use std::path::{Path, PathBuf};

use ww_tape::index::IndexPolicy;
use ww_tape::{ChainOptions, CutPolicy, ItemNum, OwnerId, PieceTapes, SyncPolicy, WriterOptions};
use ww_tape_fs::check::{
    range_hash_each, range_hash_each_parallel, verify_each, verify_each_parallel,
};

use crate::definitions::base_defs;
use crate::pieces::{chunk, chunk_id, column, scratch};

/// A chain of 60 chunks on tracks 0 to 2 in piece files at `dir`, cut every 7 chunks.
fn write(dir: &Path) -> Vec<PathBuf> {
    let options = ChainOptions {
        tapes: PieceTapes::Each,
        writer: WriterOptions {
            sync: SyncPolicy::None,
            index: IndexPolicy {
                bytes: exact_units::Bytes::new(2048),
                interval: None,
                ..IndexPolicy::default()
            },
            producer: "check".to_owned(),
            ..WriterOptions::default()
        },
        cut: CutPolicy {
            chunks: exact_units::Count::new(7),
            ..CutPolicy::default()
        },
    };
    let mut c = ww_tape_fs::chain::create(dir, OwnerId([4; 32]), 0, &[], options).unwrap();
    let defs = base_defs();
    let mut next = [0u64; 3];
    for n in 0..60 {
        let t = n % 3;
        let (h, b) = chunk(t, next[t as usize]);
        next[t as usize] += 1;
        c.write_chunk(defs, &h, &column(&b)).unwrap();
    }
    let done = c.close().unwrap();
    done.iter()
        .map(|p| dir.join(ww_tape_fs::piece_name(p.tape)))
        .collect()
}

#[test]
fn verify_each_parallel_holds_the_same_as_one_piece_at_a_time() {
    let dir = scratch("check_verify");
    let paths = write(&dir);
    // --8<-- [start:verify_parallel]
    let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();

    let seq: Vec<_> = verify_each(&refs).into_iter().map(|r| r.unwrap()).collect();
    let par: Vec<_> = verify_each_parallel(&refs)
        .into_iter()
        .map(|r| r.unwrap())
        .collect();
    // --8<-- [end:verify_parallel]

    assert!(seq.iter().all(|v| v.holds), "every piece verify() holds");
    assert_eq!(
        seq, par,
        "parallel gives the same Verification as sequential"
    );
}

#[test]
fn range_hash_each_parallel_holds_the_same_as_one_piece_at_a_time() {
    let dir = scratch("check_range_hash");
    let paths = write(&dir);
    let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
    let items = ItemNum::new(0)..ItemNum::new(u64::MAX);
    let track_id = chunk_id(0);

    let seq: Vec<_> = range_hash_each(&refs, track_id, OwnerId::NONE, items.clone())
        .into_iter()
        .map(|r| r.unwrap())
        .collect();
    let par: Vec<_> = range_hash_each_parallel(&refs, track_id, OwnerId::NONE, items)
        .into_iter()
        .map(|r| r.unwrap())
        .collect();

    assert!(
        seq.iter().any(|h| h.chunks > 0),
        "track 0 has chunks somewhere in the chain"
    );
    assert_eq!(seq, par, "parallel gives the same RangeHash as sequential");
}
