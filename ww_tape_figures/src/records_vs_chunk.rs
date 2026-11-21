//! `records_vs_chunk.svg`: the drive's first 96 CAN frames on `can0` ([ww_tape_example::frame]) stored twice, as a
//! records block and as an events chunk, both drawn to one scale of bytes. Each part's place and length is read
//! back from the block [ww_tape] wrote.

use anyhow::{Context, ensure};
use ww_tape::RecordTrack;
use ww_tape::chunk::write_chunk;
use ww_tape::payloads::EncodedPayloads;
use ww_tape::times::EncodedTimes;
use ww_tape::{
    Block, BlockId, BlockKind, BlockWriter, CRC_LEN, ClockTime, ColumnKind, ColumnSpec, HEADER_LEN,
    SessionTag,
};
use ww_tape_example::{Drive, DriveNumbers, frame, key};

use crate::OrDebug;
use crate::svg::Svg;

/// Frames drawn.
pub const FRAMES: u64 = 96;
/// µs between two frames: 1 kHz, as `can0` runs at full scale.
const PERIOD_US: u64 = 1_000;

/// A drawn part of a block: offset, length, name and class.
struct Part {
    at: usize,
    len: usize,
    name: String,
    class: &'static str,
}

/// Where `inner` starts in `outer`, which holds it.
fn offset_in(outer: &[u8], inner: &[u8]) -> anyhow::Result<usize> {
    let at = (inner.as_ptr() as usize)
        .checked_sub(outer.as_ptr() as usize)
        .context("the slice lies in the block")?;
    ensure!(
        at + inner.len() <= outer.len(),
        "the slice lies in the block"
    );
    Ok(at)
}

/// The frames as a records block on the drive's numbers `n`, and its parts: header, clock table, the records, CRC.
fn records(frames: &[(i64, Vec<u8>)], n: &DriveNumbers) -> anyhow::Result<(Vec<u8>, Vec<Part>)> {
    let mut buf = vec![0u8; 64 << 10];
    let id = BlockId::new(SessionTag::writable(1), 0);
    let mut w = BlockWriter::new(&mut buf, BlockKind::Records, id, &[n.board_clock]).or_debug()?;
    for (t, payload) in frames {
        w.push(n.can0, ClockTime::new(n.board_clock, *t), &[payload])
            .or_debug()?;
    }
    let len = w.finish();
    buf.truncate(len);
    let block = Block::read(&buf).or_debug()?;
    let mut parts = vec![Part {
        at: 0,
        len: HEADER_LEN,
        name: "header".into(),
        class: "tint-ink",
    }];
    // the first record starts after the clock table; each record ends with its item
    let mut ends = Vec::new();
    for r in block.records() {
        let r = r.or_debug()?;
        ensure!(
            r.track == RecordTrack::Data(n.can0),
            "every record is a frame of can0"
        );
        ends.push(offset_in(&buf, r.item)? + r.item.len());
    }
    ensure!(ends.len() == frames.len(), "one record per frame");
    // the clock table of one clock: its count and its number, a byte each
    let table = 2;
    parts.push(Part {
        at: HEADER_LEN,
        len: table,
        name: "clocks".into(),
        class: "tint-teal",
    });
    let mut at = HEADER_LEN + table;
    for (i, end) in ends.iter().enumerate() {
        parts.push(Part {
            at,
            len: end - at,
            name: String::new(),
            class: if i % 2 == 0 {
                "tint-blue"
            } else {
                "tint-purple"
            },
        });
        at = *end;
    }
    ensure!(at == len - CRC_LEN, "the records end at the CRC");
    parts.push(Part {
        at,
        len: CRC_LEN,
        name: "CRC".into(),
        class: "tint-accent",
    });
    Ok((buf, parts))
}

