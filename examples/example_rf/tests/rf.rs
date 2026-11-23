//! The RF capture against the numbers the book gives: the same bytes every run, every sample read back, and the size
//! of a second at the full 20 MS/s in each codec.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap,
    clippy::expect_used,
    clippy::panic,
    reason = "tests fail by panicking and compute expected values freely"
)]

use example_capture::about;
use example_capture::rf::{BURST_MS, BURSTS_MS, Rf, iq_key};
use example_rf::chains::{Chains, Decoded, WINDOW_SAMPLES};
use example_rf::{Tally, read, record, record_on};
use ww_tape::Codec;
use ww_tape_fs::follow::{ChainFollower, FollowOptions, TrackItem};

#[test]
fn a_scaled_capture_is_the_same_every_run_and_reads_back() {
    let rf = Rf::scaled(100).expect("100 divides the rate").seconds(10);
    let piece = record(&rf).expect("the capture records");
    assert_eq!(
        piece.bytes,
        record(&rf).expect("again").bytes,
        "the same settings give the same bytes"
    );
    let back = read(&piece.bytes).expect("the piece reads");
    assert_eq!(back.chunks, rf.chunks(), "one chunk per 10 ms");
    assert_eq!(back.samples, rf.samples(), "every sample comes back");
}

#[test]
fn a_second_at_full_rate_is_the_size_the_book_gives() {
    let second = Rf::capture().seconds(1);
    assert_eq!(second.count(), 20_000_000, "20 MS/s");
    let mut got = Vec::new();
    for codec in [Codec::RAW, Codec::PACKED12_LSB, Codec::DELTA] {
        let c = record_on(&second.codec(codec), Tally::default()).expect("the second records");
        got.push((c.len, format!("{:.3}", c.bytes_per_sample())));
        assert_eq!(
            (c.summary_bytes, c.summary_blocks),
            (900_250, 655),
            "the summaries do not depend on the codec"
        );
    }
    // the bytes per sample `cargo run --release -p example_rf` prints for a minute
    assert_eq!(
        got,
        [
            (81_104_896, "4.055".to_owned()),
            (61_104_128, "3.055".to_owned()),
            (57_974_784, "2.899".to_owned()),
        ]
    );
}

/// The text of the book page `name`, its line breaks as spaces.
fn page(name: &str) -> String {
    let path = format!("{}/../../docs/{name}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).expect("the page is there");
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn the_pages_give_the_size_of_a_minute() {
    let second =
        record_on(&Rf::capture().seconds(1), Tally::default()).expect("the second records");
    // a minute is sixty such seconds to within its one piece head, index and close, far below the rounding; the bin
    // `rf` prints the minute itself, 3666.0 MB
    let minute = about(second.len * 60);
    assert_eq!(minute, "3.7 GB");
    for (name, phrase) in [
        (
            "index.md",
            format!("A minute of the radio capture is one piece of {minute}."),
        ),
        (
            "index.md",
            "a radio capture of 20 million samples a second".to_owned(),
        ),
        (
            "reading.md",
            format!("{minute} for a minute of a radio capture"),
        ),
        (
            "series.md",
            format!("a minute of the capture is a piece of {minute}"),
        ),
        (
            "series.md",
            "A sample then takes 3 bytes instead of 4".to_owned(),
        ),
    ] {
        assert!(page(name).contains(&phrase), "docs/{name} says: {phrase}");
    }
}

/// A scratch directory of its own for test `name`, empty.
fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("example_rf_{name}_{}", std::process::id()));
    drop(std::fs::remove_dir_all(&dir));
    dir
}

