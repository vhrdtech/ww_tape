//! The figures of the book's Part III, writing, drawn from the drive's recording ([ww_tape_example]):
//!
//! - `deferred.svg`: a stretch of an aligned file, the drive's tracks together and `phase_a` alone: where the summary
//!   blocks land and the zeros in each chunk's head that put its payload on 4 KiB.
//! - `linear_group.svg`: the first 128 frame starts of `can0`, the line its group record fits, and the residuals
//!   bit-packed at the group's width.
//! - `delta_group.svg`: 128 samples of `phase_a`, their differences, and the differences less their minimum at the
//!   group's width.
//! - `sync_loss.svg`: three seconds of the drive's file and where each sync policy synced, and what a crash in the
//!   third second loses under each.
//! - `cut.svg`: the end of piece 0 and the start of piece 1 of the drive's chain at 1/1000 of its data.

use anyhow::{Context, ensure};
use ww_tape::piece::close_padding;
use ww_tape::times::TimeMode;
use ww_tape::{BlockKind, ColumnKind, FileBlocks, Layout, Recovery, SyncPolicy, TapeReader};
use ww_tape_example::writing::{SYNC_POLICIES, phase_values, series_tape, sync_offsets};
use ww_tape_example::{Act, CAN0, Drive, PHASE_A};

use crate::OrDebug;
use crate::crash::{LEFT, Part, ROW_H, ROW_W, View, drive, kib, legend, row, spans};
use crate::svg::Svg;

/// A block of an aligned file and where its payload starts, for a chunk.
struct Placed {
    at: u64,
    len: u64,
    part: Part,
    /// Bytes of the chunk's head, its zeros included: the payload starts here.
    head: Option<u64>,
}

/// The blocks of `bytes` with each chunk's head length.
fn placed(bytes: &[u8]) -> anyhow::Result<Vec<Placed>> {
    let parts = spans(bytes)?;
    let mut heads = Vec::new();
    let mut walk = FileBlocks::with_recovery(bytes, Recovery::Prefix);
    while let Some(Ok(b)) = walk.next_block() {
        heads.push(match b.kind {
            BlockKind::Events | BlockKind::Series => Some(u64::try_from(
                b.chunk().context("a chunk reads")?.payload_offset(),
            )?),
            _ => None,
        });
    }
    ensure!(heads.len() == parts.len(), "one head per block");
    Ok(parts
        .into_iter()
        .zip(heads)
        .map(|(p, head)| Placed {
            at: p.at,
            len: p.len,
            part: p.part,
            head,
        })
        .collect())
}

/// The bytes of a chunk's head past its fixed fields and column table: the zeros before the payload. A chunk of an
/// unaligned file has none, so its head is the shortest; the zeros are the difference.
fn zeros(aligned: &Placed, unaligned_head: u64) -> u64 {
    aligned.head.map_or(0, |h| h.saturating_sub(unaligned_head))
}

/// Draw `blocks` within `v` on a row at `y`: a chunk's head zeros hatched, its payload in its colour; 4 KiB lines.
fn aligned_row(s: &mut Svg, blocks: &[Placed], heads: &[u64], v: View, y: f64, from: u64, to: u64) {
    for (b, h) in blocks.iter().zip(heads) {
        if b.at + b.len <= from || b.at >= to {
            continue;
        }
        let (x0, x1) = (v.x(b.at), v.x(b.at + b.len));
        s.rect(
            (x0 + 0.5, y),
            ((x1 - x0 - 1.0).max(1.0), ROW_H),
            1.0,
            b.part.class(),
        );
        let z = zeros(b, *h);
        let mut label_from = x0;
        if z > 0 {
            let head = b.head.unwrap_or(0);
            let (z0, z1) = (v.x(b.at + head - z), v.x(b.at + head));
            s.rect((z0, y), ((z1 - z0).max(1.0), ROW_H), 0.0, "faint hatch");
            label_from = z1;
        }
        let name = match b.part {
            Part::Samples => "samples",
            Part::Frames => "frames",
            Part::Summary => "summary",
            Part::Log => "log",
            Part::Puts => "puts",
            Part::Checkpoint => "checkpoint",
            _ => "",
        };
        let w = x1 - label_from;
        if w >= 6.4 * name.len() as f64 + 6.0 {
            s.text(label_from + w / 2.0, y + 19.5, "middle", "small", name);
        } else if !name.is_empty() && b.part != Part::Puts {
            s.text((x0 + x1) / 2.0, y - 4.0, "middle", "soft small", name);
        }
    }
    let mut grid = from.next_multiple_of(4096);
    while grid <= to {
        let x = v.x(grid);
        s.line((x, y - 4.0), (x, y + ROW_H + 4.0), "thin dash");
        grid += 4096;
    }
}

