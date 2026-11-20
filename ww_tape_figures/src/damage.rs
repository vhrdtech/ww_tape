//! `prefix_resync.svg` and `loss_cases.svg`: the drive's last piece ([ww_tape_example]) at 1/1000 of its data,
//! damaged as the book's chapter "Damage" tells, and what each reader still gets from it.

use anyhow::{Context, ensure};
use ww_tape::Recovery;
use ww_tape_example::{Damaged, Drive, Recording, walk};

use crate::OrDebug;
use crate::crash::{LEFT, Part, ROW_H, ROW_W, Span, View, kib, legend, spans};
use crate::svg::Svg;

/// The drive at the figures' scale.
fn drive() -> anyhow::Result<Drive> {
    Drive::scaled(1000).context("1000 divides the drive's rates")
}

/// What became of a block in one reading.
#[derive(Copy, Clone, PartialEq, Eq)]
enum Fate {
    Read,
    Salvaged,
    Lost,
}

/// Draw `blocks` on a row at `y`, each by its fate; the bytes `damage` changed marked above the row.
fn fates(s: &mut Svg, blocks: &[Span], fate: impl Fn(&Span) -> Fate, v: View, y: f64) {
    for b in blocks {
        let (x0, x1) = (v.x(b.at), v.x(b.at + b.len));
        let w = (x1 - x0 - 1.0).max(1.0);
        let class = match fate(b) {
            Fate::Read => b.part.class(),
            Fate::Salvaged => "tint-accent",
            Fate::Lost => "faint",
        };
        s.rect((x0 + 0.5, y), (w, ROW_H), 1.0, class);
        if fate(b) == Fate::Salvaged {
            s.path(
                &format!("M{:.1} {y:.1} h{w:.1} v{ROW_H} h{:.1} Z", x0 + 0.5, -w),
                "thin dash",
            );
        }
    }
}

/// A mark over the bytes `at..end` of `v` at `y`: where the damage is.
fn damage_mark(s: &mut Svg, v: View, at: u64, end: u64, y: f64, label: &str) {
    let (x0, x1) = (v.x(at), v.x(end).max(v.x(at) + 2.0));
    s.rect((x0, y - 6.0), (x1 - x0, ROW_H + 12.0), 0.0, "tint-accent");
    s.path(
        &format!("M{x0:.1} {:.1} V{:.1}", y - 10.0, y + ROW_H + 6.0),
        "accent-line",
    );
    s.text(x0 + 4.0, y - 12.0, "start", "small accent", label);
}

/// The records block about three quarters into `blocks`, and the samples chunk the flipped bit hits: the figure's two
/// damages, in that order of bytes.
fn second_damage(blocks: &[Span], flipped: u64) -> anyhow::Result<&Span> {
    let end = blocks.last().map_or(0, |b| b.at + b.len);
    blocks
        .iter()
        .find(|b| b.part == Part::Log && b.at > flipped && b.at > end / 2)
        .context("a records block after the flipped chunk")
}

/// `prefix_resync.svg`: the last piece with a flipped bit in a chunk of `phase_a` and a damaged records block after
/// it, walked in each recovery mode.
pub fn prefix_resync() -> anyhow::Result<String> {
    let drive = drive()?;
    let recording = drive.record().or_debug()?;
    let (piece, at) = recording.story_flip().or_debug()?;
    let flip = recording.flip(piece, at).or_debug()?;
    let blocks = spans(&flip.whole)?;
    let records = second_damage(&blocks, at)?;
    let mut bytes = flip.bytes.clone();
    let second = records.at + records.len / 2;
    let i = usize::try_from(second)?;
    *bytes.get_mut(i).context("the records block's byte")? ^= 1;
    let (records_at, records_len) = (records.at, records.len);

    let prefix = walk(&drive, &bytes, Recovery::Prefix);
    let resync = walk(&drive, &bytes, Recovery::Resync);
    let chunk = blocks
        .iter()
        .find(|b| b.at <= at && at < b.at + b.len)
        .context("the flipped chunk")?;
    ensure!(
        chunk.part == Part::Samples,
        "the bit flips in a chunk of samples"
    );
    ensure!(
        prefix.blocks.last().map(|b| b.at + b.len) == Some(chunk.at),
        "Prefix stops at the flipped chunk"
    );
    let salvaged = resync
        .salvaged
        .first()
        .context("Resync salvages the chunk")?;
    ensure!(
        resync.salvaged.len() == 1 && salvaged.block.at == chunk.at,
        "Resync hands out the flipped chunk, its payload damaged"
    );
    let bad = resync
        .reports
        .iter()
        .find(|r| r.1 == "bad")
        .context("Resync reports the records block")?;
    ensure!(bad.0 == records_at, "the bad block is the records block");
    ensure!(
        resync.blocks.len() + 1 == blocks.len(),
        "Resync reads every block but the bad one"
    );

    let v = View::new(0, flip.whole.len() as u64);
    let (top, row_p, row_r) = (20.0, 92.0, 196.0);
    let mut s = Svg::new(
        LEFT * 2.0 + ROW_W,
        row_r + ROW_H + 92.0,
        &format!(
            "One piece of {} with two damaged blocks, walked twice: Prefix reads the blocks before the first damage and stops; Resync hands out the chunk with the damaged payload marked, reports the bad records block, skips to the next block and reads on to the end",
            kib(flip.whole.len() as u64)
        ),
    );
    s.text(LEFT, top, "start", "strong", "one damaged piece, two walks");
    s.text(
        LEFT + ROW_W,
        top,
        "end",
        "soft small",
        &format!(
            "the drive's last piece at 1/1000, {}",
            kib(flip.whole.len() as u64)
        ),
    );
    for (y, name) in [(row_p, "Prefix"), (row_r, "Resync")] {
        s.text(LEFT, y - 22.0, "start", "small strong", name);
    }
    fates(
        &mut s,
        &blocks,
        |b| match prefix.blocks.iter().any(|p| p.at == b.at) {
            true => Fate::Read,
            false => Fate::Lost,
        },
        v,
        row_p,
    );
    damage_mark(&mut s, v, at, at + 1, row_p, "flipped bit");
    let xc = v.x(chunk.at);
    s.text(
        xc + (LEFT + ROW_W - xc) / 2.0,
        row_p + ROW_H + 18.0,
        "middle",
        "soft small",
        "not read: the walk stops at the first damage",
    );
    fates(
        &mut s,
        &blocks,
        |b| {
            if b.at == chunk.at {
                Fate::Salvaged
            } else if resync.blocks.iter().any(|p| p.at == b.at) {
                Fate::Read
            } else {
                Fate::Lost
            }
        },
        v,
        row_r,
    );
    damage_mark(&mut s, v, at, at + 1, row_r, "flipped bit");
    damage_mark(&mut s, v, second, second + 1, row_r, "flipped bit");
    let (xc1, xr0) = (v.x(chunk.at + chunk.len), v.x(records_at));
    s.bracket_under(
        xr0,
        v.x(records_at + records_len),
        row_r + ROW_H + 4.0,
        "thin",
    );
    s.text(
        xc1,
        row_r + ROW_H + 18.0,
        "end",
        "small",
        &format!(
            "chunk handed out, {} of {} segment bad",
            salvaged.bad_segments, salvaged.segments
        ),
    );
    s.text(
        xr0,
        row_r + ROW_H + 34.0,
        "start",
        "small",
        &format!("records block reported bad, {} bytes skipped", bad.2),
    );
    legend(
        &mut s,
        row_r + ROW_H + 58.0,
        &[
            ("tint-blue", "read"),
            ("tint-accent", "handed out damaged"),
            ("faint", "not read"),
        ],
    );
    Ok(s.finish())
}

