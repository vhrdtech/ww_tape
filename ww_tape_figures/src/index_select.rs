//! `index_select.svg`: the index entries of `phase_a` and `can0` in the last piece of the drive's hour
//! ([ww_tape_example]) at 1/1000 of its data, each over the ticks it covers, and the entries the query of the chapter
//! "Reading a tape" keeps: [ww_tape_example::fault::find] for `phase_a`, the same ticks for `can0`.

use anyhow::{Context, ensure};
use ww_tape::index::IndexEntry;
use ww_tape::{BlockKind, TapeReader};
use ww_tape_example::fault::{fault_us, find, open};
use ww_tape_example::{Drive, key};

use crate::OrDebug;
use crate::svg::Svg;

/// The scale the drive is drawn at: a chunk covers a minute.
const SCALE: u32 = 1000;
/// Microseconds of a minute.
const MINUTE: i64 = 60_000_000;

/// `m:ss` of `us`.
fn minutes(us: i64) -> String {
    format!("{}:{:02}", us / MINUTE, us / 1_000_000 % 60)
}

pub fn index_select() -> anyhow::Result<String> {
    let drive = Drive::scaled(SCALE).context("1000 divides the drive's rates")?;
    let bytes = drive.record()?.concat();
    let (reader, opened): (TapeReader<&[u8]>, _) = open(&bytes[..]).or_debug()?;
    let piece = opened.piece;
    let found = find(&reader, piece).or_debug()?;
    let kept = |e: &IndexEntry| found.chunks.contains(e);
    let rows: [(&str, Vec<IndexEntry>); 2] = [
        (
            "phase_a",
            reader
                .track_entries(piece, key("phase_a"))
                .or_debug()?
                .into_iter()
                .filter(|e| e.kind == BlockKind::Series)
                .collect(),
        ),
        (
            "can0",
            reader
                .track_entries(piece, key("can0"))
                .or_debug()?
                .into_iter()
                .filter(|e| e.kind == BlockKind::Events)
                .collect(),
        ),
    ];
    let fault = fault_us();
    let (from, to) = (fault - 1_000_000, fault + 1_000_000);
    let can_kept = |e: &IndexEntry| e.first_tick < to && e.last_tick >= from;
    ensure!(
        found.chunks.len() == 2,
        "two chunks of phase_a around the fault"
    );

    let all = rows.iter().flat_map(|(_, r)| r.iter());
    let t0 = all.clone().map(|e| e.first_tick).min().context("entries")?;
    let t1 = all.map(|e| e.last_tick).max().context("entries")?;
    // the axis runs over whole minutes
    let (t0, t1) = (t0 / MINUTE * MINUTE, (t1 / MINUTE + 1) * MINUTE);
    let (left, width) = (88.0, 570.0);
    let x = |t: i64| left + (t - t0) as f64 / (t1 - t0) as f64 * width;
    let mut s = Svg::new(
        680.0,
        240.0,
        "The index entries of phase_a and can0 in the drive's last piece, one per minute of chunks, each drawn over the ticks it covers; a query for a second either side of the fault at minute 52 keeps the two entries of each track that overlap it and reads only their blocks",
    );
    s.text(
        20.0,
        22.0,
        "start",
        "strong",
        &format!("index entries of piece {piece}, the last, by ticks"),
    );
    // the query's window
    let (qx0, qx1) = (x(from), x(to));
    s.rect((qx0, 34.0), (qx1 - qx0, 124.0), 0.0, "tint-accent");
    s.line((x(fault), 34.0), (x(fault), 158.0), "accent-line");
    s.text(
        x(fault),
        172.0,
        "middle",
        "accent small",
        "52:00, the fault",
    );
    for (row, (name, entries)) in rows.iter().enumerate() {
        let y = 52.0 + row as f64 * 52.0;
        s.text(20.0, y + 15.0, "start", "small", name);
        for e in entries {
            let keep = if row == 0 { kept(e) } else { can_kept(e) };
            let (x0, x1) = (x(e.first_tick) + 1.0, x(e.last_tick) - 1.0);
            let class = if keep { "tint-blue" } else { "faint" };
            s.rect((x0, y), ((x1 - x0).max(2.0), 22.0), 2.0, class);
            if keep {
                s.rect(
                    (x0 - 2.0, y - 2.0),
                    ((x1 - x0).max(2.0) + 4.0, 26.0),
                    3.0,
                    "accent-line",
                );
            }
        }
    }
    // minutes along the axis
    let axis = 190.0;
    s.line((left, axis), (left + width, axis), "thin");
    let mut t = t0;
    while t <= t1 {
        s.line((x(t), axis - 3.0), (x(t), axis + 3.0), "thin");
        s.text(x(t), axis + 16.0, "middle", "soft small", &minutes(t));
        t += 5 * MINUTE;
    }
    s.text(
        20.0,
        232.0,
        "start",
        "soft small",
        "outlined: the entries the query keeps; only their blocks are read",
    );
    Ok(s.finish())
}