/// The head length each chunk of `blocks` would have unaligned: its fixed fields and column table.
fn unaligned_heads(blocks: &[Placed], unaligned: &[Placed]) -> Vec<u64> {
    let mut chunk_heads = unaligned.iter().filter_map(|b| b.head);
    blocks
        .iter()
        .map(|b| match b.head {
            Some(_) => chunk_heads.next().unwrap_or(0),
            None => 0,
        })
        .collect()
}

/// The stretch from the block after series chunk `k` to 4 KiB past the next series chunk's payload start.
fn window(blocks: &[Placed], k: usize) -> anyhow::Result<(u64, u64, u64)> {
    let mut series = blocks.iter().filter(|b| b.part == Part::Samples);
    let this = series.nth(k).context("series chunk k")?;
    let next = series.next().context("series chunk k + 1")?;
    let payload = next.at + next.head.context("a chunk's head")?;
    Ok((this.at + this.len, payload + 4096, payload))
}

/// The number, from 4 on, of the first series chunk whose next block is of a part `next` takes.
fn nth_series_where(blocks: &[Placed], next: impl Fn(Part) -> bool) -> Option<usize> {
    let mut n = 0;
    for w in blocks.windows(2) {
        let (a, b) = (w.first()?, w.get(1)?);
        if a.part == Part::Samples {
            if n >= 4 && next(b.part) {
                return Some(n);
            }
            n += 1;
        }
    }
    None
}

/// `deferred.svg`.
pub fn deferred() -> anyhow::Result<String> {
    let minute = Drive::full().minutes(1);
    let aligned = Drive {
        layout: Layout::ALIGNED,
        ..minute
    };
    let mixed = placed(
        aligned
            .record()
            .or_debug()?
            .pieces
            .first()
            .context("a piece")?,
    )?;
    let mixed_plain = placed(
        minute
            .record()
            .or_debug()?
            .pieces
            .first()
            .context("a piece")?,
    )?;
    let alone = placed(&series_tape(&aligned).or_debug()?)?;
    let alone_plain = placed(&series_tape(&minute).or_debug()?)?;
    let chunks = |b: &[Placed]| b.iter().filter(|b| b.head.is_some()).count();
    ensure!(
        chunks(&mixed) == chunks(&mixed_plain) && chunks(&alone) == chunks(&alone_plain),
        "the aligned and the unaligned file hold the same chunks"
    );
    let mixed_heads = unaligned_heads(&mixed, &mixed_plain);
    let alone_heads = unaligned_heads(&alone, &alone_plain);
    for (b, h) in mixed
        .iter()
        .chain(&alone)
        .zip(mixed_heads.iter().chain(&alone_heads))
    {
        if let Some(head) = b.head {
            ensure!(
                (b.at + head) % 4096 == 0,
                "an aligned chunk's payload starts on 4 KiB"
            );
            ensure!(head >= *h, "zeros only lengthen a head");
        }
    }
    // a second where a summary of phase_a comes due: in the drive's file it follows the series chunk, in phase_a
    // alone it precedes the next one, in the gap
    let km = nth_series_where(&mixed, |next| next == Part::Summary)
        .context("a summary after a series chunk in the drive's file")?;
    let (m0, m1, mpay) = window(&mixed, km)?;
    let ka = nth_series_where(&alone, |next| next == Part::Summary)
        .context("a summary after a series chunk in phase_a alone")?;
    let (a0, a1, apay) = window(&alone, ka)?;

    let width = (m1 - m0).max(a1 - a0);
    let vm = View::new(m0, m0 + width);
    let va = View::new(a0, a0 + width);
    let (top, y1, y2) = (20.0, 70.0, 176.0);
    let zeros_in = |blocks: &[Placed], heads: &[u64], from: u64, to: u64| {
        blocks
            .iter()
            .zip(heads)
            .filter(|(b, _)| b.at >= from && b.at < to)
            .map(|(b, h)| zeros(b, *h))
            .sum::<u64>()
    };
    let (mz, az) = (
        zeros_in(&mixed, &mixed_heads, m0, mpay),
        zeros_in(&alone, &alone_heads, a0, apay),
    );
    let mut s = Svg::new(
        LEFT * 2.0 + ROW_W,
        y2 + ROW_H + 92.0,
        &format!(
            "Two stretches of aligned files: in the drive's file the next second's put of the mode places the waiting summary before the next gap, and the gap before the series payload holds {mz} zero bytes; in a file of phase_a alone the summary fills the gap and {az} zero bytes remain"
        ),
    );
    s.text(
        LEFT,
        top,
        "start",
        "strong",
        "aligned files: every chunk's payload starts on a 4 KiB line",
    );
    s.text(
        LEFT,
        y1 - 18.0,
        "start",
        "small",
        "the drive's file: the mode's put each second places the summary first",
    );
    aligned_row(&mut s, &mixed, &mixed_heads, vm, y1, m0, m0 + width);
    s.text(
        vm.x(mpay),
        y1 + ROW_H + 18.0,
        "middle",
        "accent small",
        &format!("payload on 4 KiB; {mz} bytes of zeros before it"),
    );
    s.text(
        LEFT,
        y2 - 18.0,
        "start",
        "small",
        "phase_a alone: the summary waits for the gap and fills it",
    );
    aligned_row(&mut s, &alone, &alone_heads, va, y2, a0, a0 + width);
    s.text(
        va.x(apay),
        y2 + ROW_H + 18.0,
        "middle",
        "accent small",
        &format!("payload on 4 KiB; {az} bytes of zeros before it"),
    );
    legend(
        &mut s,
        y2 + ROW_H + 40.0,
        &[
            ("tint-blue", "data"),
            ("tint-teal", "summary"),
            ("faint hatch", "zeros in a chunk's head"),
            ("thin dash", "4 KiB lines"),
        ],
    );
    s.text(
        LEFT,
        y2 + ROW_H + 74.0,
        "soft small",
        "start",
        &format!(
            "each row: {} of the drive's first minute at full rate",
            kib(width)
        ),
    );
    Ok(s.finish())
}

