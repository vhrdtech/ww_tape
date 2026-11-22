//! The layout figures of the format reference, byte by byte, from the first piece of the drive's recording
//! ([ww_tape_example]) at 1/1000 of its data: `chunk_head.svg`, the head of the first `phase_a` chunk;
//! `index_entry.svg`, that chunk's entry in the final index; `tail.svg`, the piece's tail. Every field drawn is
//! checked against what [ww_tape] reads from the same bytes.

use anyhow::{Context, ensure};
use ww_tape::index::IndexBlock;
use ww_tape::{
    Block, BlockKind, CRC_LEN, CloseReason, ColumnKind, FileBlocks, HEADER_LEN, TAIL_LEN, Tail,
};
use ww_tape_example::{Drive, key};

use crate::OrDebug;
use crate::layout::{Field, Grid, Row, hex_start, le};
use crate::svg::Svg;

/// The scale the drive is drawn at, as the other figures of the drive draw it.
const SCALE: u32 = 1000;
/// Bytes of the payload `chunk_head.svg` shows after the head.
const PAYLOAD_SHOWN: usize = 16;
/// The numbers the captions in `docs/format.md` state: a changed recording fails here until they follow.
const CAPTION_SAMPLES: u32 = 1500;
const CAPTION_PAYLOAD_AT: usize = 115;
const CAPTION_ENTRY_AT: usize = 396;
const CAPTION_PIECE_LEN: u64 = 8192;
const CAPTION_CLOSE_AT: u64 = 6308;

/// The bytes of the drive's first piece.
fn first_piece() -> anyhow::Result<Vec<u8>> {
    let drive = Drive::scaled(SCALE)
        .context("1000 divides the drive's rates")?
        .minutes(1);
    let recording = drive.record()?;
    recording
        .pieces
        .into_iter()
        .next()
        .context("the first piece")
}

/// Every block of `piece` with its offset, walked from the start.
fn blocks(piece: &[u8]) -> anyhow::Result<Vec<(usize, Vec<u8>)>> {
    let mut walk = FileBlocks::new(piece);
    let mut at = 0usize;
    let mut out = Vec::new();
    while let Some(b) = walk.next_block() {
        let bytes = b.or_debug()?.bytes().to_vec();
        ensure!(
            piece.get(at..at + bytes.len()) == Some(bytes.as_slice()),
            "the blocks of a closed piece lie back to back"
        );
        let n = bytes.len();
        out.push((at, bytes));
        at += n;
    }
    ensure!(at == piece.len(), "the blocks fill the piece");
    Ok(out)
}

/// The header's fields, as `block.svg` names them, checked against [Block::read].
fn header(bytes: &[u8], count_name: &str, head_crc: bool) -> anyhow::Result<Vec<Field>> {
    let block = Block::read(bytes).or_debug()?;
    ensure!(le(bytes, 6, 2)? == u64::from(block.flags), "flags at 6");
    ensure!(
        le(bytes, 8, 4)? == u64::try_from(bytes.len())?,
        "length at 8"
    );
    ensure!(
        le(bytes, 12, 4)? == u64::from(block.record_count),
        "count at 12"
    );
    ensure!(le(bytes, 16, 8)? == block.seq, "seq at 16");
    ensure!(
        le(bytes, 24, 4)? == u64::from(block.session.get()),
        "session at 24"
    );
    let crc = le(bytes, 28, 4)?;
    ensure!(
        head_crc == (crc != 0),
        "a head CRC only where the kind has one"
    );
    Ok(vec![
        Field::new(0, 4, "magic", "WWTP".into(), "ink"),
        Field::new(4, 1, "ver", block.version.to_string(), "blue"),
        Field::new(5, 1, "kind", le(bytes, 5, 1)?.to_string(), "blue"),
        Field::new(6, 2, "flags", format!("{:#x}", block.flags), "purple"),
        Field::new(8, 4, "length", bytes.len().to_string(), "green"),
        Field::new(12, 4, count_name, block.record_count.to_string(), "green"),
        Field::new(16, 8, "seq", block.seq.to_string(), "amber"),
        Field::new(
            24,
            4,
            "session",
            format!("{:08x}", block.session.get()),
            "teal",
        ),
        Field::new(
            28,
            4,
            "head CRC",
            if head_crc {
                format!("{crc:08x}")
            } else {
                "0".into()
            },
            "ink",
        ),
    ])
}

