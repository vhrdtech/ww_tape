//! The RF capture against the numbers the book gives: the same bytes every run, every sample read back, and the size
//! of a second at the full 20 MS/s in each codec.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::expect_used,
    reason = "tests compute expected values freely"
)]

use ww_tape::Codec;
use ww_tape_example::rf::{Rf, Tally, read};

#[test]
fn a_scaled_capture_is_the_same_every_run_and_reads_back() {
    let rf = Rf::scaled(100).expect("100 divides the rate").seconds(10);
    let piece = rf.record().expect("the capture records");
    assert_eq!(
        piece.bytes,
        rf.record().expect("again").bytes,
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
        let c = second
            .codec(codec)
            .record_on(Tally::default())
            .expect("the second records");
        got.push((c.len, format!("{:.3}", c.bytes_per_sample())));
        assert_eq!(
            (c.summary_bytes, c.summary_blocks),
            (900_250, 655),
            "the summaries do not depend on the codec"
        );
    }
    // the bytes per sample `cargo run --release -p ww_tape_example --bin rf` prints for a minute
    assert_eq!(
        got,
        [
            (81_100_800, "4.055".to_owned()),
            (61_100_032, "3.055".to_owned()),
            (57_974_784, "2.899".to_owned()),
        ]
    );
}
