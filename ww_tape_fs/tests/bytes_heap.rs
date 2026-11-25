#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    reason = "tests fail by panicking and compute expected values freely"
)]
//! A value of 2 GiB on a bytes track written streamed to a file and read back whole with its hash checked: the heap
//! and the resident set stay under 32 MiB, as the writer and the reader hold one part at a time.

use shrink_wrap::prelude::RefVec;
use ww_tape::bytes_track::{BytesOptions, BytesWriter, ValueId};
use ww_tape::{
    ClockOrigin, ClockSpec, Definitions, ItemNum, Items, Magnitude, Segment, SyncPolicy,
    TapeReader, TrackKind, TrackRepeat, TrackSpec, clock_key, track_key,
};
use ww_tape_fs::FileSource;
use ww_tape_testalloc::{Counting, thread_heap_of};

#[global_allocator]
static ALLOC: Counting = Counting::new();

/// Bytes of the value.
const VALUE_LEN: u64 = 2 << 30;
/// Most bytes of heap and of resident set the write and the read may take.
const MEMORY_MAX: usize = 32 << 20;

/// The value's bytes, made as they are read: each 8 bytes the little-endian number of their place.
struct Pattern {
    at: u64,
}

impl std::io::Read for Pattern {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let left = VALUE_LEN - self.at;
        let n = (buf.len() as u64).min(left) as usize & !7;
        for (k, word) in buf[..n].as_chunks_mut::<8>().0.iter_mut().enumerate() {
            *word = ((self.at >> 3) + k as u64).to_le_bytes();
        }
        self.at += n as u64;
        Ok(n)
    }
}

/// The resident set's high-water mark of this process, from `/proc/self/status`.
#[cfg(target_os = "linux")]
fn rss_peak_bytes() -> usize {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let line = status.lines().find(|l| l.starts_with("VmHWM:")).unwrap();
    let kib: usize = line.split_whitespace().nth(1).unwrap().parse().unwrap();
    kib << 10
}

#[test]
fn a_2_gib_value_is_written_and_read_in_32_mib() {
    let mut defs = Definitions::new();
    let clock = defs
        .define_clock(ClockSpec {
            key: clock_key(0xb1, &[], 1),
            rate: Magnitude::pow10(9).into(),
            resolution: Magnitude::pow10(-9).into(),
            origin: ClockOrigin::Boot,
            label: "host",
        })
        .unwrap();
    let track = defs
        .define_track(TrackSpec {
            key: track_key(0xb1, &[Segment::Name("blobs")]),
            clock,
            dimensions: RefVec::Slice { slice: &[] },
            items: Items::RawBytes,
            label: "blobs",
            repeat: TrackRepeat::Stream,
            kind: TrackKind::Bytes,
            relations: RefVec::Slice { slice: &[] },
            defined_twice: false,
            owner: [0; 32],
        })
        .unwrap();
    let id = defs.track_identity(track).unwrap().0;
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("bytes_heap");
    _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("value.wwt");

    let started = std::time::Instant::now();
    let (seal, write_heap) = thread_heap_of(|| {
        let mut w = ww_tape_fs::writer::create(&path, SyncPolicy::None).unwrap();
        let mut values =
            BytesWriter::new(&defs, track, ItemNum::new(0), BytesOptions::default()).unwrap();
        let seal = values
            .write_from(1, Pattern { at: 0 }, |p| {
                w.write_chunk(&defs, &p.head, p.columns()).map(drop)
            })
            .unwrap();
        w.close().unwrap();
        seal
    });
    assert_eq!(seal.id, ValueId(ItemNum::new(0)));
    assert_eq!(seal.len, VALUE_LEN);
    assert_eq!(seal.parts, VALUE_LEN / (4 << 20) + 1);

    let written = started.elapsed();
    let (read, read_heap) = thread_heap_of(|| {
        let r = TapeReader::open(FileSource::open(&path).unwrap()).unwrap();
        // every part but the last is a whole number of words: each word is its place
        let mut at = 0u64;
        let read = r
            .read_value(id, seal.id, |b| {
                for word in b.as_chunks::<8>().0 {
                    assert_eq!(*word, (at >> 3).to_le_bytes(), "the word at {at}");
                    at += 8;
                }
                Ok::<(), ()>(())
            })
            .unwrap();
        assert_eq!(at, VALUE_LEN, "the bytes handed out");
        read
    });
    assert_eq!(read, seal);
    std::fs::remove_dir_all(&dir).unwrap();

    assert!(
        write_heap.peak_bytes < MEMORY_MAX,
        "the write's heap peaked at {} bytes",
        write_heap.peak_bytes
    );
    assert!(
        read_heap.peak_bytes < MEMORY_MAX,
        "the read's heap peaked at {} bytes",
        read_heap.peak_bytes
    );
    #[cfg(target_os = "linux")]
    assert!(
        rss_peak_bytes() < MEMORY_MAX,
        "the resident set peaked at {} bytes",
        rss_peak_bytes()
    );
    println!(
        "heap peak: write {} bytes, read {} bytes; rss peak {}; write {written:?}, all {:?}",
        write_heap.peak_bytes,
        read_heap.peak_bytes,
        rss_peak_bytes(),
        started.elapsed()
    );
}