/// The frames as an events chunk, and its parts: header, chunk head, column table and segment CRC, the columns, the
/// zeros after them, CRC; on the drive's numbers `n`.
fn chunk(frames: &[(i64, Vec<u8>)], n: &DriveNumbers) -> anyhow::Result<(Vec<u8>, Vec<Part>)> {
    let starts: Vec<i64> = frames.iter().map(|f| f.0).collect();
    let times = EncodedTimes::encode(&starts, None, None).or_debug()?;
    let payloads = EncodedPayloads::encode(frames.iter().map(|f| f.1.as_slice())).or_debug()?;
    let head = times.head.chunk_head(key("can0"), n.can0, 0);
    let columns: Vec<(ColumnSpec, &[u8])> = times.columns().chain(payloads.columns()).collect();
    let mut buf = vec![0u8; 64 << 10];
    let id = BlockId::new(SessionTag::writable(1), 0);
    let len = write_chunk(&mut buf, id, &head, &columns, 0).or_debug()?;
    buf.truncate(len);
    let block = Block::read(&buf).or_debug()?;
    let chunk = block.chunk().context("an events chunk")?;
    let mut parts = vec![Part {
        at: 0,
        len: HEADER_LEN,
        name: "header".into(),
        class: "tint-ink",
    }];
    let mut cols = Vec::new();
    for c in chunk.columns() {
        let name = match c.kind {
            ColumnKind::Starts => "times",
            ColumnKind::PayloadLengths => "lengths",
            ColumnKind::Payloads => "payloads",
            other => anyhow::bail!("a column of kind {other:?} in a chunk of frames"),
        };
        cols.push((offset_in(&buf, c.bytes)?, c.bytes.len(), name));
    }
    let (first, _, _) = *cols.first().context("a column")?;
    parts.push(Part {
        at: HEADER_LEN,
        len: first - HEADER_LEN,
        name: "chunk head".into(),
        class: "tint-green",
    });
    let mut end = first;
    for (at, n, name) in cols {
        ensure!(at == end, "the columns lie back to back");
        parts.push(Part {
            at,
            len: n,
            name: name.into(),
            class: "tint-blue",
        });
        end = at + n;
    }
    parts.push(Part {
        at: end,
        len: len - CRC_LEN - end,
        name: "zeros".into(),
        class: "faint hatch",
    });
    parts.push(Part {
        at: len - CRC_LEN,
        len: CRC_LEN,
        name: "CRC".into(),
        class: "tint-accent",
    });
    Ok((buf, parts))
}

pub fn records_vs_chunk() -> anyhow::Result<String> {
    let frames: Vec<(i64, Vec<u8>)> = (0..FRAMES).map(|k| frame(k, PERIOD_US)).collect();
    let payload: usize = frames.iter().map(|f| f.1.len()).sum();
    let n = Drive::full().numbers().or_debug()?;
    let (rec, rec_parts) = records(&frames, &n)?;
    let (chk, chk_parts) = chunk(&frames, &n)?;
    let (left, width) = (20.0, 640.0);
    let px = width / rec.len() as f64;
    let mut s = Svg::new(
        left * 2.0 + width,
        252.0,
        &format!(
            "{FRAMES} CAN frames of {payload} payload bytes: a records block of {} bytes against an events chunk of {} bytes",
            rec.len(),
            chk.len()
        ),
    );
    let h = 28.0;
    let draw = |s: &mut Svg, parts: &[Part], y: f64| {
        for p in parts {
            let x = left + p.at as f64 * px;
            let w = (p.len as f64 * px - 0.6).max(0.6);
            s.rect((x, y), (w, h), 1.0, p.class);
        }
    };

    // the records block
    let y0 = 34.0;
    s.text(
        left,
        y0 - 10.0,
        "start",
        "strong",
        &format!(
            "records block: {} bytes, {:.1} bytes a frame",
            rec.len(),
            rec.len() as f64 / FRAMES as f64
        ),
    );
    draw(&mut s, &rec_parts, y0);
    let first = rec_parts.get(2).context("a record")?;
    let last = rec_parts
        .get(rec_parts.len() - 2)
        .context("the last record")?;
    let (x0, x1) = (
        left + first.at as f64 * px,
        left + (last.at + last.len) as f64 * px,
    );
    s.bracket_under(x0, x1, y0 + h + 8.0, "thin");
    s.text(
        (x0 + x1) / 2.0,
        y0 + h + 22.0,
        "middle",
        "small",
        &format!(
            "{FRAMES} records, each a head, a time delta, a length and the frame's {} bytes",
            frames.first().map_or(0, |f| f.1.len())
        ),
    );

    // the chunk
    let y1 = 128.0;
    s.text(
        left,
        y1 - 10.0,
        "start",
        "strong",
        &format!(
            "events chunk: {} bytes, {:.1} bytes a frame",
            chk.len(),
            chk.len() as f64 / FRAMES as f64
        ),
    );
    draw(&mut s, &chk_parts, y1);
    // labels below, each on its own line, a leader up to its part
    let named: Vec<&Part> = chk_parts.iter().filter(|p| !p.name.is_empty()).collect();
    let label_x = left + chk.len() as f64 * px + 24.0;
    for (i, p) in named.iter().enumerate() {
        let cx = left + (p.at as f64 + p.len as f64 / 2.0) * px;
        let ly = y1 + h + 14.0 + 12.0 * i as f64;
        s.path(
            &format!(
                "M{cx:.1} {:.1} V{:.1} H{:.1}",
                y1 + h + 2.0,
                ly - 4.0,
                label_x - 4.0
            ),
            "thin",
        );
        s.text(
            label_x,
            ly,
            "start",
            "small",
            &if p.name == "payloads" {
                format!(
                    "payloads, {} bytes: the frames' {payload} bytes coded with lz4",
                    p.len
                )
            } else {
                format!("{}, {} bytes", p.name, p.len)
            },
        );
    }
    Ok(s.finish())
}
