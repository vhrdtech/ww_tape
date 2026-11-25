//! The hostile builders: the same input gives the same bytes, and the CRCs are good often enough for the decoders'
//! deeper paths to run.

use ww_tape::{Block, block_len};
use ww_tape_fuzz::hostile;

/// A hostile builder: input to block or file.
type Builder = fn(&[u8]) -> Vec<u8>;

const BUILDERS: [(&str, Builder); 9] = [
    ("records", hostile::records),
    ("chunk", hostile::chunk),
    ("times", hostile::times),
    ("payloads", hostile::payloads),
    ("samples", hostile::samples),
    ("series", hostile::series),
    ("summary", hostile::summary),
    ("index", hostile::index),
    ("file", hostile::file),
];

/// Inputs from a fixed xorshift stream.
fn inputs(n: usize) -> Vec<Vec<u8>> {
    let mut x = 0x2545_f491_4f6c_dd1d_u64;
    (0..n)
        .map(|i| {
            (0..64 + i % 32)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    x as u8
                })
                .collect()
        })
        .collect()
}

#[test]
fn the_same_input_gives_the_same_bytes() {
    for (name, build) in BUILDERS {
        for input in inputs(200) {
            assert_eq!(build(&input), build(&input), "{name}");
        }
    }
}

#[test]
fn every_builder_takes_any_input() {
    for (_, build) in BUILDERS {
        for input in [&[][..], &[0xff; 3], &[0; 200]] {
            build(input);
        }
    }
}

/// Of 1000 blocks, most carry a good block CRC and some read whole: hostile values reach the decoders behind the CRCs.
#[test]
fn most_blocks_pass_their_crcs() {
    for (name, build) in BUILDERS {
        let (mut crc_good, mut good) = (0, 0);
        for input in inputs(1000) {
            let bytes = build(&input);
            let first = block_len(&bytes).ok().and_then(|n| bytes.get(..n));
            let Some(first) = first else { continue };
            let crc = crc::Crc::<u32>::new(&crc::CRC_32_ISCSI);
            let (covered, stored) = first.split_at(first.len() - 4);
            if crc.checksum(covered).to_le_bytes() == stored {
                crc_good += 1;
            }
            if Block::read(first).is_ok() {
                good += 1;
            }
        }
        assert!(crc_good > 700, "{name}: {crc_good} of 1000 with a good CRC");
        assert!(good > 50, "{name}: {good} of 1000 read whole");
    }
}

/// Every block a builder makes is at most 1 KiB, so a million of them decode in seconds.
#[test]
fn blocks_stay_small() {
    for (name, build) in BUILDERS {
        for input in inputs(2000) {
            let bytes = build(&input);
            let mut rest = &bytes[..];
            while let Ok(n) = block_len(rest) {
                let Some((_, after)) = rest.split_at_checked(n) else {
                    break;
                };
                assert!(n <= 1024, "{name}: a block of {n} bytes");
                rest = after;
            }
        }
    }
}

/// Of 1000 hostile summary blocks, some name a grid tile and read whole, and some carry FLAG_TILE and fail: the tile
/// head's reader runs on good and on hostile bytes.
#[test]
fn summaries_reach_the_tile_head() {
    let (mut tiles, mut refused) = (0, 0);
    for input in inputs(1000) {
        let bytes = hostile::summary(&input);
        let flagged = bytes
            .get(6..8)
            .is_some_and(|f| u16::from_le_bytes([f[0], f[1]]) & ww_tape::FLAG_TILE != 0);
        match Block::read(&bytes).ok().and_then(|b| b.summary_head()) {
            Some(h) if !h.tile.is_empty() => tiles += 1,
            None if flagged => refused += 1,
            _ => {}
        }
    }
    assert!(tiles > 50, "{tiles} of 1000 tile heads read whole");
    assert!(refused > 5, "{refused} of 1000 tile heads refused");
}

/// Of the hostile grid files whose grid definition decodes, some hold tile summary blocks the box of the whole grid
/// finds and merges into a region's timeline, by their entries' tile numbers and by their heads.
#[test]
fn grid_files_reach_the_tile_summaries() {
    let (mut defined, mut found, mut by_number, mut by_head, mut merged) = (0, 0, 0, 0, 0);
    for input in inputs(2000) {
        let r = ww_tape_fuzz::targets::grid_reach(&hostile::grid(&input));
        defined += usize::from(r.defined);
        found += usize::from(r.found > 0);
        by_number += usize::from(r.found > r.scanned);
        by_head += usize::from(r.scanned > 0);
        merged += usize::from(r.nodes > 0);
    }
    assert!(defined > 200, "{defined} of 2000 grids decode");
    assert!(found * 8 > defined, "{found} of {defined} find summaries");
    assert!(merged * 8 > defined, "{merged} of {defined} merge nodes");
    assert!(by_number > 10, "{by_number} found by tile number");
    assert!(by_head > 10, "{by_head} found by head");
}

/// Of 20000 hostile grid files, some hold overview chunks the box of the whole grid at their level finds and whose
/// min, max and mean columns read.
#[test]
fn grid_files_reach_the_overview_columns() {
    let overviews = inputs(20_000)
        .iter()
        .filter(|input| ww_tape_fuzz::targets::grid_reach(&hostile::grid(input)).overviews > 0)
        .count();
    assert!(overviews > 15, "{overviews} of 20000 read overview chunks");
}

/// The bytes builder's template reads each value as written and its torn value as torn; of 2000 hostile chains, some
/// values read whole, and some are absent, damaged or fail their hash behind good CRCs.
#[test]
fn bytes_chains_reach_every_outcome() {
    use ww_tape::bytes_track::{Absence, ValueError, read_value};
    let t = hostile::bytes_template();
    let r = ww_tape::TapeReader::open(&t.bytes[..]).unwrap();
    assert_eq!(
        r.pieces().len(),
        4,
        "16 parts, a cut every 5: values cross cuts"
    );
    let read = |r: &ww_tape::TapeReader<&[u8]>, id| {
        let mut out = Vec::new();
        read_value::<_, core::convert::Infallible>(&[r], t.track, id, |b| {
            out.extend_from_slice(b);
            Ok(())
        })
        .map(|_| out)
    };
    for (id, want) in &t.values {
        match (read(&r, *id), want) {
            (Ok(got), Some(v)) => assert_eq!(&got, v),
            (Err(ValueError::Absent { why, .. }), None) => {
                assert!(matches!(why, Absence::Torn { parts: 2 }), "{why:?}");
            }
            (got, _) => panic!("value {id:?}: {got:?}"),
        }
    }
    let mut seen = [0usize; 4];
    let mut x = 0x6279_7465_7300_0002_u64;
    for _ in 0..2000 {
        let input: Vec<u8> = (0..64)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        let f = hostile::bytes(&input);
        let Ok(r) = ww_tape::TapeReader::open(&f[..]) else {
            continue;
        };
        for (id, _) in &t.values {
            let k = match read(&r, *id) {
                Ok(_) => 0,
                Err(ValueError::Absent { .. }) => 1,
                Err(ValueError::Damaged { .. }) => 2,
                Err(ValueError::Mismatch { .. }) => 3,
                Err(_) => continue,
            };
            seen[k] += 1;
        }
    }
    assert!(
        seen.iter().all(|n| *n > 0),
        "read, absent, damaged, mismatch: {seen:?}"
    );
}
