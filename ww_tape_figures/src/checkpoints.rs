//! `checkpoints.svg`: the drive's first piece, closed, at 1/1000 of its data with a checkpoint every 3 KiB
//! ([crate::crash::drive]): the checkpoints pointing back to each other, the final index after the data, the close
//! naming the final index and the newest checkpoint, and the tail naming the close. Every offset is read back by
//! [TapeReader] from the piece.

use anyhow::{Context, ensure};
use ww_tape::TapeReader;

use crate::OrDebug;
use crate::crash::{LEFT, Part, ROW_H, ROW_W, View, drive, kib, legend, row, spans};
use crate::svg::Svg;

/// An arrow head at `to`, pointing from `from`.
fn head(s: &mut Svg, from: (f64, f64), to: (f64, f64), class: &str) {
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    let n = dx.hypot(dy).max(1e-9);
    let (ux, uy) = (dx / n, dy / n);
    let (bx, by) = (to.0 - 7.0 * ux, to.1 - 7.0 * uy);
    s.path(
        &format!(
            "M{:.1} {:.1} L{:.1} {:.1} L{:.1} {:.1} Z",
            to.0,
            to.1,
            bx - 3.5 * uy,
            by + 3.5 * ux,
            bx + 3.5 * uy,
            by - 3.5 * ux
        ),
        &format!("{class} head"),
    );
}

pub fn checkpoints() -> anyhow::Result<String> {
    let drive = drive()?;
    let recording = drive.record().or_debug()?;
    let piece = recording.pieces.first().context("the first piece")?;
    let blocks = spans(piece)?;
    let len = u64::try_from(piece.len())?;

    let r = TapeReader::open(&piece[..]).or_debug()?;
    let info = r.pieces().first().context("the piece")?;
    let tail = info.tail.as_ref().context("a closed piece has a tail")?;
    ensure!(tail.piece_len == len, "the tail gives the piece's length");
    let close_at = info.close_offset.context("the close's offset")?;
    ensure!(tail.close_offset == close_at, "the tail names the close");
    let (close, _) = info.close.as_ref().context("the close record")?;
    let final_at = blocks
        .iter()
        .find(|b| b.part == Part::FinalIndex)
        .context("a final index")?;
    ensure!(
        close.final_index == Some(final_at.at),
        "the close names the final index"
    );
    let newest = blocks
        .iter()
        .filter(|b| b.part == Part::Checkpoint)
        .max_by_key(|b| b.at)
        .context("a checkpoint")?;
    ensure!(
        close.checkpoint == Some(newest.at),
        "the close names the newest checkpoint"
    );
    let checkpoints = blocks.iter().filter(|b| b.part == Part::Checkpoint).count();

    let v = View::new(0, len);
    let (top, y) = (20.0, 92.0);
    let mut s = Svg::new(
        LEFT * 2.0 + ROW_W,
        y + ROW_H + 136.0,
        &format!(
            "A closed piece of {}: {checkpoints} checkpoints, each pointing back to the one before; a final index after the data; the close naming the final index and the newest checkpoint; the tail naming the close",
            kib(len)
        ),
    );
    s.text(
        LEFT,
        top,
        "start",
        "strong",
        &format!("the drive's piece 0, closed: {}", kib(len)),
    );
    s.text(
        LEFT + ROW_W,
        top,
        "end",
        "soft small",
        "arcs above: each checkpoint points back to the one before",
    );
    row(&mut s, &blocks, len, v, y, None);
    for b in blocks.iter().filter(|b| b.part == Part::Checkpoint) {
        let Some(prev) = b.prev else { continue };
        let prev_len = blocks.iter().find(|p| p.at == prev).map_or(0, |p| p.len);
        let (xa, xb) = (v.x(b.at + b.len / 2), v.x(prev + prev_len / 2));
        let lift = 14.0 + (xa - xb) * 0.35;
        s.path(
            &format!(
                "M{xa:.1} {:.1} C{xa:.1} {:.1} {xb:.1} {:.1} {xb:.1} {:.1}",
                y - 2.0,
                y - lift,
                y - lift,
                y - 2.0
            ),
            "thin",
        );
        head(&mut s, (xb + 0.5, y - 9.0), (xb, y - 2.0), "thin");
    }

    // below: the tail names the close, the close names the final index and the newest checkpoint
    let below = y + ROW_H + 2.0;
    let x_tail = v.x(len - 82);
    let x_close = v.x(close_at + 40);
    let x_final = v.x(final_at.at + final_at.len / 2);
    let x_newest = v.x(newest.at + newest.len / 2);
    let arc = |s: &mut Svg, xa: f64, xb: f64, drop: f64, class: &str| {
        s.path(
            &format!(
                "M{xa:.1} {below:.1} C{xa:.1} {:.1} {xb:.1} {:.1} {xb:.1} {below:.1}",
                below + drop,
                below + drop
            ),
            class,
        );
        head(s, (xb - 0.5, below + 8.0), (xb, below), class);
    };
    arc(&mut s, x_tail, x_close, 22.0, "accent-line");
    arc(&mut s, x_close, x_final, 40.0, "accent-line");
    arc(&mut s, x_close, x_newest, 64.0, "accent-line");
    s.text(x_tail, below + 12.0, "start", "small", "tail");
    let notes = [
        (
            "accent small",
            format!(
                "arcs below, from the right: the tail, the piece's last {} bytes, names the close;",
                ww_tape::piece::TAIL_LEN
            ),
        ),
        (
            "accent small",
            "the close names the final index and the newest checkpoint".to_owned(),
        ),
        (
            "soft small",
            "the final index lists every block of the piece, sorted by track".to_owned(),
        ),
    ];
    for (i, (class, note)) in notes.iter().enumerate() {
        s.text(LEFT, below + 86.0 + 15.0 * i as f64, "start", class, note);
    }
    legend(
        &mut s,
        top + 16.0,
        &[
            ("tint-green", "head, final index, close, tail"),
            ("tint-purple", "puts"),
            ("tint-blue", "data"),
            ("tint-amber", "checkpoint"),
        ],
    );
    Ok(s.finish())
}