/// The first chunk of `track` from second 60 on, in the drive's run at full rate: its head and columns.
/// A chunk's head and its columns, as the recorder hands them over.
type Columns = (
    ww_tape::chunk::ChunkHead,
    Vec<(ww_tape::ColumnSpec, Vec<u8>)>,
);

fn chunk_at_60_s(track: u32) -> anyhow::Result<Columns> {
    for act in Drive::full().minutes(2).acts().or_debug()? {
        if let Act::Chunk { head, columns } = act.or_debug()?
            && head.track_number == track
            && head.first_tick >= 60_000_000
        {
            return Ok((*head, columns));
        }
    }
    anyhow::bail!("a chunk of track {track} from second 60 on")
}

/// A plot area: `x` from item 0 to 127, `y` from `lo` to `hi`, at `top` with `height`.
struct Plot {
    top: f64,
    height: f64,
    lo: f64,
    hi: f64,
}

impl Plot {
    fn x(j: usize) -> f64 {
        LEFT + 40.0 + j as f64 * (ROW_W - 50.0) / 127.0
    }
    fn y(&self, v: f64) -> f64 {
        self.top + self.height - (v - self.lo) / (self.hi - self.lo).max(1e-9) * self.height
    }
    fn frame(&self, s: &mut Svg, label: &str, lo: &str, hi: &str) {
        s.line(
            (Self::x(0), self.top + self.height),
            (Self::x(127), self.top + self.height),
            "thin",
        );
        s.line(
            (Self::x(0), self.top),
            (Self::x(0), self.top + self.height),
            "thin",
        );
        s.text(LEFT, self.top - 14.0, "start", "small", label);
        s.text(Self::x(0) - 4.0, self.top + 4.0, "end", "soft small", hi);
        s.text(
            Self::x(0) - 4.0,
            self.top + self.height,
            "end",
            "soft small",
            lo,
        );
    }
}