/// The minute at 1/1000 of the rate in two chains, followed while written: the decoder finds every burst and
/// second, the reader gets every chunk and every find in time order, each burst right after the chunk it starts
/// in, and the chain `iq` holds every sample.
#[test]
fn two_chains_hold_the_samples_and_the_finds_and_merge_by_time() {
    let rf = Rf::scaled(1000).expect("1000 divides the rate");
    let dir = scratch("two_chains");
    let w = Chains::new(rf)
        .write_followed(&dir)
        .expect("the chains are written and followed");
    let (written, followed) = (&w.written, &w.followed);
    assert_eq!(
        followed.decoded, written.decoded,
        "the reader reads every find, in the decoder's order"
    );
    assert_eq!(
        (followed.chunks, followed.samples),
        (rf.chunks(), rf.count())
    );
    assert_eq!(followed.late, 0, "the merge waits for both chains");

    let period = 1_000_000_000 / i64::from(rf.rate_hz);
    let window = WINDOW_SAMPLES as i64 * period;
    let powers: Vec<i64> = written
        .decoded
        .iter()
        .filter_map(|d| match d {
            Decoded::Power { at, .. } => Some(*at),
            Decoded::Burst { .. } => None,
        })
        .collect();
    assert_eq!(
        powers,
        (1..=60).map(|s| s * 1_000_000_000).collect::<Vec<_>>(),
        "a power each second"
    );
    assert_eq!(followed.bursts.len(), BURSTS_MS.len());
    for ((d, first), ms) in followed.bursts.iter().zip(BURSTS_MS) {
        let Decoded::Burst { start, end, .. } = *d else {
            panic!("{d:?}")
        };
        let at = i64::from(ms) * 1_000_000;
        assert!(
            (start - at).abs() < window,
            "the burst at {ms} ms starts at {start}"
        );
        assert!(
            (end - start - i64::from(BURST_MS) * 1_000_000).abs() <= window,
            "it lasts {BURST_MS} ms"
        );
        let first_tick = *first as i64 * period;
        let chunk_ticks = i64::from(rf.chunk_samples) * period;
        assert!(
            first_tick <= start && start < first_tick + chunk_ticks,
            "the chunk before holds its start"
        );
    }

    let mut iq = ChainFollower::new(&Chains::iq_dir(&dir), FollowOptions::default())
        .expect("the chain is there")
        .track(iq_key().id());
    let mut samples = Vec::new();
    while let Some(item) = iq.try_next().expect("the chain reads") {
        let TrackItem::Samples(s) = item else {
            panic!("{item:?}")
        };
        samples.extend(s.decode::<i16>().expect("the samples decode"));
    }
    assert!(samples == rf.samples(), "every sample comes back");
    std::fs::remove_dir_all(&dir).expect("the scratch directory goes");
}

/// The numbers the RF page gives for the two chains: the minute at 1/100 of the rate, as `rf chains` writes it.
#[test]
fn the_page_gives_the_two_chains_numbers() {
    let rf = Rf::scaled(100).expect("100 divides the rate");
    let dir = scratch("two_chains_page");
    let w = Chains::new(rf)
        .write_followed(&dir)
        .expect("the chains are written and followed");
    std::fs::remove_dir_all(&dir).expect("the scratch directory goes");
    let (written, followed) = (&w.written, &w.followed);
    // the exact numbers `rf chains DIR` prints
    assert_eq!(
        (
            written.iq_pieces,
            written.iq_bytes,
            written.decoded_pieces,
            written.decoded_bytes
        ),
        (60, 38_338_560, 1, 20_480)
    );
    assert_eq!(
        (followed.chunks, followed.decoded.len(), followed.late),
        (6000, 65, 0)
    );
    let (first, chunk) = followed.bursts.first().expect("a burst");
    assert!(
        matches!(
            first,
            Decoded::Burst {
                start: 7_300_000_000,
                ..
            }
        ),
        "{first:?}"
    );
    assert_eq!(*chunk, 1_460_000);
    for phrase in [
        format!(
            "A minute at 1/100 of the rate then holds {} of samples and five bursts.",
            about(written.iq_bytes)
        ),
        format!(
            "The chain `iq` holds {} pieces of {} together, a piece per second, and `decoded` one piece of {}.",
            written.iq_pieces,
            about(written.iq_bytes),
            about(written.decoded_bytes)
        ),
        format!(
            "The reader merges {} chunks and {} finds, none of them late.",
            followed.chunks,
            followed.decoded.len()
        ),
        "The burst at 7.3 s comes after the chunk from sample 1 460 000.".to_owned(),
    ] {
        assert!(
            page("series.md").contains(&phrase),
            "docs/series.md says: {phrase}"
        );
    }
}
