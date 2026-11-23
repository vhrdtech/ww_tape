//! `piece.svg`: a closed piece of a chain, every block to scale on rows of 1 KiB, the 4 KiB boundaries marked. The
//! piece is written by [PieceChain] into memory and walked back block by block.

use anyhow::{Context, ensure};
use ww_tape::chunk::ChunkHead;
use ww_tape::index::IndexPolicy;
use ww_tape::piece::{CloseBlock, close_len};
use ww_tape::{
    BlockKind, BlockWriter, ChainOptions, ClockTime, Codec, ColumnKind, ColumnSpec, CutPolicy,
    Definitions, EndMode, FLAG_REPEAT, FileBlocks, MemPieces, OwnerId, PieceChain, SyncPolicy,
    TrackRepeat, WriterOptions,
};
use ww_tape::{ClockNum, TrackNum};

use crate::OrDebug;
use crate::defs::{define_clock, define_track};
use crate::svg::Svg;

/// Track of the log lines (records blocks) and of the frames (events chunks), as the figure defines them.
const LOG: u32 = 1;
const FRAMES: u32 = 2;

/// The numbers the piece's data is written on.
#[derive(Clone, Copy)]
struct Numbers {
    clock: ClockNum,
    log: TrackNum,
    frames: TrackNum,
}
/// Bytes a row of the drawing holds.
const ROW: u64 = 1024;
/// [ww_tape::piece::PIECE_ALIGN] as a byte count.
const PIECE_ALIGN: u64 = ww_tape::piece::PIECE_ALIGN as u64;
/// [ww_tape::CRC_LEN] as a byte count.
const CRC_LEN: u64 = ww_tape::CRC_LEN as u64;

/// What a block is, as the drawing names and colours it.
#[derive(Copy, Clone, PartialEq, Eq)]
enum Part {
    Head,
    Puts,
    Records,
    Chunk,
    Checkpoint,
    FinalIndex,
    Close,
    Padding,
    Tail,
}

impl Part {
    fn name(self) -> &'static str {
        match self {
            Self::Head => "piece head",
            Self::Puts => "puts",
            Self::Records => "records",
            Self::Chunk => "events chunk",
            Self::Checkpoint => "checkpoint",
            Self::FinalIndex => "final index",
            Self::Close => "close",
            Self::Padding => "zero padding",
            Self::Tail => "tail",
        }
    }

    fn class(self) -> &'static str {
        match self {
            Self::Head | Self::Tail => "tint-ink",
            Self::Puts => "tint-purple",
            Self::Records | Self::Chunk => "tint-blue",
            Self::Checkpoint | Self::FinalIndex => "tint-amber",
            Self::Close => "tint-green",
            Self::Padding => "faint hatch",
        }
    }
}

/// A records block of one log line on the log track of `on`.
fn log_line(
    w: &ww_tape::TapeWriter<ww_tape::MemSink>,
    on: Numbers,
    n: i64,
) -> anyhow::Result<Vec<u8>> {
    let mut buf = vec![0u8; 1024];
    let mut b =
        BlockWriter::new(&mut buf, BlockKind::Records, w.next_id(), &[on.clock]).or_debug()?;
    let line = format!("{n:>4} ms: link up, 2 peers, queue {:>3}", n % 97).repeat(4);
    b.push(
        on.log,
        ClockTime::new(on.clock, n * 1_000_000),
        &[line.as_bytes()],
    )
    .or_debug()?;
    let len = b.finish();
    buf.truncate(len);
    Ok(buf)
}

/// Frames chunk `k` on the frames track of `on`: 96 items with raw start ticks.
fn frames(on: Numbers, k: u64) -> (ChunkHead, Vec<u8>) {
    let first = k * 96;
    let tick = |i: u64| i64::try_from((first + i) * 250_000).unwrap_or(i64::MAX);
    let starts: Vec<u8> = (0..96).flat_map(|i| tick(i).to_le_bytes()).collect();
    let head = ChunkHead {
        kind: BlockKind::Events,
        track_id: ww_tape::track_key(0xf1, &[ww_tape::Segment::Index(FRAMES)]).id(),
        track_number: on.frames,
        first_item: ww_tape::ItemNum::new(first),
        count: 96,
        first_tick: tick(0),
        last_tick: tick(95),
        latest_end: tick(95),
        time_mode: 0,
        end_mode: EndMode::Absent,
        grid: None,
    };
    (head, starts)
}

/// The piece's blocks: offset, length, what each is; the padding as a part of its own after the close's tracks.
/// A block, or the close's padding: offset, length, what it is.
type Span = (u64, u64, Part);