/// Rows of 16 bytes over `from..to`, the first titled `title`.
fn rows_of<'a>(from: usize, to: usize, title: &'a str) -> Vec<Row<'a>> {
    (from..to)
        .step_by(16)
        .enumerate()
        .map(|(i, at)| (at, (at + 16).min(to), if i == 0 { title } else { "" }))
        .collect()
}

/// The first `phase_a` chunk of the piece: its offset and bytes.
fn phase_chunk(piece: &[u8]) -> anyhow::Result<(usize, Vec<u8>)> {
    let id = key("phase_a").id();
    blocks(piece)?
        .into_iter()
        .find(|(_, b)| {
            Block::read(b)
                .ok()
                .and_then(|b| b.chunk())
                .is_some_and(|c| c.head().kind == BlockKind::Series && c.head().track_id == id)
        })
        .context("a chunk of phase_a")
}

/// Check the fixed part of the head in `bytes` against what [ww_tape] reads, and the numbers the caption states.
fn check_head(bytes: &[u8], chunk: &ww_tape::Chunk<'_>) -> anyhow::Result<()> {
    let head = chunk.head();
    let grid = head.grid.context("a series chunk's grid")?;
    let payload_at = chunk.payload_offset();
    let columns = chunk.column_count();
    ensure!(
        head.count == CAPTION_SAMPLES,
        "format.md's caption: {CAPTION_SAMPLES} samples, the payload at {CAPTION_PAYLOAD_AT}"
    );
    ensure!(
        payload_at == CAPTION_PAYLOAD_AT,
        "format.md's caption: {CAPTION_SAMPLES} samples, the payload at {CAPTION_PAYLOAD_AT}"
    );
    ensure!(
        head.first_item.get() == 0,
        "format.md's caption: the grid from sample 0 at tick 0"
    );
    ensure!(
        grid.grid_index == 0,
        "format.md's caption: the grid from sample 0 at tick 0"
    );
    ensure!(
        grid.grid_tick == 0,
        "format.md's caption: the grid from sample 0 at tick 0"
    );

    ensure!(
        le(bytes, 32, 4)? == u64::try_from(payload_at)?,
        "payload offset at 32"
    );
    ensure!(
        le(bytes, 36, 4)? == u64::from(head.track_number.get()),
        "track number at 36"
    );
    ensure!(le(bytes, 40, 8)? == head.track_id.get(), "track id at 40");
    ensure!(
        le(bytes, 48, 8)? == head.first_item.get(),
        "first item at 48"
    );
    ensure!(
        le(bytes, 56, 8)?.cast_signed() == head.first_tick,
        "first tick at 56"
    );
    ensure!(
        le(bytes, 64, 8)?.cast_signed() == head.last_tick,
        "last tick at 64"
    );
    ensure!(
        le(bytes, 72, 8)?.cast_signed() == head.latest_end,
        "latest end at 72"
    );
    ensure!(
        1usize << le(bytes, 80, 1)? == chunk.segment_len(),
        "segment shift at 80"
    );
    ensure!(
        usize::try_from(le(bytes, 81, 1)?)? == columns,
        "columns at 81"
    );
    ensure!(le(bytes, 84, 8)? == grid.grid_index, "grid index at 84");
    ensure!(
        le(bytes, 92, 8)?.cast_signed() == grid.grid_tick,
        "grid tick at 92"
    );
    Ok(())
}

