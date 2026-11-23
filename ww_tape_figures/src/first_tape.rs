//! `first_tape.svg`: the file the program of the chapter "A first tape" writes ([example_drive::first_tape]), the
//! drive's first minute at full rate. Its first seconds and its end are drawn to scale on rows of 16 KiB, every block
//! named by what it holds, and its first KiB enlarged to name every put; the blocks in between are counted.

use anyhow::{Context, ensure};
use example_drive::{Drive, DriveNumbers};
use shrink_wrap::prelude::*;
use ww_tape::RecordTrack;
use ww_tape::{BlockKind, Describe, FLAG_REPEAT, FileBlocks};

use crate::OrDebug;
use crate::svg::Svg;

/// Bytes a row of the drawing holds.
const ROW: u64 = 16 << 10;
/// Bytes of the file's start drawn enlarged.
const ZOOM: u64 = 1 << 10;
/// Rows drawn of the file's start.
const START_ROWS: u64 = 5;
/// [ww_tape::CRC_LEN] as a byte count.
const CRC_LEN: u64 = ww_tape::CRC_LEN as u64;

/// What a block is, as the drawing names and colours it.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Part {
    Puts,
    Records,
    Frames,
    Samples,
    Summary,
    Index,
    Close,
    Padding,
    Tail,
}

impl Part {
    fn name(self) -> &'static str {
        match self {
            Self::Puts => "puts",
            Self::Records => "log, temp",
            Self::Frames => "can0",
            Self::Samples => "phase_a: a second of samples",
            Self::Summary => "summaries",
            Self::Index => "final index",
            Self::Close => "close",
            Self::Padding => "zero padding",
            Self::Tail => "tail",
        }
    }

    fn class(self) -> &'static str {
        match self {
            Self::Puts => "tint-purple",
            Self::Records | Self::Frames | Self::Samples => "tint-blue",
            Self::Summary => "tint-teal",
            Self::Index => "tint-amber",
            Self::Close | Self::Tail => "tint-green",
            Self::Padding => "faint hatch",
        }
    }
}

/// A block, or a close's padding: offset, length, what it is, and for puts what they hold.
type Span = (u64, u64, Part, Option<Vec<String>>);

/// What the records of a block of puts hold, as the lines of its label: the definitions (the producer, the layout,
/// clocks, the map, track labels), then the item of the mode, track `mode`.
fn puts_of(b: &ww_tape::Block<'_>, mode: ww_tape::TrackNum) -> anyhow::Result<Vec<String>> {
    let (mut defs, mut clocks, mut state): (Vec<String>, usize, Vec<String>) = Default::default();
    for r in b.records() {
        let r = r.or_debug()?;
        if r.track == RecordTrack::Data(mode) {
            state.push(format!(
                "\u{201c}{}\u{201d}",
                String::from_utf8_lossy(r.item)
            ));
            continue;
        }
        ensure!(
            r.track == RecordTrack::Definitions,
            "a put on track {}",
            r.track
        );
        match Describe::from_ww_bytes(r.item).or_debug()? {
            Describe::Producer(_) => defs.push("producer".to_owned()),
            Describe::Layout(_) => defs.push("layout".to_owned()),
            Describe::Clock(_) => clocks += 1,
            Describe::Map(_) => defs.push("map".to_owned()),
            Describe::Track(t) => defs.push(t.label.to_owned()),
            _ => defs.push("definition".to_owned()),
        }
    }
    match clocks {
        0 => {}
        1 => defs.insert(0, "clock".to_owned()),
        n => defs.insert(0, format!("{n} clocks")),
    }
    let mut lines = vec![defs.join(", ")];
    lines.extend(state);
    Ok(lines)
}

