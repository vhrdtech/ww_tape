//! The program of the book's Part IV finds the fault at minute 52 in the drive's full hour and prints what the
//! chapters quote (`fault.out`): the pieces, the blocks a query reads, the mode, the frames, the current, the
//! overview from summaries, the checks, and the bytes each step reads.
#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use std::path::Path;

use ww_tape_example::fault::{BEFORE_US, Fault, fault_us};
use ww_tape_example::{Drive, EMERGENCY_ID, LIMIT_MA, Mode};

#[test]
fn part_iv_finds_the_fault_and_prints_what_the_chapters_quote() {
    let recording = Drive::full().record().expect("the hour records");
    let f = Fault::find(&recording.pieces).expect("the fault is found");
    let want =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fault.out"))
            .expect("tests/fault.out");
    assert_eq!(
        f.to_string(),
        want,
        "tests/fault.out is what the program prints from `Finding the fault` on: copy it from `cargo run -p ww_tape_example`"
    );

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
    assert!(f.same.same && f.same.stated == f.same.chunks);
}
