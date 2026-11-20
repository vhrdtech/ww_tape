//! Print the numbers of the drive's story, recorded at a scale: `ww_tape_example [K]`, `K` dividing 1000 (1, the full
//! hour, by default).
#![allow(
    clippy::print_stdout,
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::arithmetic_side_effects,
    reason = "the binary prints the story's numbers, computed freely from sizes far below 2^52"
)]

use ww_tape::index::{IndexBlock, IndexKind};
use ww_tape::reader::Found;
use ww_tape::{BlockKind, FLAG_REPEAT, FileBlocks, Recovery};
use ww_tape_example::{Damaged, Drive, EARLIER_CHAIN, Error, Recording, Salvaged, Story};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let k = match std::env::args().nth(1) {
        Some(k) => k.parse()?,
        None => 1,
    };
    let drive = Drive::scaled(k).ok_or(Error::Settings("the scale divides 1000"))?;
    print(&drive.story()?);
    let recording = drive.record()?;
    let first = recording
        .pieces
        .first()
        .ok_or(Error::Settings("the run has a piece"))?;
    println!();
    print_blocks(first)?;
    let (puts, wrote) = recording.mode_puts(0);
    let at: Vec<String> = wrote
        .iter()
        .map(|us| format!("{} s", us / 1_000_000))
        .collect();
    println!();
    println!(
        "Piece 0: {puts} puts of the mode; {} of them wrote, at {}.",
        wrote.len(),
        at.join(", ")
    );
    println!();
    print_damage(&drive, &recording)?;
    println!();
    print_card(&drive)?;
    Ok(())
}

/// The run on a raw SD card that held an earlier run, the power failing at the story's crash.
fn print_card(drive: &Drive) -> Result<(), Error> {
    let today = drive.on_sd_card();
    let earlier = today.chain(EARLIER_CHAIN).record()?;
    let card = today
        .crash(today.record()?.story_crash_byte()?)?
        .on_card(&earlier)?;
    let read = card.read(&earlier)?;
    let len = card.bytes.len() as u64;
    println!(
        "On a raw card of {} that held an earlier run: the run wrote {} before the power failed.",
        mib(len),
        mib(card.crash.byte)
    );
    println!(
        "As a raw medium: {} pieces, the data ends at byte {}; {} of the earlier run stay behind as stale data.",
        read.raw_pieces,
        read.raw_end,
        mib(len - read.raw_end)
    );
    if let Some((at, why)) = &read.raw_stop {
        println!("The walk stops at byte {at}: {why}.");
    }
    println!(
        "As a file: {} pieces, {} of them the earlier run's.",
        read.file_pieces, read.file_earlier
    );
    Ok(())
}

/// The story's three damages to the last piece and what the readers get from each.
fn print_damage(drive: &Drive, recording: &Recording) -> Result<(), Error> {
    let [flip, first, last] = recording.story_damage()?;
    let s = &first_salvaged(drive, &flip)?;
    let us = s.first_tick;
    println!(
        "Damage to piece {} ({} bytes), which holds the fault:",
        flip.piece,
        flip.whole.len()
    );
    println!();
    println!(
        "A flipped bit at byte {} of the piece: a chunk of phase_a from {:02}:{:02}, {} bytes; {} of its {} segments fail; {} of its {} samples lost.",
        flip.damage.start,
        us / 60_000_000,
        us / 1_000_000 % 60,
        s.block.len,
        s.bad_segments,
        s.segments,
        s.items_lost.unwrap_or(s.items),
        s.items,
    );
    println!();
    println!(
        "| Damage | Found by | Content hash holds | Index lists | Read | Unreadable | Unlisted | Prefix: blocks, stops at | Resync: blocks, salvaged, reports |"
    );
    println!("|---|---|---|---:|---:|---:|---:|---|---|");
    let mut lines = Vec::new();
    for (name, d) in [
        ("a flipped bit", &flip),
        ("the first 4 KiB lost", &first),
        ("the last 4 KiB lost", &last),
    ] {
        let r = d.read(drive)?;
        let found = match r.found {
            Found::Tail => "tail".to_owned(),
            Found::Checkpoint { scanned } => format!("checkpoint, {scanned} bytes scanned"),
            Found::Scan { scanned } => format!("scan, {scanned} bytes"),
        };
        let holds = r.holds.map_or("no close".to_owned(), |h| h.to_string());
        let stop = r
            .prefix
            .reports
            .iter()
            .find(|e| e.1 == "cut")
            .map_or("the end".to_owned(), |e| format!("byte {}", e.0));
        println!(
            "| {name} | {found} | {holds} | {} of {} | {} | {} | {} | {}, {stop} | {}, {}, {} |",
            r.read.len() + r.unreadable.len(),
            r.data_blocks(),
            r.read.len(),
            r.unreadable.len(),
            r.unlisted.len(),
            r.prefix.blocks.len(),
            r.resync.blocks.len(),
            r.resync.salvaged.len(),
            r.resync.reports.len(),
        );
        lines.push(format!(
            "{name}: unreadable blocks {:?}; definitions at the end {} of {}.",
            r.unreadable.iter().map(|b| b.kind).collect::<Vec<_>>(),
            r.definitions,
            r.definitions_written
        ));
    }
    println!();
    for l in lines {
        println!("{l}");
    }
    Ok(())
}