/// The file the first-tape program writes, as spans, and its length.
fn spans() -> anyhow::Result<(u64, Vec<Span>)> {
    let dir = std::env::temp_dir().join(format!("ww_tape_figures-first-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("drive.wwt");
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    let drive = Drive::full().minutes(1);
    let numbers: DriveNumbers = drive.numbers()?;
    example_drive::first_tape::write(&path, &drive)?;
    let bytes = std::fs::read(&path)?;
    std::fs::remove_dir_all(&dir)?;
    let len = u64::try_from(bytes.len())?;

    let mut walk = FileBlocks::new(&bytes[..]);
    let mut out = Vec::new();
    while let Some(b) = walk.next_block() {
        let b = b.or_debug()?;
        let n = u64::try_from(b.len())?;
        let part = match b.kind {
            BlockKind::Records if b.flags & FLAG_REPEAT != 0 => Part::Puts,
            BlockKind::Records => Part::Records,
            BlockKind::Events => {
                let head = b.chunk().context("a chunk's head")?.head();
                ensure!(
                    head.track_number == numbers.can0,
                    "events chunks are can0's"
                );
                Part::Frames
            }
            BlockKind::Series => {
                let head = b.chunk().context("a chunk's head")?.head();
                ensure!(
                    head.track_number == numbers.phase_a,
                    "series chunks are phase_a's"
                );
                Part::Samples
            }
            BlockKind::Summary => Part::Summary,
            BlockKind::Index => Part::Index,
            BlockKind::Close => {
                let c = ww_tape::piece::CloseBlock::read(&b).or_debug()?;
                let used = u64::try_from(ww_tape::piece::close_len(
                    c.tracks().count(),
                    c.removed().count(),
                ))?;
                let pad = n - used;
                let at = walk.offset();
                out.push((at, n - pad - CRC_LEN, Part::Close, None));
                out.push((at + n - pad - CRC_LEN, pad, Part::Padding, None));
                out.push((at + n - CRC_LEN, CRC_LEN, Part::Close, None));
                continue;
            }
            BlockKind::Tail => Part::Tail,
            other => anyhow::bail!("a block of kind {other:?} in the first tape"),
        };
        let name = if part == Part::Puts {
            Some(puts_of(&b, numbers.mode)?)
        } else {
            None
        };
        out.push((walk.offset(), n, part, name));
    }
    ensure!(
        out.last().map(|p| p.0 + p.1) == Some(len),
        "the blocks fill the file"
    );
    ensure!(
        len % u64::from(ww_tape::PIECE_ALIGN) == 0,
        "a closed piece ends on 4 KiB"
    );
    Ok((len, out))
}

/// `line` broken after its commas into lines that fit `width` units of small text, where a part fits alone.
fn wrap(line: &str, width: f64) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for part in line.split_inclusive(", ") {
        match out.last_mut() {
            Some(last) if 6.6 * (last.len() + part.len()) as f64 + 6.0 <= width => {
                last.push_str(part)
            }
            _ => out.push(part.to_owned()),
        }
    }
    out.iter().map(|l| l.trim_end().to_owned()).collect()
}

pub fn first_tape() -> anyhow::Result<String> {
    let (len, spans) = spans()?;
    let index_at = spans
        .iter()
        .find(|sp| sp.2 == Part::Index)
        .map(|s| s.0)
        .context("the final index")?;
    let end_rows = (index_at / ROW)..len.div_ceil(ROW);
    ensure!(
        end_rows.start > START_ROWS,
        "the start and the end of the file are apart"
    );
    let skipped_from = START_ROWS * ROW;
    let skipped_to = end_rows.start * ROW;
    let skipped = spans
        .iter()
        .filter(|sp| sp.0 >= skipped_from && sp.0 + sp.1 <= skipped_to)
        .count();

    let (zoom_h, zoom_gap) = (44.0, 26.0);
    let (left, top, row_w, row_h, pitch) = (76.0, 30.0 + zoom_h + zoom_gap, 600.0, 30.0, 38.0);
    let scale = row_w / ROW as f64;
    let gap = 34.0;
    let end_top = top + pitch * START_ROWS as f64 + gap;
    let end_count = end_rows.end - end_rows.start;
    let legend_y = end_top + pitch * end_count as f64 + 12.0;
    let mut s = Svg::new(
        left + row_w + 24.0,
        legend_y + 46.0,
        &format!(
            "The drive's first minute in one file of {len} bytes, on rows of 16 KiB: the first seconds, then the end. Puts, records blocks of log lines and temperatures, chunks of CAN frames and of phase current, summaries, the final index, the close padded with zeros and the tail"
        ),
    );
    let y_of = |row: u64| {
        if row < START_ROWS {
            top + pitch * row as f64
        } else {
            end_top + pitch * (row - end_rows.start) as f64
        }
    };
    let rows = (0..START_ROWS).chain(end_rows.clone());
    for row in rows {
        s.text(
            left - 8.0,
            y_of(row) + 19.5,
            "end",
            "soft mono",
            &format!("{}", row * ROW),
        );
    }
    let break_y = top + pitch * START_ROWS as f64 + gap / 2.0 - 4.0;
    s.text(
        left + row_w / 2.0,
        break_y + 4.0,
        "middle",
        "small soft",
        &format!(
            "{skipped} more blocks, {:.2} MiB: the other seconds of the minute",
            (skipped_to - skipped_from) as f64 / f64::from(1 << 20)
        ),
    );
    for (at, n, part, _) in &spans {
        let mut from = *at;
        let end = at + n;
        let mut labelled = false;
        while from < end {
            let row = from / ROW;
            let to = end.min((row + 1) * ROW);
            if row < START_ROWS || end_rows.contains(&row) {
                let x = left + (from - row * ROW) as f64 * scale;
                let w = (to - from) as f64 * scale;
                let y = y_of(row);
                s.rect((x + 0.3, y), ((w - 0.6).max(0.8), row_h), 1.5, part.class());
                let name = part.name();
                if !labelled && w >= 6.6 * name.len() as f64 + 8.0 {
                    s.text(x + w / 2.0, y + 19.5, "middle", "small", name);
                    labelled = true;
                }
            }
            from = to;
        }
    }

    // the first KiB enlarged above row 0, every block named
    let zoom_scale = row_w / ZOOM as f64;
    let zoom_y = top - zoom_gap - zoom_h;
    s.text(left - 8.0, zoom_y + 19.5, "end", "small soft", "first KiB");
    for (at, n, part, puts) in spans.iter().filter(|sp| sp.0 < ZOOM) {
        let to = (at + n).min(ZOOM);
        let x = left + *at as f64 * zoom_scale;
        let w = (to - at) as f64 * zoom_scale;
        s.rect(
            (x + 0.5, zoom_y),
            ((w - 1.0).max(0.8), zoom_h),
            1.5,
            part.class(),
        );
        let lines = puts.clone().unwrap_or_else(|| vec![part.name().to_owned()]);
        let wrapped: Vec<String> = lines.iter().flat_map(|l| wrap(l, w)).collect();
        let longest = wrapped.iter().map(String::len).max().unwrap_or(0);
        if 6.6 * longest as f64 + 6.0 <= w && wrapped.len() <= 3 {
            let lines = wrapped;
            // inside, centred
            let first = zoom_y + zoom_h / 2.0 + 4.0 - 7.0 * (lines.len() - 1) as f64;
            for (i, line) in lines.iter().enumerate() {
                s.text(
                    x + w / 2.0,
                    first + 14.0 * i as f64,
                    "middle",
                    "small",
                    line,
                );
            }
        } else {
            // above, from the block's start
            s.line((x + 2.0, zoom_y - 1.0), (x + 2.0, zoom_y - 6.0), "thin");
            s.text(x, zoom_y - 9.0, "start", "small", &lines.join(", "));
        }
    }
    // the enlarged stretch is the first KiB of row 0
    let x_end = left + ZOOM as f64 * scale;
    s.path(
        &format!(
            "M{left:.1} {:.1} L{left:.1} {:.1} M{:.1} {:.1} L{x_end:.1} {:.1}",
            zoom_y + zoom_h + 2.0,
            top - 2.0,
            left + row_w,
            zoom_y + zoom_h + 2.0,
            top - 2.0
        ),
        "thin dash",
    );

    let legend = [
        (Part::Puts, "puts (track 0, mode)"),
        (Part::Records, "data: records, chunks"),
        (Part::Summary, "summaries"),
        (Part::Index, "index"),
        (Part::Close, "close, tail"),
        (Part::Padding, "zero padding"),
    ];
    for (i, (part, name)) in legend.into_iter().enumerate() {
        let col = (i % 3) as f64;
        let line = (i / 3) as f64;
        let x = left + col * 200.0;
        let y = legend_y + line * 20.0;
        s.rect((x, y), (16.0, 12.0), 2.0, part.class());
        s.text(x + 22.0, y + 10.5, "start", "small", name);
    }
    Ok(s.finish())
}