fn parts() -> anyhow::Result<(u64, Vec<Span>)> {
    let options = ChainOptions {
        tapes: ww_tape::PieceTapes::Each,
        writer: WriterOptions {
            sync: SyncPolicy::None,
            index: IndexPolicy {
                bytes: exact_units::Bytes::new(2 * 1024),
                interval: None,
                ..IndexPolicy::default()
            },
            producer: "figures".into(),
            ..WriterOptions::default()
        },
        cut: CutPolicy {
            bytes: exact_units::Bytes::new(0),
            chunks: exact_units::Count::new(0),
            interval: None,
        },
    };
    let mut defs = Definitions::new();
    let clock = define_clock(&mut defs)?;
    let on = Numbers {
        clock,
        log: define_track(&mut defs, LOG, clock, "log", TrackRepeat::Stream)?,
        frames: define_track(&mut defs, FRAMES, clock, "frames", TrackRepeat::Stream)?,
    };
    let mut chain = PieceChain::create_on(MemPieces::new(), OwnerId([0xf1; 32]), 0, &[], options)?;
    for k in 0..4 {
        let w = chain.writer()?;
        let line = log_line(w, on, i64::try_from(k)? * 40)?;
        w.write_block(&defs, &line)?;
        let (head, starts) = frames(on, k);
        let spec = ColumnSpec {
            kind: ColumnKind::Starts,
            codec: Codec::RAW,
            param: 0,
            len: starts.len(),
        };
        chain.write_chunk(&defs, &head, &[(spec, &starts)])?;
    }
    let tapes = chain.store().clone();
    chain.close()?;
    let bytes = tapes.get(&0)?.context("the closed piece")?.to_vec()?;
    let len = u64::try_from(bytes.len())?;
    ensure!(len % PIECE_ALIGN == 0, "a closed piece ends on 4 KiB");

    let mut walk = FileBlocks::new(&bytes[..]);
    let mut blocks = Vec::new();
    while let Some(b) = walk.next_block() {
        let b = b.or_debug()?;
        let (n, kind, flags) = (u64::try_from(b.len())?, b.kind, b.flags);
        // a close's padding: its length past a close of its tracks and removed pieces
        let pad = if kind == BlockKind::Close {
            let c = CloseBlock::read(&b).or_debug()?;
            n - u64::try_from(close_len(c.tracks().count(), c.removed().count()))?
        } else {
            0
        };
        blocks.push((walk.offset(), n, kind, flags, pad));
    }
    let last_data = blocks
        .iter()
        .rposition(|b| matches!(b.2, BlockKind::Records | BlockKind::Events))
        .context("data blocks")?;
    let mut out = Vec::new();
    for (i, &(at, n, kind, flags, pad)) in blocks.iter().enumerate() {
        let part = match kind {
            BlockKind::PieceHead => Part::Head,
            BlockKind::Records if flags & FLAG_REPEAT != 0 => Part::Puts,
            BlockKind::Records => Part::Records,
            BlockKind::Events => Part::Chunk,
            BlockKind::Index if i < last_data => Part::Checkpoint,
            BlockKind::Index => Part::FinalIndex,
            BlockKind::Close => {
                // the padding lies after the close's tracks, before its CRC
                out.push((at, n - pad - CRC_LEN, Part::Close));
                out.push((at + n - pad - CRC_LEN, pad, Part::Padding));
                out.push((at + n - CRC_LEN, CRC_LEN, Part::Close));
                continue;
            }
            BlockKind::Tail => Part::Tail,
            other => anyhow::bail!("a block of kind {other:?} in the drawn piece"),
        };
        out.push((at, n, part));
    }
    ensure!(
        out.last().map(|p| p.0 + p.1) == Some(len),
        "the blocks fill the piece"
    );
    Ok((len, out))
}

pub fn piece() -> anyhow::Result<String> {
    let (len, parts) = parts()?;
    let rows = len / ROW;
    let (left, top, row_w, row_h, pitch) = (64.0, 16.0, 600.0, 30.0, 40.0);
    let scale = row_w / f64::from(u32::try_from(ROW)?);
    let legend_y = top + pitch * f64::from(u32::try_from(rows)?) + 14.0;
    let width = left + row_w + 56.0;
    let mut s = Svg::new(
        width,
        legend_y + 62.0,
        &format!(
            "A closed piece of {} KiB on rows of 1 KiB: piece head, puts, records, events chunks, a checkpoint, the final index, the close padded with zeros, and the tail ending on the 4 KiB boundary",
            len / 1024
        ),
    );
    let y_of = |row: u64| top + pitch * f64::from(u32::try_from(row).unwrap_or(0));
    for row in 0..rows {
        let y = y_of(row);
        s.text(
            left - 8.0,
            y + 19.5,
            "end",
            "soft mono",
            &format!("{}", row * ROW),
        );
    }
    for &(at, n, part) in &parts {
        // a block may run over several rows
        let mut from = at;
        let end = at + n;
        let mut labelled = false;
        while from < end {
            let row = from / ROW;
            let to = end.min((row + 1) * ROW);
            let x = left + f64::from(u32::try_from(from - row * ROW)?) * scale;
            let w = f64::from(u32::try_from(to - from)?) * scale;
            let y = y_of(row);
            s.rect((x + 0.5, y), ((w - 1.0).max(0.6), row_h), 2.0, part.class());
            let name = part.name();
            let fits = w >= 7.0 * f64::from(u32::try_from(name.len())?) + 8.0;
            if fits && !labelled {
                s.text(x + w / 2.0, y + 19.5, "middle", "small", name);
                labelled = true;
            }
            from = to;
        }
    }
    // the 4 KiB boundaries: after every fourth row
    for b in 1..=len / PIECE_ALIGN {
        let y = y_of(b * PIECE_ALIGN / ROW) - (pitch - row_h) / 2.0;
        s.line((left - 4.0, y), (left + row_w + 4.0, y), "accent-line");
        s.text(
            left + row_w + 8.0,
            y + 4.0,
            "start",
            "accent small",
            &format!("{} KiB", b * PIECE_ALIGN / 1024),
        );
    }
    // the legend
    let legend = [
        (Part::Head, "piece head, tail"),
        (Part::Puts, "puts (track 0)"),
        (Part::Records, "records, chunk"),
        (Part::Checkpoint, "index"),
        (Part::Close, "close"),
        (Part::Padding, "zero padding"),
    ];
    for (i, (part, name)) in legend.into_iter().enumerate() {
        let col = f64::from(u8::try_from(i % 3)?);
        let line = f64::from(u8::try_from(i / 3)?);
        let x = left + col * 200.0;
        let y = legend_y + line * 22.0;
        s.rect((x, y), (16.0, 12.0), 2.0, part.class());
        s.text(x + 22.0, y + 10.5, "start", "small", name);
    }
    Ok(s.finish())
}