/// `linear_group.svg`.
pub fn linear_group() -> anyhow::Result<String> {
    let (head, columns) = chunk_at_60_s(CAN0)?;
    ensure!(head.time_mode == 1, "can0's starts are linear");
    let column = columns
        .iter()
        .find(|(c, _)| c.kind == ColumnKind::Starts)
        .map(|(_, b)| b.as_slice())
        .context("a starts column")?;
    let rec = column.get(..25).context("group 0's record")?;
    let first = i64::from_le_bytes(rec.get(0..8).context("first")?.try_into()?);
    let slope = i64::from_le_bytes(rec.get(8..16).context("slope")?.try_into()?);
    let rmin = i32::from_le_bytes(rec.get(16..20).context("rmin")?.try_into()?);
    let width = u32::from(*rec.get(20).context("width")?);
    let period = 1_000_000 / u64::from(Drive::full().can_hz);
    let starts: Vec<i64> = (head.first_item..head.first_item + 128)
        .map(|k| ww_tape_example::frame(k, period).0)
        .collect();
    ensure!(
        starts.first() == Some(&first),
        "the record's first tick is the first start"
    );
    let line = |j: i64| first + i64::from(rmin) + ((j * slope) >> 16);
    let residuals: Vec<i64> = starts.iter().zip(0..).map(|(t, j)| t - line(j)).collect();
    ensure!(
        residuals.iter().all(|r| (0..1i64 << width).contains(r)),
        "every residual fits the group's width"
    );
    // the encoder's own bytes for these starts agree
    let (mode, ours) = ww_tape::times::encode_starts(&starts).context("encoded")?;
    ensure!(
        mode == TimeMode::Linear && ours.get(..25) == Some(rec),
        "the record is the encoder's"
    );

    let jitter = Plot {
        top: 70.0,
        height: 120.0,
        lo: -50.0,
        hi: (1i64 << width) as f64 + 50.0,
    };
    // how late the latest frame of the group is against its nominal tick, k periods after 0
    let late_us = (head.first_item..head.first_item + 128)
        .zip(&starts)
        .map(|(k, t)| t - i64::try_from(k * period).unwrap_or(i64::MAX))
        .max()
        .context("a start")?;
    let mut s = Svg::new(
        LEFT * 2.0 + ROW_W,
        jitter.top + jitter.height + 92.0,
        &format!(
            "128 CAN frame starts of the drive, about 1 ms apart and up to {:.1} ms late: the group record's line runs {:.3} µs per frame, and each start is stored as its distance above the line, {width} bits each",
            late_us as f64 / 1000.0,
            slope as f64 / 65536.0
        ),
    );
    s.text(
        LEFT,
        20.0,
        "start",
        "strong",
        &format!(
            "can0, frames {} to {}: one group of a linear starts column",
            head.first_item,
            head.first_item + 127
        ),
    );
    s.text(LEFT, 38.0, "start", "small", &format!(
        "group record: first tick {first} µs, slope {:.3} µs per frame, residual minimum {rmin}, width {width} bits",
        slope as f64 / 65536.0
    ));
    jitter.frame(
        &mut s,
        "each start less the line: the residual the column stores",
        "0",
        &format!("2^{width}"),
    );
    let top_band = jitter.y((1i64 << width) as f64);
    s.rect(
        (Plot::x(0), top_band),
        (Plot::x(127) - Plot::x(0), jitter.y(0.0) - top_band),
        0.0,
        "tint-amber",
    );
    for (j, r) in residuals.iter().enumerate() {
        s.circle((Plot::x(j), jitter.y(*r as f64)), 2.2, "blue");
    }
    s.line(
        (Plot::x(0), jitter.y(0.0)),
        (Plot::x(127), jitter.y(0.0)),
        "accent-line",
    );
    s.text(
        Plot::x(127),
        jitter.y(0.0) + 14.0,
        "end",
        "accent small",
        "the line: first tick + residual minimum + j × slope",
    );
    let raw = 64 * 128;
    let packed = 128 * width + 25 * 8;
    s.text(
        LEFT,
        jitter.top + jitter.height + 32.0,
        "start",
        "small",
        &format!("stored: a 25-byte record and 128 × {width} bits, {} bytes, against {} bytes of raw 64-bit ticks", packed / 8, raw / 8),
    );
    s.text(LEFT, jitter.top + jitter.height + 52.0, "start", "soft small", "item j of the group starts at the line plus its residual; no other item is decoded to read it");
    Ok(s.finish())
}

