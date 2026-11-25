//! Damage to the drive's archived last piece and the run on a raw card, against what the book's Part V tells: a
//! flipped bit costs one segment, lost sectors at the start cost the blocks in them and no definition, lost sectors at
//! the end cost no data, and a raw card's stale run is never read as data.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::expect_used,
    clippy::as_conversions,
    reason = "tests compute expected values freely"
)]

use example_capture::about;
use example_drive::{Damaged, Drive, EARLIER_CHAIN, Error, FAULT_MINUTE, FLIP_MINUTE, LOST_UNIT};
use ww_tape::BlockKind;
use ww_tape::reader::Found;

const US_PER_S: i64 = 1_000_000;

/// A book page with its line breaks as spaces.
fn page(name: &str) -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/").to_owned() + name;
    let text = std::fs::read_to_string(&path).expect("a book page");
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Asserts that `docs/{name}` holds `phrase`: a number rounded the way the pages write it (docs/writers.md, "Numbers").
fn says(name: &str, phrase: &str) {
    assert!(page(name).contains(phrase), "docs/{name} says {phrase:?}");
}

#[test]
fn book_output_damage_to_the_last_piece_costs_what_the_book_says() -> Result<(), Error> {
    let drive = Drive::full();
    let recording = drive.record()?;
    let [flip, first, last] = recording.story_damage()?;
    assert_eq!(flip.piece, 3, "the flipped bit lands in the last piece");
    let last_piece = ww_tape::TapeReader::open(&flip.whole[..]).expect("the last piece opens");
    let close = &last_piece.pieces()[0].close.as_ref().expect("a close").0;
    assert_eq!(close.next, None, "the last piece names no next piece");
    const {
        assert!(
            FLIP_MINUTE < FAULT_MINUTE,
            "the fault itself stays readable"
        );
    };
    let (flip_row, walks) = check_flip(&drive, &flip)?;
    let first_row = check_first(&drive, &first)?;
    let last_row = check_last(&drive, &last)?;
    let cases = format!(
        "| Damage | The reader finds the piece by | Content hash | Blocks that read | Definitions found |\n\
         |---|---|---|---:|---:|\n{flip_row}{first_row}{last_row}"
    );
    ww_tape_book_outputs::check("damage_cases.txt", &cases);
    ww_tape_book_outputs::check("damage_walks.txt", &walks);
    Ok(())
}

/// The row of the table of damages that docs/damaged_pieces.md includes (`docs/generated/damage_cases.txt`).
fn case_row(damage: &str, r: &example_drive::Damage) -> String {
    let found = match r.found {
        Found::Tail => "its tail".to_owned(),
        Found::Checkpoint { scanned: 0 } => {
            "its final index, with no byte to scan after it".to_owned()
        }
        Found::Checkpoint { scanned } => format!(
            "its newest checkpoint, scanning {} after it",
            about(scanned)
        ),
        Found::Scan { scanned } => format!("a scan of {}", about(scanned)),
    };
    let holds = match r.holds {
        Some(true) => "holds",
        Some(false) => "does not hold",
        None => "no close to check",
    };
    format!(
        "| {damage} | {found} | {holds} | {} | {} of {} |\n",
        r.read.len(),
        r.definitions,
        r.definitions_written
    )
}

