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
use ww_tape_example::about;
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

/// The text of the book page `name`, its line breaks as spaces.
fn page(name: &str) -> String {
    let path = format!("{}/../docs/{name}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).expect("the page is there");
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn the_pages_give_the_size_of_a_minute() {
    let second = Rf::capture()
        .seconds(1)
        .record_on(Tally::default())
        .expect("the second records");
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