/// `delta_group.svg`.
pub fn delta_group() -> anyhow::Result<String> {
    let (head, columns) = chunk_at_60_s(PHASE_A)?;
    let drive = Drive::full();
    let values = phase_values(&drive, head.first_item, 128);
    let diffs: Vec<i64> = values
        .windows(2)
        .filter_map(|w| Some(i64::from(*w.get(1)?) - i64::from(*w.first()?)))
        .collect();
    let min = *diffs.iter().min().context("differences")?;
    let max = *diffs.iter().max().context("differences")?;
    let width = 64 - (max - min).unsigned_abs().leading_zeros();
    let first = *values.first().context("a sample")?;
    // the column's own group 0: after the table of group ends, the first value, the minimum and the width
    let column = &columns.first().context("a samples column")?.1;
    let groups = (head.count as usize).div_ceil(128);
    let g0 = column.get(groups * 4..).context("group 0")?;
    let stored_first = i16::from_le_bytes(g0.get(0..2).context("first")?.try_into()?);
    let stored_min = i16::from_le_bytes(g0.get(2..4).context("min")?.try_into()?);
    let stored_width = u32::from(*g0.get(4).context("width")?);
    ensure!(
        (stored_first, i64::from(stored_min), stored_width) == (first, min, width),
        "the drawn group is the column's group 0: {stored_first} {stored_min} {stored_width}"
    );
    let (vlo, vhi) = values
        .iter()
        .fold((i16::MAX, i16::MIN), |(lo, hi), v| (lo.min(*v), hi.max(*v)));
    let p1 = Plot {
        top: 70.0,
        height: 90.0,
        lo: f64::from(vlo),
        hi: f64::from(vhi),
    };
    let p2 = Plot {
        top: 210.0,
        height: 70.0,
        lo: min as f64,
        hi: max as f64,
    };
    let mut s = Svg::new(
        LEFT * 2.0 + ROW_W,
        p2.top + p2.height + 76.0,
        &format!(
            "128 samples of the drive's phase current, their differences from {min} to {max} mA, and those less the minimum, each stored in {width} bits instead of 16"
        ),
    );
    s.text(
        LEFT,
        20.0,
        "start",
        "strong",
        &format!(
            "phase_a, samples {} to {}: one group of a delta column",
            head.first_item,
            head.first_item + 127
        ),
    );
    s.text(
        LEFT,
        38.0,
        "start",
        "small",
        &format!("group: first value {first} mA, minimum difference {min} mA, width {width} bits"),
    );
    p1.frame(
        &mut s,
        "the samples, mA",
        &vlo.to_string(),
        &vhi.to_string(),
    );
    let path: String = values
        .iter()
        .enumerate()
        .map(|(j, v)| {
            format!(
                "{}{:.1} {:.1}",
                if j == 0 { "M" } else { " L" },
                Plot::x(j),
                p1.y(f64::from(*v))
            )
        })
        .collect();
    s.path(&path, "line");
    p2.frame(
        &mut s,
        "the differences: each sample less the one before",
        &min.to_string(),
        &max.to_string(),
    );
    let zero = p2.y(min as f64);
    for (j, d) in diffs.iter().enumerate() {
        let (x, y) = (Plot::x(j + 1), p2.y(*d as f64));
        s.rect((x - 1.0, y), (2.0, (zero - y).max(0.5)), 0.0, "blue");
    }
    s.text(
        Plot::x(127),
        zero + 14.0,
        "end",
        "accent small",
        &format!(
            "stored: each difference less {min}, from 0 to {}, in {width} bits",
            max - min
        ),
    );
    s.text(
        LEFT,
        p2.top + p2.height + 40.0,
        "start",
        "small",
        &format!(
            "the group: 5 bytes and 127 × {width} bits, {} bytes, against 256 bytes raw",
            5 + (127 * width as usize).div_ceil(8)
        ),
    );
    Ok(s.finish())
}

