//! `tape_map.svg`: the drive's hour ([ww_tape_example]) at 1/100 of its data, read back from its pieces. Above, its
//! five tracks over the hour; below, the blocks that hold them on the same time line: records blocks that carry the
//! slow tracks together, and chunks of one track each. The cuts between the pieces cross both.

use anyhow::{Context, ensure};
use ww_tape::chunk::ColumnKind;
use ww_tape::samples::{SampleShape, Samples};
use ww_tape::{BlockKind, Element, ElementDesc, FLAG_REPEAT, FileBlocks};
use ww_tape_example::{CAN0, Drive, LOG, MODE, PHASE_A, TEMP};

use crate::OrDebug;
use crate::svg::Svg;

/// The scale the map draws the hour at.
const SCALE: u32 = 100;
/// Microseconds of the hour.
const HOUR_US: f64 = 3_600_000_000.0;
/// Columns of the plot: a pixel each.
const COLUMNS: usize = 600;

/// What the map reads from the drive's pieces.
#[derive(Default)]
struct Hour {
    /// Where the mode changes: µs, its item.
    modes: Vec<(i64, Vec<u8>)>,
    /// Log line times, µs.
    log: Vec<i64>,
    /// Temperatures: µs, hundredths of a degree.
    temp: Vec<(i64, i16)>,
    /// Records blocks: the µs of their first and last record.
    records: Vec<(i64, i64)>,
    /// Chunks of can0 and phase_a: first and last tick.
    frames: Vec<(i64, i64)>,
    samples: Vec<(i64, i64)>,
    /// phase_a's lowest and highest value in each column, mA.
    envelope: Vec<Option<(i16, i16)>>,
    /// Where each piece after the first starts: the time of its first chunk.
    cuts: Vec<i64>,
}

/// The column of the plot `us` falls in.
fn column(us: i64) -> usize {
    let us = u128::try_from(us.max(0)).unwrap_or(0);
    let c = us * COLUMNS as u128 / 3_600_000_000;
    usize::try_from(c).map_or(COLUMNS - 1, |c| c.min(COLUMNS - 1))
}

fn read() -> anyhow::Result<(Drive, Hour)> {
    let drive = Drive::scaled(SCALE).context("100 divides the drive's rates")?;
    let recording = drive.record()?;
    let mut h = Hour {
        envelope: vec![None; COLUMNS],
        ..Hour::default()
    };
    let mut values = Vec::new();
    for (seq, piece) in recording.pieces.iter().enumerate() {
        let mut first_chunk = None;
        let mut walk = FileBlocks::new(&piece[..]);
        while let Some(b) = walk.next_block() {
            let b = b.or_debug()?;
            match b.kind {
                BlockKind::Records => {
                    let mut span: Option<(i64, i64)> = None;
                    for r in b.records() {
                        let r = r.or_debug()?;
                        let Some(at) = r.at.map(|t| t.ticks) else {
                            continue;
                        };
                        match r.track.get() {
                            MODE => {
                                if h.modes.last().is_none_or(|m| m.1 != r.item) {
                                    h.modes.push((at, r.item.to_vec()));
                                }
                            }
                            LOG => h.log.push(at),
                            TEMP => {
                                let v = r.item.first_chunk::<2>().context("a temperature")?;
                                h.temp.push((at, i16::from_le_bytes(*v)));
                            }
                            _ => {}
                        }
                        if b.flags & FLAG_REPEAT == 0 {
                            span = Some(span.map_or((at, at), |(a, z)| (a.min(at), z.max(at))));
                        }
                    }
                    if let Some(span) = span {
                        h.records.push(span);
                    }
                }
                BlockKind::Events | BlockKind::Series => {
                    let chunk = b.chunk().context("a chunk's head")?;
                    let head = chunk.head();
                    first_chunk.get_or_insert(head.first_tick);
                    let span = (head.first_tick, head.last_tick);
                    match head.track_number.get() {
                        CAN0 => h.frames.push(span),
                        PHASE_A => {
                            h.samples.push(span);
                            let col = chunk
                                .find(ColumnKind::Samples, 0)
                                .context("the samples column")?;
                            let count = usize::try_from(head.count)?;
                            let samples = Samples::of(
                                &col,
                                SampleShape::scalar(ElementDesc::of(Element::I16)),
                                count,
                            )
                            .or_debug()?;
                            values.resize(count, 0i16);
                            samples.decode(0, &mut values).or_debug()?;
                            let step = (head.last_tick - head.first_tick) as f64
                                / (count.max(2) - 1) as f64;
                            for (i, &v) in values.iter().enumerate() {
                                let at = head.first_tick + (i as f64 * step) as i64;
                                let e = h.envelope.get_mut(column(at)).context("a column")?;
                                *e = Some(e.map_or((v, v), |(lo, hi)| (lo.min(v), hi.max(v))));
                            }
                        }
                        other => anyhow::bail!("a chunk of track {other}"),
                    }
                }
                _ => {}
            }
        }
        if seq > 0 {
            h.cuts.push(first_chunk.context("a piece holds chunks")?);
        }
    }
    ensure!(
        h.modes.len() == 3,
        "idle, run and fault: {:?}",
        h.modes.len()
    );
    Ok((drive, h))
}

