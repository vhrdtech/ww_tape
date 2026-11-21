//! `block.svg`: a records block of two records, byte by byte. The bytes are the ones [BlockWriter] writes; the field
//! boundaries are checked against them before anything is drawn.

use anyhow::{Context, ensure};
use ww_tape::{
    Block, BlockId, BlockKind, BlockWriter, CRC_LEN, ClockTime, HEADER_LEN, MAGIC, SessionTag,
};
use ww_tape::{ClockNum, TrackNum};

use crate::OrDebug;
use crate::defs::CLOCK;
use crate::svg::Svg;

/// Session tag and seq of the drawn block.
const SESSION: u32 = 0x5e55_10a7;
const SEQ: u64 = 3;
/// The two records: track, tick, item.
const RECORDS: [(u32, i64, &[u8]); 2] = [(2, 1_000_000, b"ok"), (3, 1_000_250, &[0x9c])];

/// A field: its first byte, its length, its name, what it holds, its colour class.
struct Field {
    at: usize,
    len: usize,
    name: &'static str,
    value: String,
    class: &'static str,
}

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

fn le_u16(b: &[u8], at: usize) -> anyhow::Result<u16> {
    let bytes = b.get(at..at + 2).context("a u16 inside the block")?;
    Ok(u16::from_le_bytes(bytes.try_into()?))
}

fn le_u32(b: &[u8], at: usize) -> anyhow::Result<u32> {
    let bytes = b.get(at..at + 4).context("a u32 inside the block")?;
    Ok(u32::from_le_bytes(bytes.try_into()?))
}

fn le_u64(b: &[u8], at: usize) -> anyhow::Result<u64> {
    let bytes = b.get(at..at + 8).context("a u64 inside the block")?;
    Ok(u64::from_le_bytes(bytes.try_into()?))
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
    ensure!(le_u16(&bytes, 6)? == block.flags, "flags at 6");
    ensure!(le_u32(&bytes, 8)? == u32::try_from(len)?, "length at 8");
    ensure!(le_u32(&bytes, 12)? == block.record_count, "records at 12");
    ensure!(le_u64(&bytes, 16)? == SEQ, "seq at 16");
    ensure!(le_u32(&bytes, 24)? == block.session.get(), "session at 24");

    let mut f = vec![
        Field {
            at: 0,
            len: 4,
            name: "magic",
            value: "WWTP".into(),
            class: "ink",
        },
        Field {
            at: 4,
            len: 1,
            name: "ver",
            value: block.version.to_string(),
            class: "blue",
        },
        Field {
            at: 5,
            len: 1,
            name: "kind",
            value: format!("{}", bytes.get(5).context("kind")?),
            class: "blue",
        },
        Field {
            at: 6,
            len: 2,
            name: "flags",
            value: format!("{:#x}", block.flags),
            class: "purple",
        },
        Field {
            at: 8,
            len: 4,
            name: "length",
            value: len.to_string(),
            class: "green",
        },
        Field {
            at: 12,
            len: 4,
            name: "records",
            value: block.record_count.to_string(),
            class: "green",
        },
        Field {
            at: 16,
            len: 8,
            name: "seq",
            value: SEQ.to_string(),
            class: "amber",
        },
        Field {
            at: 24,
            len: 4,
            name: "session",
            value: format!("{:08x}", block.session.get()),
            class: "teal",
        },
        Field {
            at: 28,
            len: 4,
            name: "head CRC",
            value: "0".into(),
            class: "ink",
        },
    ];
    ensure!(le_u32(&bytes, 28)? == 0, "no head CRC in a records block");

    // the body: a clock table, then per record a head, a time delta, a length and the item
    let mut body = Vec::new();
    let mut at = HEADER_LEN;
    let mut push = |f: &mut Vec<Field>, part: Vec<u8>, name, value: String, class| {
        let len = part.len();
        f.push(Field {
            at,
            len,
            name,
            value,
            class,
        });
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
    f.push(Field {
        at: len - CRC_LEN,
        len: CRC_LEN,
        name: "CRC-32C",
        value: "of bytes 0..".to_owned() + &(len - CRC_LEN).to_string(),
        class: "accent",
    });
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
    let (cell, ch, left, top, pitch) = (34.0, 24.0, 92.0, 30.0, 88.0);
    let width = left + 16.0 * cell + 16.0;
    let height = top + 4.0 * pitch - 12.0;
    let mut s = Svg::new(
        width,
        height,
        &format!(
            "A records block of {len} bytes: a {HEADER_LEN}-byte header, a body of a clock table and two records, a CRC-32C"
        ),
    );
    for (r, &(from, to, title)) in rows.iter().enumerate() {
        let y = top + pitch * f64::from(u8::try_from(r)?);
        s.text(left - 10.0, y + 16.5, "end", "soft mono", &from.to_string());
        if !title.is_empty() {
            s.text(left - 40.0, y + 16.5, "end", "strong", title);
        }
        for i in from..to {
            let field = fields
                .iter()
                .find(|f| (f.at..f.at + f.len).contains(&i))
                .context("every byte in a field")?;
            let x = left + cell * f64::from(u16::try_from(i - from)?);
            s.rect(
                (x + 1.0, y),
                (cell - 2.0, ch),
                3.0,
                &format!("tint-{}", field.class),
            );
            s.text(
                x + cell / 2.0,
                y + 16.5,
                "middle",
                "mono",
                &format!("{:02x}", bytes.get(i).context("byte")?),
            );
        }
        for field in fields.iter().filter(|f| f.at >= from && f.at < to) {
            let x0 = left + cell * f64::from(u16::try_from(field.at - from)?) + 2.0;
            let x1 = x0 + cell * f64::from(u16::try_from(field.len)?) - 4.0;
            let cx = (x0 + x1) / 2.0;
            s.bracket_under(x0, x1, y + ch + 6.0, field.class);
            s.text(cx, y + ch + 21.0, "middle", field.class, field.name);
            s.text(cx, y + ch + 36.0, "middle", "soft small", &field.value);
        }
    }
    // the two records over the body row
    let body_y = top + 2.0 * pitch;
    let mut x = left + cell * 2.0;
    for (i, (_, _, item)) in RECORDS.iter().enumerate() {
        let n = fields
            .iter()
            .filter(|f| f.at >= HEADER_LEN && f.at < body_end && f.name != "clocks")
            .skip(i * 4)
            .take(4)
            .map(|f| f.len)
            .sum::<usize>();
        ensure!(n > item.len());
        let w = cell * f64::from(u16::try_from(n)?);
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
