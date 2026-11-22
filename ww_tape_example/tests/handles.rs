//! The drive's hour through mux handles, one thread per source (H5): a sanity check, not the pinned story numbers
//! ([`handles`](ww_tape_example::handles)'s own chain, written and cut on its own).
#![allow(clippy::expect_used, reason = "tests fail by panicking")]

use ww_tape_example::Drive;
use ww_tape_example::handles::record;

#[test]
fn one_thread_per_source_writes_a_chain() {
    let drive = Drive::scaled(1000)
        .expect("1000 divides the rates")
        .minutes(5);
    let via = record(&drive).expect("the hour records through handles");
    assert!(via.pieces > 0, "at least one piece");
    assert!(via.bytes > 0, "at least one byte");
    assert_eq!(
        via.frames,
        u64::from(drive.can_hz) * 60 * u64::from(drive.minutes),
        "every can0 frame of the run, sent by its own thread"
    );
    assert_eq!(
        via.samples,
        u64::from(drive.phase_hz) * 60 * u64::from(drive.minutes),
        "every phase_a sample of the run, sent by its own thread"
    );
    assert!(via.puts >= 1, "at least the mode's first put");
    assert!(
        via.records > 0,
        "log lines and temperatures, sent by their own thread"
    );
}
