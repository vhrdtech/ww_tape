//! The drive's recording against the story the book tells: the same bytes every run, the crash at minute 37 in the
//! third piece, and a resume to the bytes of the run without it, at full scale and at any crash byte of a scaled run.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::expect_used,
    clippy::as_conversions,
    clippy::cast_precision_loss,
    reason = "tests compute expected values freely"
)]

use example_capture::about;
use example_drive::{
    CRASH_MINUTE, CRASH_PAST_END, Drive, Error, FAULT_MINUTE, Recording, Story, emergency_frame,
    frame, parts, parts_table,
};
use ww_tape::TapeReader;

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
    // docs/tape.md quotes the pieces' sizes rounded (docs/writers.md, "Numbers")
    let sizes: Vec<u64> = whole.pieces.iter().map(|p| p.len() as u64).collect();
    let (least, most) = (sizes.iter().min(), sizes.iter().max());
    let phrase = format!(
        "four pieces of about {} to {}",
        about(*least.expect("pieces")),
        about(*most.expect("pieces"))
    );
    assert!(
        page("tape.md").contains(&phrase),
        "docs/tape.md says {phrase:?}"
    );

    // the mode is put before each one-second block: the first piece's 900; the next block's put starts the next piece
    // by the cut policy; six puts write: idle, its two copies, run at minute 1, its two copies
    let (puts, wrote) = whole.mode_puts(0);
    assert_eq!(puts, 900, "one put of the mode a block");
    assert_eq!(
        wrote,
        [0, 1_000_000, 41_000_000, 60_000_000, 61_000_000, 97_000_000]
    );

    let story = whole.story()?;
    rounded_on_the_pages(&whole, &story, puts, &wrote)?;
    assert_eq!(
        story.crash.piece, 2,
        "the host loses power while it writes the third piece"
    );
    let figures = Drive::scaled(1000)
        .ok_or(Error::Settings("1000 divides the rates"))?
        .story()?;
    assert_eq!(
        (figures.pieces.len(), figures.crash.piece),
        (story.pieces.len(), story.crash.piece),
        "the figures' scale tells the story of the full hour: as many pieces, the crash in the same one"
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
    assert_eq!(payload[..4], example_drive::EMERGENCY_ID.to_le_bytes());
    Ok(())
}

fn page(name: &str) -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/").to_owned() + name;
    let text = std::fs::read_to_string(&path).expect("a book page");
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The pages of "How a tape is built", Series and Summaries quote the story's numbers, rounded where their precision doesn't matter
/// (docs/writers.md, "Numbers").
fn rounded_on_the_pages(
    whole: &Recording,
    story: &Story,
    puts: usize,
    wrote: &[i64],
) -> Result<(), Error> {
    let first = &whole.pieces[0];
    let parts = parts(first)?;
    let structure: u64 = parts
        .iter()
        .filter(|p| p.is_structure())
        .map(|p| p.bytes)
        .sum();
    let index = parts
        .iter()
        .find(|p| p.name == "final index")
        .expect("a closed piece has a final index");
    let part = |name: &str| {
        parts
            .iter()
            .find(|p| p.name == name)
            .expect("piece 0 holds every part")
    };
    let (summaries, series) = (part("summary blocks"), part("series chunks"));
    let len = first.len() as u64;
    let s = |us: i64| us / 1_000_000;
    for (name, phrase) in [
        (
            "tracks.md",
            format!(
                "takes about {:.1} bytes a sample",
                story.sample_bytes as f64 / story.samples as f64
            ),
        ),
        (
            "blocks.md",
            format!(
                "at about {:.1} bytes a frame",
                story.frame_bytes as f64 / story.frames as f64
            ),
        ),
        (
            "state.md",
            format!("{puts} times while it writes the first piece"),
        ),
        (
            "state.md",
            format!(
                "idle at the start, its copies at {} and {} seconds, run at minute {} and its copies at {} and {} seconds",
                s(wrote[1]),
                s(wrote[2]),
                s(wrote[3]) / 60,
                s(wrote[4]),
                s(wrote[5])
            ),
        ),
        (
            "state.md",
            format!("ends at about {} MiB", (len + (1 << 19)) >> 20),
        ),
        ("anatomy.md", format!("of the run in about {}", about(len))),
        (
            "series.md",
            format!(
                "costs about {:.1} bytes per sample",
                story.sample_bytes as f64 / story.samples as f64
            ),
        ),
        (
            "summaries.md",
            format!(
                "holds {} summary blocks of about {}, beside about {} of samples",
                summaries.count,
                about(summaries.bytes),
                about(series.bytes)
            ),
        ),
        (
            "anatomy.md",
            format!(
                "take about {:.1} % of it",
                structure as f64 * 100.0 / len as f64
            ),
        ),
        (
            "anatomy.md",
            format!(
                "takes about {} of its piece's {}",
                about(index.bytes),
                about(len)
            ),
        ),
    ] {
        assert!(page(name).contains(&phrase), "docs/{name} says {phrase:?}");
    }
    Ok(())
}

