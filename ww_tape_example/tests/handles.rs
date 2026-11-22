//! The drive's hour through mux handles, one thread per source, holds what the story's recording holds: each
//! track's decoded items are the same, though the bytes differ with the order the threads' units reach the queue.
#![allow(clippy::expect_used, reason = "tests fail by panicking")]

use ww_tape_example::Drive;
use ww_tape_example::handles::{decode, record};

#[test]
fn one_thread_per_source_writes_the_items_of_the_story() {
    // 20 minutes of the hour at 1/1000 of its data: two pieces, the mode changes at minute 1
    let drive = Drive::scaled(1000)
        .expect("1000 divides the rates")
        .minutes(20);
    let direct = drive.record().expect("the run records");
    let via = record(&drive).expect("the run records through handles");
    assert_eq!(direct.pieces.len(), 2, "the run cuts a piece at minute 15");
    let want = decode(&direct.pieces, &direct.numbers).expect("the story's pieces decode");
    let got = decode(&via.pieces, &direct.numbers).expect("the handles' pieces decode");
    assert_eq!(
        u64::try_from(got.frames.len()).expect("fits"),
        u64::from(drive.can_hz) * 60 * u64::from(drive.minutes),
        "every frame of can0"
    );
    assert_eq!(
        u64::try_from(got.samples.len()).expect("fits"),
        u64::from(drive.phase_hz) * 60 * u64::from(drive.minutes),
        "every sample of phase_a"
    );
    assert!(got.mode.len() >= 2, "the mode changed");
    assert!(!got.log.is_empty() && !got.temp.is_empty());
    assert_eq!(got.log, want.log, "log lines");
    assert_eq!(got.temp, want.temp, "temperatures");
    assert_eq!(got.mode, want.mode, "the mode's changes");
    assert_eq!(got.frames, want.frames, "frames of can0");
    assert_eq!(got.samples, want.samples, "samples of phase_a");
}