/// `sync_loss.svg`.
pub fn sync_loss() -> anyhow::Result<String> {
    let run = Drive::full().minutes(2);
    let recording = run.record().or_debug()?;
    let piece = recording.pieces.first().context("a piece")?;
    let blocks = spans(piece)?;
    let syncs: Vec<(SyncPolicy, Vec<u64>)> = SYNC_POLICIES
        .iter()
        .map(|p| Ok((*p, sync_offsets(&run, *p).or_debug()?)))
        .collect::<anyhow::Result<_>>()?;
    // seconds 70 to 73 of the run: each second starts with its records block (the log line and the temperature)
    let second = |n: usize| {
        blocks
            .iter()
            .filter(|b| b.part == Part::Log)
            .nth(n)
            .map(|b| b.at)
            .context("a second of the run")
    };
    let (from, to) = (second(70)?, second(73)?);
    // the crash: in the middle of the chunk of samples of the third second
    let third = second(72)?;
    let torn = blocks
        .iter()
        .find(|b| b.part == Part::Samples && b.at > third)
        .context("a chunk of samples in the third second")?;
    let crash = torn.at + torn.len / 2;
    let v = View::new(from, to);
    let (top, y) = (20.0, 60.0);
    let row_gap = 34.0;
    let mut s = Svg::new(
        LEFT * 2.0 + ROW_W,
        y + ROW_H + 30.0 + row_gap * syncs.len() as f64 + 40.0,
        "Three seconds of the drive's file and where four sync policies synced: a crash in the third second loses the bytes after the last sync, from part of a block to everything since the last cut",
    );
    s.text(
        LEFT,
        top,
        "start",
        "strong",
        "seconds 70 to 72 of the drive's file, and the syncs of each policy",
    );
    row(&mut s, &blocks, to, v, y, None);
    let xc = v.x(crash);
    s.line(
        (xc, y - 10.0),
        (xc, y + ROW_H + 30.0 + row_gap * syncs.len() as f64 - 10.0),
        "accent-line",
    );
    s.text(xc, y - 14.0, "middle", "accent small", "the crash");
    for (i, (policy, offsets)) in syncs.iter().enumerate() {
        let yy = y + ROW_H + 30.0 + row_gap * i as f64;
        let name = match policy {
            SyncPolicy::Interval(t) => format!("Interval({} s)", t.as_secs()),
            SyncPolicy::EveryBlocks(n) => format!("EveryBlocks({n})"),
            SyncPolicy::None => "None".to_owned(),
        };
        s.text(LEFT, yy - 8.0, "start", "small", &name);
        s.line((v.x(from), yy), (v.x(to), yy), "faint");
        for o in offsets.iter().filter(|o| **o >= from && **o <= to) {
            s.circle((v.x(*o), yy), 3.0, "blue");
        }
        let last = offsets
            .iter()
            .filter(|o| **o <= crash)
            .max()
            .copied()
            .unwrap_or(0);
        let x0 = v.x(last.max(from));
        s.rect(
            (x0, yy - 4.0),
            ((xc - x0).max(1.0), 8.0),
            1.0,
            "tint-accent",
        );
        let lost = crash - last;
        let label = if last < from {
            format!("lost: {} back to its last sync", kib(lost))
        } else {
            format!("lost: {} bytes", lost)
        };
        s.text(v.x(to), yy - 8.0, "end", "accent small", &label);
    }
    legend(
        &mut s,
        y + ROW_H + 30.0 + row_gap * syncs.len() as f64 + 4.0,
        &[("blue", "a sync"), ("tint-accent", "what the crash loses")],
    );
    Ok(s.finish())
}

