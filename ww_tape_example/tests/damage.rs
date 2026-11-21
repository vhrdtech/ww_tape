//! Damage to the drive's archived last piece and the run on a raw card, against what the book's Part V tells: a
//! flipped bit costs one segment, lost sectors at the start cost the blocks in them and two definitions no block
//! uses, lost sectors at the end cost no data, and a raw card's stale run is never read as data.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::expect_used,
    clippy::as_conversions,
    reason = "tests compute expected values freely"
)]

use ww_tape::BlockKind;
use ww_tape::reader::Found;
use ww_tape_example::{Damaged, Drive, EARLIER_CHAIN, Error, FAULT_MINUTE, FLIP_MINUTE, LOST_UNIT};

#[test]
fn damage_to_the_last_piece_costs_what_the_book_says() -> Result<(), Error> {
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
        )
    };
    check_flip(&drive, &flip)?;
    check_first(&drive, &first)?;
    check_last(&drive, &last)
}

fn check_flip(drive: &Drive, flip: &Damaged) -> Result<(), Error> {
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
    assert!(r.unlisted.is_empty());
    let stop = r
        .prefix
        .reports
        .iter()
        .find(|e| e.1 == "cut")
        .expect("Prefix stops");
    assert_eq!(stop.0, bad.at, "Prefix stops at the damaged chunk");
    assert_eq!(r.resync.blocks, r.blocks, "Resync hands out every block");
    assert!(r.resync.reports.is_empty());
    assert_eq!(r.resync.salvaged.len(), 1, "one chunk salvaged");
    let s = r.resync.salvaged[0];
    assert_eq!((s.segments, s.bad_segments), (1, 1));
    assert_eq!((s.items, s.items_lost), (25_000, Some(25_000)));
    assert_eq!(s.first_tick, i64::from(FLIP_MINUTE) * 60_000_000);
    // the numbers the chapter "When bytes go bad" gives
    assert_eq!(flip.damage.start, 9_730_801, "the flipped byte");
    assert_eq!(s.block.len, 28_171, "the flipped chunk's bytes");
    assert_eq!((r.read.len(), r.data_blocks()), (3446, 3447));
    assert_eq!(
        (r.prefix.blocks.len(), stop.0),
        (1156, 9_716_716),
        "Prefix's blocks and where it stops"
    );
    assert_eq!(r.resync.blocks.len(), 3457, "the blocks Resync hands out");
    Ok(())
}

fn check_first(drive: &Drive, first: &Damaged) -> Result<(), Error> {
    // the first 4 KiB: the blocks in them; every track, clock and map comes from its copy, the producer and the layout
    // are lost
    let r = first.read(drive)?;
    assert_eq!(r.found, Found::Tail);
    assert_eq!(r.holds, Some(false));
    assert!(!r.unreadable.is_empty());
    assert!(r.unreadable.iter().all(|b| b.at < LOST_UNIT));
    assert!(r.read.iter().all(|b| b.at + b.len > LOST_UNIT));
    assert_eq!((r.definitions, r.definitions_written), (8, 10));
    assert!(r.prefix.blocks.is_empty(), "Prefix stops at once");
    assert!(
        r.blocks
            .iter()
            .filter(|b| b.at >= LOST_UNIT)
            .all(|b| r.resync.blocks.contains(b)),
        "Resync reads every block after the lost bytes"
    );
    assert_eq!((r.read.len(), r.unreadable.len()), (3443, 4));
    Ok(())
}

fn check_last(drive: &Drive, last: &Damaged) -> Result<(), Error> {
    // the last 4 KiB: the tail and the close; the reader starts at the newest checkpoint, and every block of data reads
    let r = last.read(drive)?;
    assert_eq!(r.found, Found::Checkpoint { scanned: 1_096_734 });
    assert_eq!(r.holds, None, "no close to check the piece by");
    assert_eq!(r.read.len(), r.data_blocks());
    assert!(r.unreadable.is_empty() && r.unlisted.is_empty());
    assert_eq!(r.definitions, r.definitions_written);
    Ok(())
}

#[test]
fn a_raw_card_never_reads_the_earlier_run() -> Result<(), Error> {
    let today = Drive::full().on_sd_card();
    let earlier = today.chain(EARLIER_CHAIN).record()?;
    let run = today.record()?;
    let crashed = today.crash(run.story_crash_byte()?)?;
    let crash = crashed.crash;
    let card = crashed.on_card(&earlier)?;
    assert_eq!(card.bytes.len() as u64, earlier.len());
    // the numbers the chapter "Where the data ends on a raw card" gives: 109.95 MiB, 68.96 MiB, 40.99 MiB
    assert_eq!((card.bytes.len(), crash.byte), (115_290_112, 72_312_518));
    let read = card.read(&earlier)?;
    assert_eq!(read.raw_pieces, 3, "two closed pieces and the torn one");
    let torn_start = crash.byte - crash.offset;
    assert!(
        (torn_start..crash.byte).contains(&read.raw_end),
        "the data ends at the torn block"
    );
    assert_eq!(read.raw_stop.map(|s| s.0), Some(read.raw_end));
    assert_eq!(read.raw_end, 72_311_255, "where today's data ends");
    assert_eq!(
        (read.file_pieces, read.file_earlier),
        (4, 1),
        "a file reader takes the earlier run's last piece by its tail"
    );
    Ok(())
}
