//! The store model ([ww_tape_fuzz::stores]) on memory tapes, over random operations in both chain shapes.

use std::path::{Path, PathBuf};

use ww_tape::MemTapes;
use ww_tape_fs::Files;
use ww_tape_fuzz::stores::{ModelStore, OPS_MAX, run};

/// xorshift64: the same inputs on every run.
pub(crate) fn input(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// A tape per file in a scratch directory.
struct FileStore(PathBuf);

impl FileStore {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("ww_tape_fuzz_stores_{name}_{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self, key: u64) -> PathBuf {
        Files::new(&self.0).path_of(key)
    }
}

impl Drop for FileStore {
    fn drop(&mut self) {
        _ = std::fs::remove_dir_all(&self.0);
    }
}

impl ModelStore for FileStore {
    type Tapes = Files;

    fn tapes(&self) -> Files {
        Files::new(&self.0)
    }

    fn keys(&self) -> Vec<u64> {
        (0..=OPS_MAX as u64)
            .filter(|&k| Path::exists(&self.path(k)))
            .collect()
    }

    fn bytes(&self, key: u64) -> Option<(u64, Vec<u8>)> {
        std::fs::read(self.path(key)).ok().map(|b| (0, b))
    }

    fn remove(&mut self, key: u64) -> bool {
        std::fs::remove_file(self.path(key)).is_ok()
    }

    fn drop_front(&mut self, _key: u64, _to: u64) -> Option<u64> {
        None
    }
}

/// Files and memory tapes run the same operations to the same pieces, but where memory drops a tape's front.
#[test]
fn files_keep_the_model_as_memory_does() {
    for seed in 0..24u64 {
        let mut data = input(0x000f_11e5 + seed, 80);
        data[0] = seed as u8 & 3;
        let files = run(FileStore::new(&seed.to_string()), &data);
        let mem = run(MemTapes::new(), &data);
        if data[0] & 1 == 0 || mem.gone == 0 {
            assert_eq!(files, mem, "seed {seed}");
        }
    }
}

#[test]
fn memory_tapes_keep_the_model() {
    // runs per shape (one tape, a tape per piece) that removed or dropped pieces
    let mut cut = [0; 2];
    for seed in 0..64u64 {
        let mut data = input(0x0057_07e5 + seed, 120);
        data[0] = seed as u8 & 3;
        let got = run(MemTapes::new(), &data);
        cut[usize::from(data[0] & 1)] += usize::from(got.gone > 0 && !got.pieces.is_empty());
    }
    assert!(
        cut.iter().all(|&n| n >= 8),
        "pieces removed and kept: {cut:?}"
    );
}
