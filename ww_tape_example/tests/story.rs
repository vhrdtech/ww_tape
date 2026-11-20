//! The drive's recording against the story the book tells: the same bytes every run, the crash at minute 37 in the
//! third piece, and a resume to the bytes of the run without it, at full scale and at any crash byte of a scaled run.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::expect_used,
    clippy::as_conversions,
    reason = "tests compute expected values freely"
)]

use ww_tape::TapeReader;
use ww_tape_example::{
    CRASH_MINUTE, CRASH_PAST_END, Drive, Error, FAULT_MINUTE, emergency_frame, frame,
};

const US_PER_MINUTE: i64 = 60_000_000;

#[test]
fn the_full_hour_is_the_same_every_run_and_tells_the_story() -> Result<(), Error> {
    let drive = Drive::full();
    let whole = drive.record()?;
    assert_eq!(
        whole.pieces,
        drive.record()?.pieces,
        "the same settings give the same bytes"
    );
    for piece in &whole.pieces {
        let r = TapeReader::open(&piece[..]).expect("a piece opens");
        let verified = r.verify(0).expect("a piece verifies");
        assert!(verified.holds, "every piece holds its content hash");
    }

    let story = whole.story()?;
    assert_eq!(
        story.crash.piece, 2,
        "the host loses power while it writes the third piece"
    );
    let minute = i64::from(CRASH_MINUTE) * US_PER_MINUTE;
    assert!(
        (minute..minute + 1_000_000).contains(&story.torn_from_us),
        "the crash tears the frames of the second after minute 37: {}",
        story.torn_from_us
    );
    assert!(
        story.kept_bytes < story.crash.offset,
        "the resume cuts the torn block off"
    );
    assert_eq!(story.lost_acts, 1, "the crash costs one chunk of frames");
    assert!(
        story.same_bytes,
        "the resumed chain holds the bytes of the one written without the crash"
    );
    assert!(story.pieces.len() > usize::try_from(story.crash.piece).unwrap_or(0) + 1);

    // the drive faults at minute 52 and says so on can0
    let period_us = 1_000_000 / u64::from(drive.can_hz);
    let (at, payload) = frame(emergency_frame(period_us), period_us);
    assert_eq!(story.fault_minute, FAULT_MINUTE);
    assert!(at >= i64::from(FAULT_MINUTE) * US_PER_MINUTE);
    assert_eq!(payload[..4], ww_tape_example::EMERGENCY_ID.to_le_bytes());
    Ok(())
}

#[test]
fn a_scaled_run_resumes_from_a_crash_at_any_byte() -> Result<(), Error> {
    let drive = Drive::scaled(1000).ok_or(Error::Settings("1000 divides the rates"))?;
    let whole = drive.record()?;
    let len = whole.len();
    let (mut resumed, mut past_end) = (0, 0);
    // crashes spread over the whole recording: in blocks of data, checkpoints and the ends of pieces
    for byte in (1..len).step_by(usize::try_from(len / 211).unwrap_or(1)) {
        match drive.crash(byte) {
            Ok(crashed) => {
                assert_eq!(crashed.crash.byte, byte);
                let on_disk: u64 = crashed.pieces()?.iter().map(|p| p.len() as u64).sum();
                assert_eq!(on_disk, byte, "the crash leaves the bytes before it");
                let resume = crashed.resume()?;
                assert_eq!(
                    resume.recording.pieces, whole.pieces,
                    "a crash at byte {byte} resumes to the same bytes"
                );
                resumed += 1;
            }
            Err(Error::Settings(CRASH_PAST_END)) => past_end += 1,
            Err(e) => return Err(e),
        }
    }
    assert_eq!(
        past_end, 0,
        "every byte of the recording is a place to crash"
    );
    assert!(resumed > 200, "{resumed} crashes resumed");
    Ok(())
}

#[test]
fn a_scaled_run_tells_the_same_story() -> Result<(), Error> {
    let story = Drive::scaled(1000)
        .ok_or(Error::Settings("1000 divides the rates"))?
        .story()?;
    let minute = i64::from(CRASH_MINUTE) * US_PER_MINUTE;
    assert!((minute..minute + US_PER_MINUTE).contains(&story.torn_from_us));
    assert!(story.same_bytes);
    Ok(())
}

#[test]
fn a_shorter_run_is_the_start_of_the_hour() -> Result<(), Error> {
    let drive = Drive::scaled(1000).ok_or(Error::Settings("1000 divides the rates"))?;
    let first = drive.minutes(1).record()?;
    let hour = drive.record()?;
    // the shorter run's blocks up to its last chunk of samples are the hour's; its last summaries and its end differ
    let (a, b) = (&first.pieces[0], &hour.pieces[0]);
    let r = TapeReader::open(&a[..]).expect("the first minute opens");
    let end = r
        .entries(0)
        .expect("its index")
        .iter()
        .filter(|e| {
            e.track_number == ww_tape_example::PHASE_A && e.kind == ww_tape::BlockKind::Series
        })
        .map(|e| e.offset + u64::from(e.len))
        .max()
        .expect("a chunk of phase_a");
    let end = usize::try_from(end).expect("in memory");
    assert_eq!(a[..end], b[..end]);
    Ok(())
}