/// `cut.svg`.
pub fn cut() -> anyhow::Result<String> {
    let recording = drive()?.record().or_debug()?;
    let (p0, p1) = (
        recording.pieces.first().context("piece 0")?,
        recording.pieces.get(1).context("piece 1")?,
    );
    let (b0, b1) = (spans(p0)?, spans(p1)?);
    let len0 = u64::try_from(p0.len())?;
    let r0 = TapeReader::open(&p0[..]).or_debug()?;
    let i0 = r0.pieces().first().context("piece 0 read")?;
    let close_at = i0.close_offset.context("piece 0's close")?;
    let (close, ranges) = i0.close.as_ref().context("piece 0's close")?;
    let pad = u64::try_from(close_padding(close_at, ranges.len(), i0.removed.len()))?;
    let r1 = TapeReader::open(&p1[..]).or_debug()?;
    let i1 = r1.pieces().first().context("piece 1 read")?;
    let (h1, _) = i1.head.as_ref().context("piece 1's head")?;
    ensure!(
        close.next == Some(h1.ident.id),
        "piece 0's close names piece 1's id"
    );
    ensure!(
        h1.prev.map(|(hash, _)| hash) == Some(close.hash),
        "piece 1's head names piece 0's hash"
    );
    let final_at = b0
        .iter()
        .find(|b| b.part == Part::FinalIndex)
        .context("a final index")?
        .at;
    let first_data = b1
        .iter()
        .find(|b| !matches!(b.part, Part::Head | Part::Puts))
        .context("data in piece 1")?;
    let carried = b1
        .iter()
        .filter(|b| b.part == Part::Puts && b.at < first_data.at)
        .count();

    let from0 = final_at.saturating_sub(1024);
    let span = len0 - from0;
    let to1 = first_data.at + first_data.len;
    let (v0, v1) = (View::new(from0, len0), View::new(0, to1));
    let (top, y0, y1) = (20.0, 76.0, 216.0);
    let close_len = u64::try_from(ww_tape::piece::close_len(ranges.len(), i0.removed.len()))?;
    let tail = ww_tape::piece::TAIL_LEN as u64;
    let mut s = Svg::new(
        LEFT * 2.0 + ROW_W,
        y1 + ROW_H + 84.0,
        &format!(
            "A cut: piece 0 ends with its final index, its close padded by {pad} bytes to the 4 KiB end, and its tail; piece 1 starts with its head and {carried} blocks of puts that carry the definitions, before its first data"
        ),
    );
    s.text(
        LEFT,
        top,
        "start",
        "strong",
        "a cut: the end of piece 0 and the start of piece 1",
    );
    s.text(
        LEFT,
        y0 - 30.0,
        "start",
        "small",
        &format!("piece 0, its last {}", kib(span)),
    );
    row(&mut s, &b0, len0, v0, y0, None);
    let (xp0, xp1) = (v0.x(close_at + close_len), v0.x(len0 - tail));
    s.rect((xp0, y0), ((xp1 - xp0).max(1.0), ROW_H), 0.0, "faint hatch");
    let mark = |s: &mut Svg, x: f64, y: f64, anchor: &str, text: &str| {
        s.line((x, y - 2.0), (x, y - 10.0), "thin");
        s.text(x, y - 13.0, anchor, "soft small", text);
    };
    mark(
        &mut s,
        v0.x(close_at + close_len / 2),
        y0,
        "middle",
        "close",
    );
    mark(
        &mut s,
        (xp0 + xp1) / 2.0,
        y0,
        "middle",
        &format!("padding, {pad} bytes"),
    );
    mark(&mut s, v0.x(len0 - tail / 2), y0, "end", "tail");
    s.text(
        v0.x(len0),
        y0 + ROW_H + 16.0,
        "end",
        "soft small",
        &format!("ends on {} ({} × 4 KiB)", kib(len0), len0 / 4096),
    );
    s.text(
        LEFT,
        y1 + ROW_H + 16.0,
        "start",
        "small",
        &format!("piece 1, its first {}", kib(to1)),
    );
    row(&mut s, &b1, to1, v1, y1, None);
    mark(
        &mut s,
        v1.x(first_data.at / 2),
        y1,
        "middle",
        &format!("head and {carried} blocks of puts"),
    );
    s.path(
        &format!(
            "M{:.1} {:.1} C{:.1} {:.1} {:.1} {:.1} {:.1} {:.1}",
            v1.x(60),
            y1 - 2.0,
            v1.x(60),
            y1 - 50.0,
            v0.x(close_at + close_len / 2),
            y0 + ROW_H + 60.0,
            v0.x(close_at + close_len / 2),
            y0 + ROW_H + 2.0
        ),
        "accent-line",
    );
    s.text(
        LEFT,
        y0 + ROW_H + 16.0,
        "start",
        "accent small",
        "piece 1's head names piece 0 by its content hash; its close names piece 1's id",
    );
    legend(
        &mut s,
        y1 + ROW_H + 34.0,
        &[
            ("tint-green", "head, final index, close, tail"),
            ("tint-purple", "puts"),
            ("tint-blue", "data"),
            ("faint hatch", "padding"),
        ],
    );
    s.text(
        LEFT,
        y1 + ROW_H + 68.0,
        "start",
        "soft small",
        "drawn from the drive's chain at 1/1000 of its data; each row at its own scale",
    );
    Ok(s.finish())
}