pub fn chunk_head() -> anyhow::Result<String> {
    let piece = first_piece()?;
    let (_, bytes) = phase_chunk(&piece)?;
    let block = Block::read(&bytes).or_debug()?;
    let chunk = block.chunk().context("a chunk")?;
    let head = chunk.head();
    let grid = head.grid.context("a series chunk's grid")?;
    let payload_at = chunk.payload_offset();
    let columns = chunk.column_count();
    let segments = chunk.segment_count();
    let scale_len = usize::try_from(le(&bytes, 100, 1)?)?;
    check_head(&bytes, &chunk)?;
    ensure!(columns == 1, "phase_a's chunk has one column, its samples");
    let entries_at = 101 + scale_len;
    let crcs_at = entries_at + 8 * columns;
    let zeros_at = crcs_at + 4 * segments;
    ensure!(
        zeros_at <= payload_at,
        "the head's tables end before the payload"
    );
    ensure!(
        bytes
            .get(zeros_at..payload_at)
            .is_some_and(|z| z.iter().all(|b| *b == 0)),
        "zeros up to the payload"
    );
    let column = chunk.column(0).context("the samples column")?;
    ensure!(column.kind == ColumnKind::Samples, "column kind");
    ensure!(le(&bytes, entries_at, 1)? == 6, "column kind");
    ensure!(
        le(&bytes, entries_at + 1, 1)? == u64::from(column.codec.0),
        "column codec"
    );
    ensure!(
        le(&bytes, entries_at + 4, 4)? == u64::try_from(column.offset)?,
        "column offset"
    );

    let mut fields = header(&bytes, "samples", true)?;
    fields.extend([
        Field::new(32, 4, "payload at", payload_at.to_string(), "green"),
        Field::new(36, 4, "track", head.track_number.get().to_string(), "blue"),
        Field::new(
            40,
            8,
            "track id",
            format!("{:016x}", head.track_id.get()),
            "blue",
        ),
        Field::new(
            48,
            8,
            "first item",
            head.first_item.get().to_string(),
            "amber",
        ),
        Field::new(56, 8, "first tick", head.first_tick.to_string(), "amber"),
        Field::new(64, 8, "last tick", head.last_tick.to_string(), "amber"),
        Field::new(72, 8, "latest end", head.latest_end.to_string(), "amber"),
        Field::new(80, 1, "seg", le(&bytes, 80, 1)?.to_string(), "purple"),
        Field::new(81, 1, "cols", columns.to_string(), "purple"),
        Field::new(82, 1, "time", le(&bytes, 82, 1)?.to_string(), "purple"),
        Field::new(83, 1, "end", le(&bytes, 83, 1)?.to_string(), "purple"),
        Field::new(84, 8, "grid index", grid.grid_index.to_string(), "teal"),
        Field::new(92, 8, "grid tick", grid.grid_tick.to_string(), "teal"),
        Field::new(100, 1, "len", scale_len.to_string(), "teal"),
        Field::new(101, scale_len, "scale", "10⁻³".into(), "teal"),
        Field::new(
            entries_at,
            1,
            "kind",
            le(&bytes, entries_at, 1)?.to_string(),
            "green",
        ),
        Field::new(
            entries_at + 1,
            1,
            "codec",
            format!("{}", column.codec.0),
            "green",
        ),
        Field::new(
            entries_at + 2,
            2,
            "param",
            le(&bytes, entries_at + 2, 2)?.to_string(),
            "green",
        ),
        Field::new(
            entries_at + 4,
            4,
            "offset",
            column.offset.to_string(),
            "green",
        ),
        Field::new(
            crcs_at,
            4 * segments,
            "segment CRC",
            format!("{segments} × CRC-32C"),
            "accent",
        ),
    ]);
    if payload_at > zeros_at {
        fields.push(Field::new(
            zeros_at,
            payload_at - zeros_at,
            "zeros",
            String::new(),
            "ink",
        ));
    }
    let shown = (payload_at + PAYLOAD_SHOWN).min(bytes.len() - CRC_LEN);
    fields.push(Field::new(
        payload_at,
        shown - payload_at,
        "samples column",
        format!("codec {}, its first bytes", column.codec.0),
        "blue",
    ));

    let mut rows = rows_of(0, HEADER_LEN, "header");
    rows.extend(rows_of(HEADER_LEN, payload_at, "head"));
    rows.extend(rows_of(payload_at, shown, "payload"));
    let g = Grid::BYTES;
    let mut s = Svg::new(
        g.width(),
        g.height(rows.len()),
        &format!(
            "The head of a series chunk of {} samples, {payload_at} bytes: the block header, where the payload starts, the track, the first item and ticks, the grid and scale, one column entry, {segments} segment CRC, then the first bytes of the samples column",
            head.count
        ),
    );
    g.draw(&mut s, &bytes, 0, &fields, &rows)?;
    Ok(s.finish())
}