fn check_flip(drive: &Drive, flip: &Damaged) -> Result<(String, String), Error> {
    // a flipped bit: one chunk of phase_a, its one segment, its second of samples; every other block reads
    let r = flip.read(drive)?;
    assert_eq!(r.found, Found::Tail);
    assert_eq!(r.holds, Some(false), "the content hash finds the flip");
    assert_eq!(r.unreadable.len(), 1);
    let bad = r.unreadable[0];
    assert_eq!(bad.kind, BlockKind::Series);
    assert!((bad.at..bad.at + bad.len).contains(&flip.damage.start));
    assert_eq!(
        r.read.len() + 1,
        r.data_blocks(),
        "the index lists every block of data"
    );
    assert_eq!(r.unlisted, [] as [example_drive::Placed; 0]);
    let stop = r
        .prefix
        .reports
        .iter()
        .find(|e| e.1 == "cut")
        .expect("Prefix stops");
    assert_eq!(stop.0, bad.at, "Prefix stops at the damaged chunk");
    assert_eq!(r.resync.blocks, r.blocks, "Resync hands out every block");
    assert_eq!(r.resync.reports, [] as [(u64, &str, u64); 0]);
    assert_eq!(r.resync.salvaged.len(), 1, "one chunk salvaged");
    let s = r.resync.salvaged[0];
    assert_eq!((s.segments, s.bad_segments), (1, 1));
    assert_eq!((s.items, s.items_lost), (25_000, Some(25_000)));
    assert_eq!(s.first_tick, i64::from(FLIP_MINUTE) * 60_000_000);
    // the numbers the chapter "When bytes go bad" gives
    assert_eq!(flip.damage.start, 9_740_029, "the flipped byte");
    assert_eq!(s.block.len, 28_171, "the flipped chunk's bytes");
    flip_on_the_page(flip.damage.start, s.block.len);
    assert_eq!(
        (r.read.len(), r.data_blocks(), r.mode_unknown.len()),
        (3448, 3449, 0),
        "blocks that read, blocks listed, chunks without the mode"
    );
    assert_eq!(
        (r.prefix.blocks.len(), stop.0),
        (1158, 9_725_944),
        "Prefix's blocks and where it stops"
    );
    assert_eq!(r.resync.blocks.len(), 3459, "the blocks Resync hands out");
    says(
        "damaged_pieces.md",
        &format!(
            "the piece holds {} blocks that its index lists",
            r.data_blocks()
        ),
    );
    // the walks of docs/reading_past_damage.md (`docs/generated/damage_walks.txt`)
    let walks = format!(
        "| Walk over the flipped piece | Blocks it hands out | Where it ends |\n\
         |---|---:|---|\n\
         | `Recovery::Prefix` | {} | stops at the damaged chunk, about {} in |\n\
         | `Recovery::Resync` | {} | reads to the end, the damaged chunk among its blocks, with {} reports |\n",
        r.prefix.blocks.len(),
        about(stop.0),
        r.resync.blocks.len(),
        r.resync.reports.len()
    );
    Ok((case_row("a flipped bit", &r), walks))
}

/// The rounded numbers "When bytes go bad" gives for the flipped bit: the byte and the chunk's size.
fn flip_on_the_page(byte: u64, chunk: u64) {
    says(
        "damage.md",
        &format!("a flipped bit about {} in", about(byte)),
    );
    says(
        "damage.md",
        &format!("in about {}. That is less than one segment", about(chunk)),
    );
}

fn check_first(drive: &Drive, first: &Damaged) -> Result<String, Error> {
    // the first 4 KiB: the blocks in them; every definition comes from its copy, the producer and the layout too
    let r = first.read(drive)?;
    assert_eq!(r.found, Found::Tail);
    assert_eq!(r.holds, Some(false));
    assert_ne!(r.unreadable, [] as [example_drive::Placed; 0]);
    assert!(r.unreadable.iter().all(|b| b.at < LOST_UNIT));
    assert!(r.read.iter().all(|b| b.at + b.len > LOST_UNIT));
    assert_eq!((r.definitions, r.definitions_written), (10, 10));
    assert!(r.prefix.blocks.is_empty(), "Prefix stops at once");
    assert!(
        r.blocks
            .iter()
            .filter(|b| b.at >= LOST_UNIT)
            .all(|b| r.resync.blocks.contains(b)),
        "Resync reads every block after the lost bytes"
    );
    assert_eq!((r.read.len(), r.unreadable.len()), (3444, 5));
    // the cut's carried mode lay in the lost bytes: no mode until its first copy, 4 KiB on, in the second at 45:01
    let (from, to) = (r.mode_unknown[0], *r.mode_unknown.last().expect("chunks"));
    assert_eq!((from / US_PER_S, to / US_PER_S), (45 * 60, 45 * 60));
    says(
        "damaged_pieces.md",
        "no mode in force over the piece's first second, 45:00",
    );
    says(
        "damaged_pieces.md",
        &format!(
            "every block outside the lost bytes, {} of the {}",
            r.read.len(),
            r.data_blocks()
        ),
    );
    Ok(case_row("the first 4 KiB lost", &r))
}