/// The table of the first piece's blocks that docs/anatomy.md shows (`docs/generated/anatomy_parts.txt`).
#[test]
fn book_output_anatomy_parts() -> Result<(), Error> {
    let whole = Drive::full().record()?;
    let table = parts_table(&parts(&whole.pieces[0])?).expect("a table");
    ww_tape_book_outputs::check("anatomy_parts.txt", &table);
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
fn every_scale_tells_the_same_story() -> Result<(), Error> {
    let minute = i64::from(CRASH_MINUTE) * US_PER_MINUTE;
    let mut shapes = Vec::new();
    for k in [10, 20, 25, 40, 50, 100, 125, 200, 250, 500, 1000] {
        let story = Drive::scaled(k)
            .ok_or(Error::Settings("k divides 1000"))?
            .story()?;
        assert!(
            (minute..minute + US_PER_MINUTE).contains(&story.torn_from_us),
            "at 1/{k} the crash tears the frames of minute 37"
        );
        assert!(story.same_bytes, "at 1/{k} the resume gives the same bytes");
        shapes.push((k, story.pieces.len(), story.crash.piece));
    }
    assert!(
        shapes
            .iter()
            .all(|&(_, pieces, crash)| (pieces, crash) == (4, 2)),
        "four pieces and the crash in the third at every scale: {shapes:?}"
    );
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
            e.track() == Some(first.numbers.phase_a) && e.kind == ww_tape::BlockKind::Series
        })
        .map(|e| e.offset + u64::from(e.len))
        .max()
        .expect("a chunk of phase_a");
    let end = usize::try_from(end).expect("in memory");
    assert_eq!(a[..end], b[..end]);
    Ok(())
}

#[test]
fn the_overcurrent_passes_the_limit_within_16_bits() {
    // the 20 ms before the fault at full rate, a sample every 40 µs
    let fault_us = i64::from(FAULT_MINUTE) * US_PER_MINUTE;
    let peak = (0..500i64)
        .map(|k| {
            let us = fault_us - 20_000 + 40 * k;
            let n = (us / 40).cast_unsigned();
            example_drive::phase_ma(us, n, 25_000).unsigned_abs()
        })
        .max()
        .expect("samples");
    assert!(
        i32::from(peak) > example_drive::LIMIT_MA,
        "the current passes the limit: {peak} mA"
    );
    assert!(peak < i16::MAX.unsigned_abs(), "no sample clips: {peak} mA");
}

#[test]
fn the_command_frame_sends_the_trip_limit() {
    // frames at 1 ms; the first in each turn of four is the command 0x101
    let (_, payload) = example_drive::frame(0, 1_000);
    assert_eq!(payload[..4], 0x101u32.to_le_bytes(), "frame 0 is a command");
    let limit = u16::from_le_bytes([payload[6], payload[7]]);
    assert_eq!(
        i32::from(limit) * 100,
        example_drive::LIMIT_MA,
        "the torque limit in 0.1 A is the limit the drive trips at"
    );
}