pub fn index_entry() -> anyhow::Result<String> {
    let piece = first_piece()?;
    let (chunk_at, chunk) = phase_chunk(&piece)?;
    let all = blocks(&piece)?;
    // the final index: the last index block of the piece
    let (_, index) = all
        .iter()
        .rev()
        .find(|(_, b)| b.get(5) == Some(&7))
        .context("a final index")?;
    let block = Block::read(index).or_debug()?;
    let ix = IndexBlock::read(&block).or_debug()?;
    let entries_at = usize::try_from(le(index, 32, 4)?)?;
    let entry_len = usize::try_from(le(index, 58, 2)?)?;
    ensure!(entry_len == 120, "entries of 120 bytes");
    let (i, entry) = ix
        .entries()
        .enumerate()
        .find(|(_, e)| e.offset == u64::try_from(chunk_at).unwrap_or(u64::MAX))
        .context("the chunk's entry")?;
    let from = entries_at + i * entry_len;
    ensure!(
        from == CAPTION_ENTRY_AT,
        "format.md's caption: the entry at byte {CAPTION_ENTRY_AT}"
    );
    let e = index
        .get(from..from + entry_len)
        .context("the entry's bytes")?;
    ensure!(le(e, 0, 8)? == entry.offset, "offset at 0");
    ensure!(le(e, 8, 4)? == u64::from(entry.len), "length at 8");
    ensure!(
        entry.len as usize == chunk.len(),
        "the entry's length is the chunk's"
    );
    ensure!(le(e, 12, 1)? == 4, "kind 4, a series chunk, at 12");
    ensure!(le(e, 16, 8)? == entry.track_id.get(), "track id at 16");
    ensure!(
        le(e, 24, 4)? == u64::from(entry.number),
        "track number at 24"
    );
    ensure!(le(e, 32, 8)? == entry.first_item.get(), "first item at 32");
    ensure!(le(e, 40, 8)? == entry.count, "items at 40");
    ensure!(
        le(e, 48, 8)?.cast_signed() == entry.first_tick,
        "first tick at 48"
    );
    ensure!(
        le(e, 56, 8)?.cast_signed() == entry.last_tick,
        "last tick at 56"
    );
    ensure!(
        le(e, 64, 8)?.cast_signed() == entry.latest_end,
        "latest end at 64"
    );
    ensure!(entry.range.is_none(), "no range relation");
    ensure!(le(e, 72, 16)? == 0, "no range relation");
    ensure!(
        e.get(88..120) == Some(entry.unit.0.as_slice()),
        "unit hash at 88"
    );

    let fields = vec![
        Field::new(0, 8, "offset", entry.offset.to_string(), "green"),
        Field::new(8, 4, "length", entry.len.to_string(), "green"),
        Field::new(12, 1, "kind", le(e, 12, 1)?.to_string(), "blue"),
        Field::new(13, 1, "level", entry.level.to_string(), "purple"),
        Field::new(14, 1, "tree", entry.tree.to_string(), "purple"),
        Field::new(15, 1, "flags", le(e, 15, 1)?.to_string(), "purple"),
        Field::new(
            16,
            8,
            "track id",
            format!("{:016x}", entry.track_id.get()),
            "blue",
        ),
        Field::new(24, 4, "track", entry.number.to_string(), "blue"),
        Field::new(28, 4, "reserved", "0".into(), "ink"),
        Field::new(
            32,
            8,
            "first item",
            entry.first_item.get().to_string(),
            "amber",
        ),
        Field::new(40, 8, "items", entry.count.to_string(), "amber"),
        Field::new(48, 8, "first tick", entry.first_tick.to_string(), "amber"),
        Field::new(56, 8, "last tick", entry.last_tick.to_string(), "amber"),
        Field::new(64, 8, "latest end", entry.latest_end.to_string(), "amber"),
        Field::new(72, 8, "range min", "0: none".into(), "ink"),
        Field::new(80, 8, "range max", "0: none".into(), "ink"),
        Field::new(88, 32, "unit hash", hex_start(&entry.unit.0, 8), "accent"),
    ];
    let rows = rows_of(0, entry_len, "entry");
    let g = Grid::BYTES;
    let mut s = Svg::new(
        g.width(),
        g.height(rows.len()),
        &format!(
            "The {entry_len}-byte index entry of a series chunk: its offset, length and kind, its track, its first item, item count and ticks, no range relation, and its unit hash"
        ),
    );
    g.draw(&mut s, e, entries_at + i * entry_len, &fields, &rows)?;
    Ok(s.finish())
}

