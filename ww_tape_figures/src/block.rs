//! `block.svg`: a records block of two records, byte by byte. The bytes are the ones [BlockWriter] writes; the field
//! boundaries are checked against them before anything is drawn.

use anyhow::{Context, ensure};
use ww_tape::{
    Block, BlockId, BlockKind, BlockWriter, CRC_LEN, ClockTime, HEADER_LEN, MAGIC, SessionTag,
};
use ww_tape::{ClockNum, TrackNum};

use crate::OrDebug;
use crate::defs::CLOCK;
use crate::layout::{Field, Grid, le};
use crate::svg::Svg;

/// Session tag and seq of the drawn block.
const SESSION: u32 = 0x5e55_10a7;
const SEQ: u64 = 3;
/// The two records: track, tick, item.
const RECORDS: [(u32, i64, &[u8]); 2] = [(2, 1_000_000, b"ok"), (3, 1_000_250, &[0x9c])];

fn uvar(mut v: u64, out: &mut Vec<u8>) {
    loop {
        let byte = u8::try_from(v & 0x7f).unwrap_or(0);
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn ivar(v: i64) -> Vec<u8> {
    let mut out = Vec::new();
    uvar(((v << 1) ^ (v >> 63)).cast_unsigned(), &mut out);
    out
}

fn uvar_of(v: u64) -> Vec<u8> {
    let mut out = Vec::new();
    uvar(v, &mut out);
    out
}

/// The block's bytes and its fields, the header's checked against the bytes and the body's against an encoding of
/// the records by hand.
fn fields() -> anyhow::Result<(Vec<u8>, Vec<Field>)> {
    let mut buf = [0u8; 256];
    let id = BlockId::new(SessionTag::writable(SESSION), SEQ);
    let mut b =
        BlockWriter::new(&mut buf, BlockKind::Records, id, &[ClockNum::new(CLOCK)]).or_debug()?;
    for (track, tick, item) in RECORDS {
        b.push(
            TrackNum::new(track),
            ClockTime::new(ClockNum::new(CLOCK), tick),
            &[item],
        )
        .or_debug()?;
    }
    let len = b.finish();
    let bytes = buf.get(..len).context("the finished block")?.to_vec();
    let block = Block::read(&bytes).or_debug()?;
    ensure!(block.kind == BlockKind::Records && block.seq == SEQ);
    ensure!(bytes.get(..4) == Some(MAGIC.as_slice()), "magic at 0");
    ensure!(bytes.get(4) == Some(&block.version), "version at 4");
    ensure!(bytes.get(5) == Some(&1), "kind 1, records, at 5");
    ensure!(le(&bytes, 6, 2)? == u64::from(block.flags), "flags at 6");
    ensure!(le(&bytes, 8, 4)? == u64::try_from(len)?, "length at 8");
    ensure!(
        le(&bytes, 12, 4)? == u64::from(block.record_count),
        "records at 12"
    );
    ensure!(le(&bytes, 16, 8)? == SEQ, "seq at 16");
    ensure!(
        le(&bytes, 24, 4)? == u64::from(block.session.get()),
        "session at 24"
    );

    let mut f = vec![
        Field::new(0, 4, "magic", "WWTP".into(), "ink"),
        Field::new(4, 1, "ver", block.version.to_string(), "blue"),
        Field::new(
            5,
            1,
            "kind",
            format!("{}", bytes.get(5).context("kind")?),
            "blue",
        ),
        Field::new(6, 2, "flags", format!("{:#x}", block.flags), "purple"),
        Field::new(8, 4, "length", len.to_string(), "green"),
        Field::new(12, 4, "records", block.record_count.to_string(), "green"),
        Field::new(16, 8, "seq", SEQ.to_string(), "amber"),
        Field::new(
            24,
            4,
            "session",
            format!("{:08x}", block.session.get()),
            "teal",
        ),
        Field::new(28, 4, "head CRC", "0".into(), "ink"),
    ];
    ensure!(le(&bytes, 28, 4)? == 0, "no head CRC in a records block");

    // the body: a clock table, then per record a head, a time delta, a length and the item
    let mut body = Vec::new();
    let mut at = HEADER_LEN;
    let mut push = |f: &mut Vec<Field>, part: Vec<u8>, name, value: String, class| {
        let len = part.len();
        f.push(Field::new(at, len, name, value, class));
        at += len;
        body.extend(part);
    };
    let mut table = uvar_of(1);
    uvar(u64::from(CLOCK), &mut table);
    push(&mut f, table, "clocks", "1: clock 1".into(), "teal");
    let mut prev = 0i64;
    for (track, tick, item) in RECORDS {
        // one clock: the head is the track, shifted by no slot bits
        push(
            &mut f,
            uvar_of(u64::from(track)),
            "track",
            format!("trk {track}"),
            "blue",
        );
        push(
            &mut f,
            ivar(tick - prev),
            "Δ ticks",
            (tick - prev).to_string(),
            "amber",
        );
        prev = tick;
        push(
            &mut f,
            uvar_of(u64::try_from(item.len())?),
            "len",
            item.len().to_string(),
            "green",
        );
        let shown = match std::str::from_utf8(item) {
            Ok(s) if s.chars().all(|c| c.is_ascii_graphic()) => format!("\"{s}\""),
            _ => item.iter().map(|b| format!("{b:02x}")).collect(),
        };
        push(&mut f, item.to_vec(), "item", shown, "ink");
    }
    ensure!(
        bytes.get(HEADER_LEN..len - CRC_LEN) == Some(body.as_slice()),
        "the body is the clock table and the records as drawn"
    );
    f.push(Field::new(
        len - CRC_LEN,
        CRC_LEN,
        "CRC-32C",
        "of bytes 0..".to_owned() + &(len - CRC_LEN).to_string(),
        "accent",
    ));
    Ok((bytes, f))
}

pub fn block() -> anyhow::Result<String> {
    let (bytes, fields) = fields()?;
    let len = bytes.len();
    let body_end = len - CRC_LEN;
    // rows: the header in two rows of 16, the body, the CRC; each starts at its offset
    let rows: [(usize, usize, &str); 4] = [
        (0, 16, "header"),
        (16, HEADER_LEN, ""),
        (HEADER_LEN, body_end, "body"),
        (body_end, len, ""),
    ];
    let g = Grid::BYTES;
    let mut s = Svg::new(
        g.width(),
        g.height(rows.len()),
        &format!(
            "A records block of {len} bytes: a {HEADER_LEN}-byte header, a body of a clock table and two records, a CRC-32C"
        ),
    );
    g.draw(&mut s, &bytes, 0, &fields, &rows)?;
    // the two records over the body row
    let body_y = g.y(2);
    let mut x = g.left + g.cell * 2.0;
    for (i, (_, _, item)) in RECORDS.iter().enumerate() {
        let n = fields
            .iter()
            .filter(|f| f.at >= HEADER_LEN && f.at < body_end && f.name != "clocks")
            .skip(i * 4)
            .take(4)
            .map(|f| f.len)
            .sum::<usize>();
        ensure!(n > item.len());
        let w = g.cell * f64::from(u16::try_from(n)?);
        s.bracket_over(x + 2.0, x + w - 2.0, body_y - 6.0, "thin");
        s.text(
            x + w / 2.0,
            body_y - 13.0,
            "middle",
            "soft small",
            &format!("record {}", i + 1),
        );
        x += w;
    }
    Ok(s.finish())
}