fn check_last(drive: &Drive, last: &Damaged) -> Result<String, Error> {
    // the last 4 KiB: the padding, the close and the tail; the final index ends before them and reads whole, so the
    // reader takes it as the newest index block with no byte to scan after it
    let r = last.read(drive)?;
    assert_eq!(r.found, Found::Checkpoint { scanned: 0 });
    assert_eq!(r.holds, None, "no close to check the piece by");
    assert_eq!(r.read.len(), r.data_blocks());
    assert!(r.unreadable.is_empty() && r.unlisted.is_empty());
    assert_eq!(r.definitions, r.definitions_written);
    assert!(r.mode_unknown.is_empty(), "the mode reads at every chunk");
    says(
        "damaged_pieces.md",
        &format!("Every one of the {} blocks of data reads", r.data_blocks()),
    );
    Ok(case_row("the last 4 KiB lost", &r))
}

#[test]
fn a_raw_card_never_reads_the_earlier_run() -> Result<(), Error> {
    let today = Drive::full().on_sd_card();
    let earlier = today.chain(EARLIER_CHAIN).record()?;
    let run = today.record()?;
    let crashed = run.crash(run.story_crash_byte()?)?;
    let crash = crashed.crash;
    let card = crashed.on_card(&earlier)?;
    assert_eq!(card.bytes.len() as u64, earlier.len());
    assert_eq!((card.bytes.len(), crash.byte), (115_523_584, 72_444_871));
    let read = card.read(&earlier)?;
    assert_eq!(read.raw_pieces, 3, "two closed pieces and the torn one");
    let torn_start = crash.byte - crash.offset;
    assert!(
        (torn_start..crash.byte).contains(&read.raw_end),
        "the data ends at the torn block"
    );
    assert_eq!(read.raw_stop.map(|s| s.0), Some(read.raw_end));
    assert_eq!(read.raw_end, 72_443_608, "where today's data ends");
    let len = card.bytes.len() as u64;
    says(
        "raw_media.md",
        &format!(
            "It holds about {}, and today's run wrote about {} of it",
            about(len),
            about(crash.byte)
        ),
    );
    says(
        "raw_media.md",
        &format!(
            "The earlier run's other {} or so",
            about(len - read.raw_end)
        ),
    );
    assert_eq!(
        (read.file_pieces, read.file_earlier),
        (4, 1),
        "a file reader takes the earlier run's last piece by its tail"
    );
    Ok(())
}

#[test]
fn a_small_piece_keeps_every_definition_after_its_first_4_kib() -> Result<(), Error> {
    // at 1/100 a piece is far shorter than the spacing of the copies: only the first copies, due 4 KiB on, survive
    let drive = Drive {
        copy_spacing: ww_tape::writer::near::COPY_SPACING,
        ..Drive::scaled(100).expect("100 divides 1000")
    };
    let recording = drive.record()?;
    let last = recording.pieces.len() - 1;
    let len = recording.pieces[last].len() as u64;
    assert!(
        len < ww_tape::writer::near::COPY_SPACING.get(),
        "the piece holds no spaced copy"
    );
    let r = recording.zero(last, 0..LOST_UNIT)?.read(&drive)?;
    assert_eq!(r.found, Found::Tail);
    assert_eq!((r.definitions, r.definitions_written), (10, 10));
    assert!(r.read.iter().all(|b| b.at + b.len > LOST_UNIT));
    Ok(())
}
