//! The program of the book's Part IV finds the fault at minute 52 in the drive's full hour and prints what the
//! chapters quote (`docs/generated/fault.txt`): the pieces, the blocks a query reads, the mode, the frames, the current, the
//! overview from summaries, the checks, and the bytes each step reads.
#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use std::path::Path;

use example_capture::about;
use example_drive::fault::{BEFORE_US, Fault, fault_us};
use example_drive::{Drive, EMERGENCY_ID, LIMIT_MA, Mode, grouped};

#[test]
fn book_output_part_iv_finds_the_fault_and_prints_what_the_chapters_quote() {
    let recording = Drive::full().record().expect("the hour records");
    let f = Fault::find(&recording.pieces).expect("the fault is found");
    ww_tape_book_outputs::check("fault.txt", &f.to_string());

    rounded_on_the_pages(&f);
    assert_eq!(f.opened.piece, 3, "the fault is in the last of four pieces");
    let (before, at) = (&f.modes[0], &f.modes[1]);
    assert_eq!(
        f.modes.len(),
        2,
        "a mode for the second before the fault and the second after it"
    );
    assert_eq!(
        (before.item.as_slice(), at.item.as_slice()),
        (Mode::Run.item(), Mode::Fault.item())
    );
    assert!(before.whole && at.whole, "nothing newer can hide");
    assert!(
        f.emergency_first(),
        "the first frame after the fault is the emergency frame"
    );
    assert_eq!(f.frames.frames[0].id, EMERGENCY_ID);
    assert!(f.frames.frames[0].us >= fault_us());
    assert_eq!(
        f.current.samples.len(),
        usize::try_from(BEFORE_US / 40).expect("in usize")
    );
    assert!(
        i32::from(f.current.peak.1.unsigned_abs()) > LIMIT_MA,
        "the current passes the limit before the fault"
    );
    let zoom = f.overview.zoom..f.overview.zoom + 1024;
    assert!(
        zoom.contains(&f.current.peak.0),
        "the zoom lands on the node of the peak"
    );
    assert!(f.checked.whole && f.checked.verified.iter().all(|&v| v));
    assert_eq!(
        f.check_read.bytes - f.len,
        f.verify_cost.extra(),
        "verify reads the chain once, a header per block, the final indexes twice, and not the closes and tails"
    );
    assert!(f.same.same && f.same.stated == f.same.chunks);
}

/// A page of the book with its line breaks and indents as single spaces, so that a phrase matches across lines.
fn page(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs")
        .join(name);
    let text = std::fs::read_to_string(&path).expect("a book page");
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The pages quote the sizes the program prints, rounded where their precision doesn't matter (docs/writers.md,
/// "Numbers"): each phrase holds `about` of the exact value.
fn rounded_on_the_pages(f: &Fault) {
    let (index, reading) = (page("index.md"), page("reading.md"));
    let (events, series) = (page("events.md"), page("series.md"));
    let (summaries, copies) = (page("summaries.md"), page("copies.md"));
    let (o, v) = (&f.overview, &f.verify_cost);
    for (name, text, phrase) in [
        (
            "index.md",
            &index,
            format!("the example's {} recording", about(f.len)),
        ),
        (
            "index.md",
            &index,
            format!("reads about {}", about(f.open_read.bytes)),
        ),
        (
            "index.md",
            &index,
            format!("then reads about {}", about(f.find_read.bytes)),
        ),
        (
            "reading.md",
            &reading,
            format!("about {} in all", about(f.len)),
        ),
        (
            "reading.md",
            &reading,
            format!(
                "opening reads about {} in {} reads",
                about(f.open_read.bytes),
                f.open_read.reads
            ),
        ),
        (
            "reading.md",
            &reading,
            format!(
                "It reads about {} in {} reads",
                about(f.find_read.bytes),
                f.find_read.reads
            ),
        ),
        (
            "reading.md",
            &reading,
            format!("lies about {} behind", about(f.modes[0].behind)),
        ),
        (
            "reading.md",
            &reading,
            format!("lies about {} behind", about(f.modes[1].behind)),
        ),
        (
            "reading.md",
            &reading,
            format!(
                "read about {} in {} reads",
                about(f.mode_read.bytes),
                f.mode_read.reads
            ),
        ),
        (
            "events.md",
            &events,
            format!("a block of about {}", about(u64::from(f.frames.block_len))),
        ),
        (
            "events.md",
            &events,
            format!(
                "The step reads about {} in {} reads",
                about(f.frames_read.bytes),
                f.frames_read.reads
            ),
        ),
        (
            "series.md",
            &series,
            format!("a block of about {}", about(u64::from(f.current.block_len))),
        ),
        (
            "series.md",
            &series,
            format!(
                "The step reads about {} in {} reads",
                about(f.current_read.bytes),
                f.current_read.reads
            ),
        ),
        (
            "summaries.md",
            &summaries,
            format!("chunks]] hold about {}", about(o.sample_bytes)),
        ),
        (
            "summaries.md",
            &summaries,
            format!(
                "about {} in {} reads, and no sample",
                about(o.read.bytes),
                o.read.reads
            ),
        ),
        (
            "summaries.md",
            &summaries,
            format!(
                "take about {} in {} reads",
                about(o.index_read.bytes),
                o.index_read.reads
            ),
        ),
        (
            "summaries.md",
            &summaries,
            format!(
                "The zoom reads about {} in {} reads",
                about(o.zoom_read.bytes),
                o.zoom_read.reads
            ),
        ),
        (
            "copies.md",
            &copies,
            format!(
                "it reads about {} in {} reads",
                about(f.check_read.bytes),
                grouped(f.check_read.reads)
            ),
        ),
        (
            "copies.md",
            &copies,
            format!(
                "about {} more than the recording's {}",
                about(v.extra()),
                about(f.len)
            ),
        ),
        (
            "copies.md",
            &copies,
            format!("{} headers of 32 bytes", grouped(v.blocks)),
        ),
        (
            "copies.md",
            &copies,
            format!("final index]] whole | about {} |", about(v.index)),
        ),
        (
            "copies.md",
            &copies,
            format!("as it replays the blocks | about {} |", about(v.entries)),
        ),
        (
            "copies.md",
            &copies,
            format!("closes and tails | none of their {} |", about(v.unread)),
        ),
        (
            "copies.md",
            &copies,
            format!(
                "reads about {} in {} reads",
                about(f.same_read.bytes),
                f.same_read.reads
            ),
        ),
    ] {
        assert!(text.contains(&phrase), "docs/{name} says {phrase:?}");
    }
}