/// `loss_cases.svg`: the last piece three times, each with one damage, and what the reader reads of it.
pub fn loss_cases() -> anyhow::Result<String> {
    let drive = drive()?;
    let recording: Recording = drive.record().or_debug()?;
    let cases: [Damaged; 3] = recording.story_damage().or_debug()?;
    let blocks = spans(&cases[0].whole)?;
    let len = cases[0].whole.len() as u64;
    let v = View::new(0, len);
    let (top, first_row, gap) = (20.0, 78.0, 92.0);
    let mut s = Svg::new(
        LEFT * 2.0 + ROW_W,
        first_row + 2.0 * gap + ROW_H + 70.0,
        "The drive's last piece three times, each with one damage: a flipped bit costs one chunk; the first 4 KiB lost cost the blocks in them; the last 4 KiB lost cost the tail and the close, and the reader finds the blocks from the newest checkpoint",
    );
    s.text(
        LEFT,
        top,
        "start",
        "strong",
        "three damages, what still reads",
    );
    s.text(
        LEFT + ROW_W,
        top,
        "end",
        "soft small",
        &format!("the drive's last piece at 1/1000, {}", kib(len)),
    );
    let names = [
        "a flipped bit",
        "the first 4 KiB lost",
        "the last 4 KiB lost",
    ];
    for (i, (d, name)) in cases.iter().zip(names).enumerate() {
        let y = first_row + gap * i as f64;
        let read = d.read(&drive).or_debug()?;
        s.text(LEFT, y - 22.0, "start", "small strong", name);
        let damaged = |b: &Span| b.at + b.len > d.damage.start && b.at < d.damage.end;
        // a block the index lists reads or not by the index; the piece's own structure is lost where damaged
        let fate = |b: &Span| match (is_listed(b.part), read.read.iter().any(|r| r.at == b.at)) {
            (true, true) => Fate::Read,
            (false, _) if !damaged(b) => Fate::Read,
            _ => Fate::Lost,
        };
        fates(&mut s, &blocks, fate, v, y);
        damage_mark(&mut s, v, d.damage.start, d.damage.end, y, "");
        let note = match i {
            0 => format!(
                "{} of {} blocks of data read; the chunk's second of samples is lost",
                read.read.len(),
                read.data_blocks()
            ),
            1 => format!(
                "{} of {} blocks of data read; {} of {} definitions found",
                read.read.len(),
                read.data_blocks(),
                read.definitions,
                read.definitions_written
            ),
            _ => format!(
                "{} of {} blocks of data read; no close, found from the newest checkpoint",
                read.read.len(),
                read.data_blocks()
            ),
        };
        s.text(LEFT + ROW_W, y + ROW_H + 18.0, "end", "small", &note);
    }
    legend(
        &mut s,
        first_row + 2.0 * gap + ROW_H + 44.0,
        &[
            ("tint-blue", "data, read"),
            ("tint-green", "structure, read"),
            ("faint", "lost"),
            ("tint-accent", "damaged bytes"),
        ],
    );
    Ok(s.finish())
}

/// Whether a drawn block is one the index lists.
fn is_listed(part: Part) -> bool {
    !matches!(
        part,
        Part::Head | Part::Checkpoint | Part::FinalIndex | Part::Close | Part::Tail
    )
}