pub fn tail() -> anyhow::Result<String> {
    let piece = first_piece()?;
    let bytes = piece
        .get(piece.len() - TAIL_LEN..)
        .context("the tail")?
        .to_vec();
    let t = Tail::read(&bytes).or_debug()?;
    let ident = t.ident.context("a piece of a chain")?;
    ensure!(le(&bytes, 32, 8)? == t.piece_len, "piece length at 32");
    ensure!(
        t.piece_len == u64::try_from(piece.len())?,
        "the tail states the piece's length"
    );
    ensure!(le(&bytes, 40, 8)? == t.close_offset, "close offset at 40");
    ensure!(
        le(&bytes, 48, 4)? == u64::from(t.close_len),
        "close length at 48"
    );
    ensure!(t.reason == CloseReason::Closed, "closed, at 52");
    ensure!(le(&bytes, 52, 1)? == 1, "closed, at 52");
    ensure!(
        bytes.get(56..88) == Some(t.hash.0.as_slice()),
        "content hash at 56"
    );
    ensure!(
        bytes.get(88..104) == Some(ident.id.0.as_slice()),
        "piece id at 88"
    );
    let next = t.next.map(|n| n.0).unwrap_or_default();
    ensure!(
        bytes.get(104..120) == Some(next.as_slice()),
        "next piece id at 104"
    );
    ensure!(
        bytes.get(120..152) == Some(ident.owner.0.as_slice()),
        "owner at 120"
    );
    ensure!(le(&bytes, 152, 8)? == ident.seq, "sequence at 152");
    ensure!(
        t.piece_len == CAPTION_PIECE_LEN,
        "format.md's caption: a piece of {CAPTION_PIECE_LEN} bytes, its close at {CAPTION_CLOSE_AT}, sequence 0"
    );
    ensure!(
        t.close_offset == CAPTION_CLOSE_AT,
        "format.md's caption: a piece of {CAPTION_PIECE_LEN} bytes, its close at {CAPTION_CLOSE_AT}, sequence 0"
    );
    ensure!(
        ident.seq == 0,
        "format.md's caption: a piece of {CAPTION_PIECE_LEN} bytes, its close at {CAPTION_CLOSE_AT}, sequence 0"
    );
    ensure!(
        le(&bytes, 53, 1)? == 1,
        "format.md's caption: flags 1, the piece has a head"
    );

    let mut fields = header(&bytes, "count", false)?;
    fields.extend([
        Field::new(32, 8, "piece length", t.piece_len.to_string(), "green"),
        Field::new(40, 8, "close at", t.close_offset.to_string(), "green"),
        Field::new(48, 4, "close length", t.close_len.to_string(), "green"),
        Field::new(52, 1, "why", "1".into(), "purple"),
        Field::new(53, 1, "flags", le(&bytes, 53, 1)?.to_string(), "purple"),
        Field::new(54, 2, "reserved", "0".into(), "ink"),
        Field::new(56, 32, "content hash", hex_start(&t.hash.0, 8), "accent"),
        Field::new(88, 16, "piece id", hex_start(&ident.id.0, 6), "teal"),
        Field::new(104, 16, "next piece id", hex_start(&next, 6), "teal"),
        Field::new(120, 32, "owner id", hex_start(&ident.owner.0, 8), "teal"),
        Field::new(152, 8, "sequence", ident.seq.to_string(), "amber"),
        Field::new(
            TAIL_LEN - CRC_LEN,
            CRC_LEN,
            "CRC-32C",
            format!("of bytes 0..{}", TAIL_LEN - CRC_LEN),
            "accent",
        ),
    ]);
    let mut rows = rows_of(0, HEADER_LEN, "header");
    rows.extend(rows_of(HEADER_LEN, TAIL_LEN, "body"));
    let g = Grid::BYTES;
    let mut s = Svg::new(
        g.width(),
        g.height(rows.len()),
        &format!(
            "The {TAIL_LEN}-byte tail of a closed piece of {} bytes: the piece's length, where its close is, why it closed, its content hash, its id, the next piece's id, its owner and its sequence number",
            t.piece_len
        ),
    );
    g.draw(&mut s, &bytes, 0, &fields, &rows)?;
    Ok(s.finish())
}
