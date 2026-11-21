//! `clock_map.svg`: the drive's clock map ([ww_tape_example::definitions]) from the board's µs since boot to the
//! host's ns since the Unix epoch, drawn as how far the host's time runs from the board's count read as host time.
//! A second, dashed line shows the map a host would write had it stepped its clock forward at minute 30: a second
//! segment, checked to keep the map increasing.

use anyhow::{Context, ensure};
use shrink_wrap::prelude::*;
use ww_tape::{Describe, MapSegment, Rounding, segments_increasing};
use ww_tape_example::{BOOT_UNIX_NS, CRASH_MINUTE, definitions};

use crate::OrDebug;
use crate::svg::Svg;

/// µs in a minute.
const US_PER_MINUTE: i64 = 60_000_000;
/// Minute of the run the drawn host steps its clock at, and by how many ns.
const STEP_MINUTE: i64 = 30;
const STEP_NS: i64 = 20_000_000;

/// The drive's map from the board's clock to the host's: its one segment.
pub fn drive_segment() -> anyhow::Result<MapSegment> {
    let (defs, n) = definitions(25, false, 1 << 20).or_debug()?;
    for (_, bytes) in defs.definitions() {
        if let Describe::Map(m) = Describe::from_ww_bytes(bytes).or_debug()?
            && m.from == n.board_clock
            && m.to == n.host_clock
        {
            let mut segments = m.segments.iter();
            let first = segments.next().context("a segment")?.or_debug()?;
            ensure!(segments.next().is_none(), "the drive's map has one segment");
            return Ok(first);
        }
    }
    anyhow::bail!("the drive's definitions hold a map from the board's clock to the host's")
}

/// The host's time at board µs `us` by `seg`, less the board's count read as host time, in ms.
fn offset_ms(seg: &MapSegment, us: i64) -> anyhow::Result<f64> {
    let ns = seg
        .map(us, Rounding::Floor)
        .context("the minute is mapped")?;
    Ok((ns - BOOT_UNIX_NS - us * 1_000) as f64 / 1e6)
}

/// `ns` since the Unix epoch as the time of day, UTC, to 0.1 ms.
fn time_of_day(ns: i64) -> String {
    let day_ns = ns.rem_euclid(86_400 * 1_000_000_000);
    let s = day_ns / 1_000_000_000;
    let frac = day_ns % 1_000_000_000 / 100_000;
    format!("{:02}:{:02}:{:02}.{frac:04}", s / 3600, s / 60 % 60, s % 60)
}

pub fn clock_map() -> anyhow::Result<String> {
    let seg = drive_segment()?;
    let step_us = STEP_MINUTE * US_PER_MINUTE;
    let before = MapSegment::new(seg.a0, Some(step_us), seg.b0, seg.slope, seg.error)
        .context("the segment before the step")?;
    let b_step = before.map(step_us - 1, Rounding::Floor).context("mapped")? + STEP_NS;
    let after = MapSegment::new(step_us, None, b_step, seg.slope, seg.error)
        .context("the segment after the step")?;
    ensure!(
        segments_increasing([before, after]),
        "a forward step keeps the map increasing"
    );

    let (left, top, w, h) = (64.0, 28.0, 560.0, 190.0);
    let (y_hi, y_lo) = (40.0, -80.0);
    let x = |minute: f64| left + w * minute / 60.0;
    let y = |ms: f64| top + h * (y_hi - ms) / (y_hi - y_lo);
    let end_ms = offset_ms(&seg, 60 * US_PER_MINUTE)?;
    let mut s = Svg::new(
        left + w + 40.0,
        top + h + 56.0,
        &format!(
            "The drive's clock map: the host's time runs {:.0} ms behind the board's count after an hour, a straight line of one segment; a forward step of the host's clock would start a second segment",
            -end_ms
        ),
    );
    // axes and grid
    for ms in [40.0, 20.0, 0.0, -20.0, -40.0, -60.0, -80.0] {
        s.line(
            (left, y(ms)),
            (left + w, y(ms)),
            if ms == 0.0 { "thin" } else { "faint" },
        );
        s.text(
            left - 8.0,
            y(ms) + 4.0,
            "end",
            "soft small mono",
            &format!("{ms:.0}"),
        );
    }
    for m in (0..=60).step_by(10) {
        let m = f64::from(m);
        s.line((x(m), top + h), (x(m), top + h + 4.0), "thin");
        s.text(
            x(m),
            top + h + 17.0,
            "middle",
            "soft small mono",
            &format!("{m:.0}"),
        );
    }
    s.text(
        left + w / 2.0,
        top + h + 36.0,
        "middle",
        "small",
        "board clock, minutes since boot",
    );
    s.text(left - 8.0, top - 12.0, "end", "small", "ms");
    s.text(
        left + 4.0,
        top - 12.0,
        "start",
        "small",
        "host time less the board's count read as host time",
    );

    // the drive's map
    let mut d = String::new();
    for m in 0..=60 {
        let ms = offset_ms(&seg, i64::from(m) * US_PER_MINUTE)?;
        d.push_str(&format!(
            "{}{:.1} {:.1} ",
            if m == 0 { "M" } else { "L" },
            x(f64::from(m)),
            y(ms)
        ));
    }
    s.path(&d, "line blue");
    s.text(
        x(1.0),
        y(-66.0),
        "start",
        "blue small",
        &format!(
            "the drive's map: one segment, slope {}/{} ns a µs, error ±{} µs",
            seg.slope.parts().0,
            seg.slope.parts().1,
            seg.error / 1_000
        ),
    );

    // the crash minute on the drive's map
    let crash_us = i64::from(CRASH_MINUTE) * US_PER_MINUTE;
    let crash_ms = offset_ms(&seg, crash_us)?;
    let crash_ns = seg.map(crash_us, Rounding::Floor).context("mapped")?;
    let (cx, cy) = (x(f64::from(CRASH_MINUTE)), y(crash_ms));
    s.circle((cx, cy), 3.0, "ink");
    s.text(
        cx - 6.0,
        cy + 18.0,
        "end",
        "small",
        &format!("minute {CRASH_MINUTE}: host {} UTC", time_of_day(crash_ns)),
    );

    // the stepped map
    let mut d = String::new();
    for m in STEP_MINUTE..=60 {
        let ms = offset_ms(&after, i64::from(u8::try_from(m)?) * US_PER_MINUTE)?;
        d.push_str(&format!(
            "{}{:.1} {:.1} ",
            if m == STEP_MINUTE { "M" } else { "L" },
            x(m as f64),
            y(ms)
        ));
    }
    s.path(&d, "line accent dash");
    let jump_from = y(offset_ms(&before, step_us - 1)?);
    let jump_to = y(offset_ms(&after, step_us)?);
    ensure!(jump_to > y(10.0), "the label stays above the stepped line");
    s.line(
        (x(STEP_MINUTE as f64), jump_from),
        (x(STEP_MINUTE as f64), jump_to),
        "thin dash",
    );
    s.text(
        x(60.0),
        y(33.0),
        "end",
        "accent small",
        &format!(
            "had the host stepped its clock {} ms forward at minute {STEP_MINUTE}:",
            STEP_NS / 1_000_000
        ),
    );
    s.text(
        x(60.0),
        y(24.0),
        "end",
        "accent small",
        "a second segment from there on",
    );
    Ok(s.finish())
}