/// The chunk a Resync walk salvages from `d`.
fn first_salvaged(drive: &Drive, d: &Damaged) -> Result<Salvaged, Error> {
    let r = d.read(drive)?;
    r.resync
        .salvaged
        .first()
        .copied()
        .ok_or(Error::Settings("the flipped bit damages a chunk's payload"))
}

/// What a block of a piece is, as the book's anatomy of a piece names it.
fn part(b: &ww_tape::Block<'_>) -> Result<&'static str, Box<dyn std::error::Error>> {
    Ok(match b.kind {
        BlockKind::PieceHead => "piece head",
        BlockKind::Records if b.flags & FLAG_REPEAT != 0 => "records blocks of puts and copies",
        BlockKind::Records => "records blocks",
        BlockKind::Events => "events chunks",
        BlockKind::Series => "series chunks",
        BlockKind::Summary => "summary blocks",
        BlockKind::Index => match IndexBlock::read(b).map_err(|e| format!("{e:?}"))?.kind() {
            IndexKind::Checkpoint => "checkpoints",
            IndexKind::Final => "final index",
        },
        BlockKind::Close => "close",
        BlockKind::Tail => "tail",
        _ => "other blocks",
    })
}

/// The blocks of `piece` by what they are: their count and bytes, in order of first appearance.
fn print_blocks(piece: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    let mut parts: Vec<(&'static str, u64, u64)> = Vec::new();
    let mut walk = FileBlocks::with_recovery(piece, Recovery::Prefix);
    while let Some(b) = walk.next_block() {
        let b = b.map_err(|e| format!("{e:?}"))?;
        let name = part(&b)?;
        let len = b.len() as u64;
        match parts.iter_mut().find(|p| p.0 == name) {
            Some(p) => {
                p.1 += 1;
                p.2 += len;
            }
            None => parts.push((name, 1, len)),
        }
    }
    println!("Piece 0, {} bytes, by its blocks:", piece.len());
    println!();
    println!("| Blocks | Count | Bytes |");
    println!("|---|---:|---:|");
    for (name, n, bytes) in &parts {
        println!("| {name} | {n} | {bytes} |");
    }
    let structure: u64 = parts
        .iter()
        .filter(|p| {
            matches!(
                p.0,
                "piece head" | "checkpoints" | "final index" | "close" | "tail"
            )
        })
        .map(|p| p.2)
        .sum();
    println!();
    println!(
        "The piece head, checkpoints, final index, close and tail: {structure} bytes, {:.2} % of the piece.",
        structure as f64 * 100.0 / piece.len() as f64
    );
    Ok(())
}

fn mib(bytes: u64) -> String {
    format!("{:.2} MiB", bytes as f64 / f64::from(1 << 20))
}

fn print(s: &Story) {
    let total: u64 = s.pieces.iter().sum();
    println!(
        "The drive's run at scale 1/{}: {} minutes.",
        25_000 / s.drive.phase_hz,
        s.drive.minutes
    );
    println!();
    println!("| Number | Value |");
    println!("|---|---:|");
    println!("| pieces | {} |", s.pieces.len());
    for (seq, len) in s.pieces.iter().enumerate() {
        println!("| piece {seq} | {} ({len} bytes) |", mib(*len));
    }
    println!("| recording | {} ({total} bytes) |", mib(total));
    println!("| can0 frames | {} |", s.frames);
    println!(
        "| can0 bytes per frame | {:.2} |",
        s.frame_bytes as f64 / s.frames as f64
    );
    println!("| phase_a samples | {} |", s.samples);
    println!(
        "| phase_a bytes per sample | {:.3} |",
        s.sample_bytes as f64 / s.samples as f64
    );
    let at = s.torn_from_us / 1000;
    println!(
        "| crash | byte {} of the recording, piece {} at {} |",
        s.crash.byte, s.crash.piece, s.crash.offset
    );
    println!(
        "| torn chunk of frames from | {:02}:{:02}.{:03} |",
        at / 60_000,
        at / 1000 % 60,
        at % 1000
    );
    println!(
        "| resume keeps | {} of piece {}; cuts {} bytes |",
        s.kept_bytes,
        s.crash.piece,
        s.crash.offset - s.kept_bytes
    );
    println!("| acts the crash lost, given again | {} |", s.lost_acts);
    println!(
        "| resumed chain has the bytes of the uncrashed one | {} |",
        s.same_bytes
    );
    println!(
        "| fault | minute {}, emergency frame {} |",
        s.fault_minute, s.emergency_frame
    );
}