pub fn tape_map() -> anyhow::Result<String> {
    let (drive, h) = read()?;
    let (left, width) = (112.0, COLUMNS as f64);
    let x_of = |us: i64| left + us as f64 / HOUR_US * width;
    // the lanes of the tracks, then the rows of blocks: top and height
    let mode_y = 38.0;
    let log_y = 62.0;
    let temp_y = 84.0;
    let can_y = 126.0;
    let phase_y = 150.0;
    let phase_h = 64.0;
    let blocks_y = 262.0;
    let row_h = 16.0;
    let rows_y = [blocks_y, blocks_y + 24.0, blocks_y + 48.0];
    let axis_y = rows_y[2] + row_h + 14.0;
    let mut s = Svg::new(
        left + width + 20.0,
        axis_y + 24.0,
        "The drive's hour: its mode, log lines, temperature, CAN frames and phase current above; the records blocks and chunks that hold them below, on the same time line, cut into four pieces",
    );

    // the cuts between the pieces, across everything
    for (i, &cut) in h.cuts.iter().enumerate() {
        let x = x_of(cut);
        s.line((x, 16.0), (x, blocks_y - 30.0), "thin dash");
        s.line(
            (x, blocks_y - 6.0),
            (x, rows_y[2] + row_h + 4.0),
            "thin dash",
        );
        s.text(
            x + 4.0,
            12.0,
            "start",
            "small soft",
            &format!("piece {}", i + 1),
        );
    }
    s.text(left + 4.0, 12.0, "start", "small soft", "piece 0");

    let lane = |s: &mut Svg, y: f64, h: f64, name: &str| {
        s.text(left - 10.0, y + h / 2.0 + 4.0, "end", "small", name);
    };

    // mode: a state track, a segment per mode
    lane(&mut s, mode_y, 18.0, "mode");
    for (i, (at, item)) in h.modes.iter().enumerate() {
        let end = h.modes.get(i + 1).map_or(HOUR_US as i64, |m| m.0);
        let (x0, x1) = (x_of(*at), x_of(end));
        let class = match i {
            0 => "tint-ink",
            1 => "tint-green",
            _ => "tint-accent",
        };
        s.rect((x0 + 0.5, mode_y), (x1 - x0 - 1.0, 18.0), 2.0, class);
        // the item's first word: idle, run, fault
        let text = String::from_utf8_lossy(item);
        let word = text.split([',', ':']).next().unwrap_or_default();
        if x1 - x0 > 6.6 * word.len() as f64 + 8.0 {
            s.text((x0 + x1) / 2.0, mode_y + 13.0, "middle", "small", word);
        } else {
            s.text(x0, mode_y - 4.0, "start", "small", word);
        }
    }

    // log: a tick per line
    lane(&mut s, log_y, 12.0, "log");
    let mut d = String::new();
    for &at in &h.log {
        d.push_str(&format!("M{:.1} {log_y:.1} v12 ", x_of(at)));
    }
    s.path(d.trim_end(), "blue");

    // temp: 20 °C to 70 °C
    lane(&mut s, temp_y, 30.0, "temp");
    let y_temp = |c: i16| temp_y + 30.0 - (f64::from(c) / 100.0 - 20.0) / 50.0 * 30.0;
    for &(at, c) in &h.temp {
        s.circle((x_of(at), y_temp(c)), 1.6, "blue");
    }

    // can0: frames all through the hour
    lane(&mut s, can_y, 12.0, "can0");
    for &(a, z) in &h.frames {
        s.rect(
            (x_of(a), can_y),
            ((x_of(z) - x_of(a)).max(0.6), 12.0),
            0.0,
            "tint-blue",
        );
    }

    // phase_a: the lowest and highest sample of each column
    lane(&mut s, phase_y, phase_h, "phase_a");
    let mid = phase_y + phase_h / 2.0;
    let y_ma = |v: i16| mid - f64::from(v) / 45_000.0 * phase_h / 2.0;
    let mut d = String::new();
    for (c, e) in h.envelope.iter().enumerate() {
        if let Some((lo, hi)) = e {
            let x = left + c as f64 + 0.5;
            d.push_str(&format!(
                "M{x:.1} {:.1} V{:.1} ",
                y_ma(*hi),
                y_ma(*lo).max(y_ma(*hi) + 0.8)
            ));
        }
    }
    s.path(d.trim_end(), "blue");

    // the blocks that hold them
    s.text(
        left,
        blocks_y - 14.0,
        "start",
        "small soft",
        &format!(
            "the blocks that hold them: a records block, a chunk of can0 and one of phase_a every {} s",
            drive.block_ms / 1000
        ),
    );
    let block_us = i64::from(drive.block_ms) * 1000;
    let rows: [(&str, Vec<(i64, i64)>); 3] = [
        (
            "records blocks",
            h.records
                .iter()
                .map(|&(a, _)| {
                    let start = a.div_euclid(block_us) * block_us;
                    (start, start + block_us)
                })
                .collect(),
        ),
        ("events chunks", h.frames.clone()),
        ("series chunks", h.samples.clone()),
    ];
    for ((name, spans), y) in rows.iter().zip(rows_y) {
        lane(&mut s, y, row_h, name);
        for &(a, z) in spans {
            let (x0, x1) = (x_of(a), x_of(z));
            s.rect(
                (x0 + 0.5, y),
                ((x1 - x0 - 1.0).max(0.8), row_h),
                1.5,
                "tint-blue",
            );
        }
    }

    // the time axis
    s.line((left, axis_y - 6.0), (left + width, axis_y - 6.0), "thin");
    for m in (0..=60).step_by(10) {
        let x = x_of(i64::from(m) * 60_000_000);
        s.line((x, axis_y - 9.0), (x, axis_y - 3.0), "thin");
        s.text(
            x,
            axis_y + 10.0,
            "middle",
            "small soft mono",
            &format!("{m} min"),
        );
    }
    Ok(s.finish())
}
