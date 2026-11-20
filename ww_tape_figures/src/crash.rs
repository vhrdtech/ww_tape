//! `crash.svg` and `resume.svg`: a piece that [TapeWriter] writes, cut off by a crash in the middle of a block, read
//! back by [TapeReader] and resumed by [TapeWriter::resume_on]. The writer places a checkpoint every 4 KiB instead of
//! every 4 MiB and puts definitions again 8 KiB on instead of 32 MiB, so that several of each fit the drawing. The resumed piece is checked to end with the bytes of the piece
//! written without the crash.

use anyhow::{Context, ensure};
use exact_units::{Bytes, Count};
use ww_tape::chunk::ChunkHead;
use ww_tape::index::{IndexBlock, IndexPolicy};
use ww_tape::reader::Found;
use ww_tape::writer::NearPolicy;
use ww_tape::{
    BlockKind, BlockWriter, ClockTime, Codec, ColumnKind, ColumnSpec, Definitions, EndMode,
    FLAG_REPEAT, FLAG_WITH_NEXT, FileBlocks, Layout, MemSink, Recovery, StatePolicy, SyncPolicy,
    TapeReader, TapeSink, TapeWriter, TrackRepeat, WriterOptions,
};

use crate::OrDebug;
use crate::defs::{CLOCK, clock, track};
use crate::svg::Svg;

/// The drive's log lines (records blocks), its CAN frames (events chunks) and its mode (a state track).
const LOG: u32 = 1;
const FRAMES: u32 = 2;
const MODE: u32 = 5;
/// Rounds of the recording: a log line, a put of the mode, a chunk of frames.
const ROUNDS: u64 = 20;
/// Frames per chunk.
const FRAMES_PER_CHUNK: u32 = 96;

/// One call the producer makes.
#[derive(Copy, Clone, Debug)]
enum Call {
    Put(u64),
    Log(u64),
    Frames(u64),
}

/// The calls of the recording, in order.
fn calls() -> Vec<Call> {
    (0..ROUNDS)
        .flat_map(|k| [Call::Log(k), Call::Put(k), Call::Frames(k)])
        .collect()
}

/// The mode in force in round `k`: run, then fault.
fn mode(k: u64) -> &'static [u8] {
    if k < 14 {
        b"mode: run, 40 A limit"
    } else {
        b"mode: fault, overcurrent"
    }
}

fn definitions() -> anyhow::Result<Definitions> {
    let mut defs = Definitions::new();
    defs.define_all(&[
        clock(),
        track(LOG, "log", TrackRepeat::Stream),
        track(FRAMES, "can0", TrackRepeat::Stream),
        track(
            MODE,
            "mode",
            TrackRepeat::State(StatePolicy {
                near: Bytes::new(32 << 20),
                copies: Count::new(1),
            }),
        ),
    ])?;
    Ok(defs)
}

/// A new writer with the figure's options: a checkpoint every 4 KiB, definitions put again 8 KiB on.
fn writer() -> anyhow::Result<TapeWriter<MemSink>> {
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    w.set_producer("figures");
    w.set_layout(Layout::UNALIGNED.with_definitions(StatePolicy {
        near: Bytes::new(8 << 10),
        copies: Count::new(1),
    }))?;
    w.set_index_policy(IndexPolicy {
        bytes: 4 * 1024,
        interval: None,
        ..IndexPolicy::default()
    })?;
    w.set_near_policy(NearPolicy {
        copy_spacing: 2 * 1024,
        ..NearPolicy::default()
    });
    Ok(w)
}

/// Make `call` on `w`.
fn make(w: &mut TapeWriter<MemSink>, defs: &Definitions, call: Call) -> anyhow::Result<()> {
    let tick = |k: u64| i64::try_from(k * 100_000_000).unwrap_or(i64::MAX);
    match call {
        Call::Put(k) => {
            w.put(defs, MODE, tick(k), mode(k))?;
        }
        Call::Log(k) => {
            let mut buf = vec![0u8; 1024];
            let mut b =
                BlockWriter::new(&mut buf, BlockKind::Records, w.next_id(), &[CLOCK]).or_debug()?;
            // ten lines a block, 10 ms apart
            for j in 0..10 {
                let ms = k * 100 + j * 10;
                let line = format!("{ms:>5} ms: phase A {:>3} A, bus 48.{} V", 20 + k, j);
                let at = tick(k) + i64::try_from(j * 10_000_000)?;
                b.push(LOG, ClockTime::new(CLOCK, at), &[line.as_bytes()])
                    .or_debug()?;
            }
            let n = b.finish();
            w.write_block(defs, buf.get(..n).context("the log line's block")?)?;
        }
        Call::Frames(k) => {
            let first = k * u64::from(FRAMES_PER_CHUNK);
            let at = |i: u32| tick(k) + i64::from(i) * 1_000_000;
            let starts: Vec<u8> = (0..FRAMES_PER_CHUNK)
                .flat_map(|i| at(i).to_le_bytes())
                .collect();
            let head = ChunkHead {
                kind: BlockKind::Events,
                track_id: ww_tape::track_key(0xf1, &[ww_tape::Segment::Index(FRAMES)]),
                track_number: FRAMES,
                first_item: first,
                count: FRAMES_PER_CHUNK,
                first_tick: at(0),
                last_tick: at(FRAMES_PER_CHUNK - 1),
                latest_end: at(FRAMES_PER_CHUNK - 1),
                time_mode: 0,
                end_mode: EndMode::Absent,
                grid: None,
            };
            let spec = ColumnSpec {
                kind: ColumnKind::Starts,
                codec: Codec::RAW,
                param: 0,
                len: starts.len(),
            };
            w.write_chunk(defs, &head, &[(spec, &starts)])?;
        }
    }
    Ok(())
}

/// The recording written without a crash: its bytes, its options, and for each call the seq of the block of data it
/// belongs with (a put belongs with the block of data after it).
fn uninterrupted() -> anyhow::Result<(Vec<u8>, WriterOptions, Vec<u64>)> {
    let mut w = writer()?;
    let options = w.options();
    let defs = definitions()?;
    let mut seqs = Vec::new();
    for call in calls() {
        make(&mut w, &defs, call)?;
        seqs.push(w.next_seq());
    }
    // a put belongs with the block after it: the seq the next block of data takes
    let mut with = vec![0; seqs.len()];
    let mut next = w.next_seq();
    for ((call, with), seq) in calls().iter().zip(&mut with).zip(&seqs).rev() {
        if let Call::Put(_) = call {
            *with = next;
        } else {
            *with = seq.wrapping_sub(1);
            next = *with;
        }
    }
    let tape = w.sink().tape();
    w.close()?;
    Ok((tape.to_vec()?, options, with))
}

/// What a block of the drawing is.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Part {
    Puts,
    Log,
    Frames,
    Checkpoint,
    FinalIndex,
    Close,
    Tail,
    Torn,
}

impl Part {
    fn class(self) -> &'static str {
        match self {
            Self::Puts => "tint-purple",
            Self::Log | Self::Frames => "tint-blue",
            Self::Checkpoint => "tint-amber",
            Self::FinalIndex | Self::Close | Self::Tail => "tint-green",
            Self::Torn => "tint-accent",
        }
    }
}

/// A block: offset, length, what it is, its flags, and for a checkpoint the offset of the one before.
struct Span {
    at: u64,
    len: u64,
    part: Part,
    flags: u16,
    prev: Option<u64>,
}

/// The good blocks of `bytes` in file order.
fn spans(bytes: &[u8]) -> anyhow::Result<Vec<Span>> {
    let mut walk = FileBlocks::with_recovery(bytes, Recovery::Prefix);
    let mut out = Vec::new();
    while let Some(Ok(b)) = walk.next_block() {
        let (len, flags) = (u64::try_from(b.len())?, b.flags);
        let (part, prev) = match b.kind {
            BlockKind::Records if flags & FLAG_REPEAT != 0 => (Part::Puts, None),
            BlockKind::Records => (Part::Log, None),
            BlockKind::Events => (Part::Frames, None),
            BlockKind::Index => {
                let ix = IndexBlock::read(&b).or_debug()?;
                match ix.kind() {
                    ww_tape::index::IndexKind::Checkpoint => (Part::Checkpoint, ix.prev()),
                    ww_tape::index::IndexKind::Final => (Part::FinalIndex, None),
                }
            }
            BlockKind::Close => (Part::Close, None),
            BlockKind::Tail => (Part::Tail, None),
            other => anyhow::bail!("a block of kind {other:?} in the drawn piece"),
        };
        let at = walk.offset();
        out.push(Span {
            at,
            len,
            part,
            flags,
            prev,
        });
    }
    Ok(out)
}

/// The crash: what is on disk, where the reader stopped, and the resumed piece's bytes.
struct Crash {
    /// The piece written without the crash.
    whole: Vec<u8>,
    /// Its blocks.
    blocks: Vec<Span>,
    /// Bytes on disk after the crash: up to the middle of `torn`.
    cut: u64,
    /// The block the crash tore.
    torn: usize,
    /// Where the newest checkpoint starts, which the reader found.
    newest: u64,
    /// Bytes the reader read block by block, up to the end of the bytes.
    scanned: u64,
    /// Bytes the resume kept.
    kept: u64,
}

fn crash() -> anyhow::Result<Crash> {
    let (whole, options, with) = uninterrupted()?;
    let blocks = spans(&whole)?;
    // the crash tears the first chunk of frames past half the data that follows puts marked to go with it
    let data_end = blocks
        .iter()
        .filter(|b| matches!(b.part, Part::Log | Part::Frames))
        .map(|b| b.at + b.len)
        .max()
        .context("blocks of data")?;
    // of those, the one farthest from the checkpoint before it, so that the reader's scan crosses several blocks
    let checkpoint_before = |at: u64| {
        blocks
            .iter()
            .filter(|c| c.part == Part::Checkpoint && c.at < at)
            .map(|c| c.at + c.len)
            .max()
            .unwrap_or(0)
    };
    let torn = blocks
        .windows(2)
        .enumerate()
        .filter(|(_, w)| {
            matches!(w, [before, b] if matches!(b.part, Part::Frames | Part::Log)
                && before.part == Part::Puts
                && before.flags & FLAG_WITH_NEXT != 0
                && b.at > data_end / 2)
        })
        .filter_map(|(i, w)| w.get(1).map(|b| (i + 1, b.at - checkpoint_before(b.at))))
        .max_by_key(|&(_, d)| d)
        .map(|(i, _)| i)
        .context("a block of data after puts marked FLAG_WITH_NEXT")?;
    let b = blocks.get(torn).context("the torn block")?;
    let cut = b.at + b.len / 2;
    let disk = whole
        .get(..usize::try_from(cut)?)
        .context("the bytes on disk")?;

    let r = TapeReader::open(disk).or_debug()?;
    let info = r.pieces().first().context("the piece")?;
    ensure!(info.tail.is_none(), "an unclosed piece has no tail");
    ensure!(info.cut == Some(b.at), "the reader stops at the torn block");
    let Found::Checkpoint { scanned } = info.found else {
        anyhow::bail!(
            "the reader finds the piece by its newest checkpoint: {:?}",
            info.found
        );
    };
    let newest = blocks
        .iter()
        .filter(|s| s.part == Part::Checkpoint && s.at < cut)
        .map(|s| s.at)
        .max()
        .context("a checkpoint before the crash")?;

    let mut sink = MemSink::new();
    sink.write_all(disk)?;
    let (mut w, resumed) = TapeWriter::resume_on(sink, options)?;
    let before = blocks.get(torn - 1).context("the puts")?;
    ensure!(
        resumed.kept_bytes == before.at,
        "the resume keeps every byte before the puts that go with the torn block"
    );
    let next = w.next_seq();
    let defs = definitions()?;
    for (call, seq) in calls().into_iter().zip(with) {
        if seq >= next {
            make(&mut w, &defs, call)?;
        }
    }
    let tape = w.sink().tape();
    w.close()?;
    ensure!(
        tape.to_vec()? == whole,
        "the resumed piece ends with the bytes of the piece written without the crash"
    );
    Ok(Crash {
        whole,
        blocks,
        cut,
        torn,
        newest,
        scanned,
        kept: resumed.kept_bytes,
    })
}

/// Drawing scale: a view of the piece fills a row of [ROW_W] units after [LEFT].
const LEFT: f64 = 24.0;
const ROW_W: f64 = 640.0;
const ROW_H: f64 = 30.0;

/// The bytes `from..to` of the piece, drawn across a row.
#[derive(Copy, Clone)]
struct View {
    from: u64,
    to: u64,
    left: f64,
    width: f64,
}

impl View {
    fn new(from: u64, to: u64) -> Self {
        Self {
            from,
            to,
            left: LEFT,
            width: ROW_W,
        }
    }

    fn x(&self, at: u64) -> f64 {
        let at = at.clamp(self.from, self.to);
        self.left + (at - self.from) as f64 / (self.to - self.from) as f64 * self.width
    }
}

impl Part {
    fn name(self) -> &'static str {
        match self {
            Self::Puts => "puts",
            Self::Log => "log",
            Self::Frames => "frames",
            Self::Checkpoint => "checkpoint",
            Self::FinalIndex => "final index",
            Self::Close => "close",
            Self::Tail => "tail",
            Self::Torn => "torn",
        }
    }
}

/// Draw the blocks up to byte `end` that `v` shows, on a row at `y`, named where the name fits; the torn block, when
/// given, only up to `cut`, its whole length dashed.
fn row(s: &mut Svg, blocks: &[Span], end: u64, v: View, y: f64, torn: Option<(usize, u64)>) {
    for (i, b) in blocks.iter().enumerate() {
        if b.at >= end.min(v.to) || b.at + b.len <= v.from {
            continue;
        }
        let (x0, x1) = (v.x(b.at), v.x(b.at + b.len));
        let (part, xe) = match torn {
            Some((t, cut)) if t == i => {
                s.path(
                    &format!(
                        "M{:.1} {y:.1} h{:.1} v{ROW_H} h{:.1} Z",
                        x0 + 0.5,
                        x1 - x0 - 1.0,
                        -(x1 - x0 - 1.0)
                    ),
                    "thin dash",
                );
                (Part::Torn, v.x(cut))
            }
            _ => (b.part, x1),
        };
        let w = (xe - x0 - 1.0).max(1.0);
        s.rect((x0 + 0.5, y), (w, ROW_H), 1.0, part.class());
        let name = part.name();
        if w >= 6.4 * name.len() as f64 + 6.0 {
            s.text(x0 + 0.5 + w / 2.0, y + 19.5, "middle", "small", name);
        }
    }
}

fn legend(s: &mut Svg, y: f64, items: &[(&str, &str)]) {
    let mut x = LEFT;
    for (class, name) in items {
        s.rect((x, y), (16.0, 12.0), 2.0, class);
        s.text(x + 22.0, y + 10.5, "start", "small", name);
        x += 30.0 + 6.6 * name.len() as f64;
    }
}

fn kib(n: u64) -> String {
    format!("{:.1} KiB", n as f64 / 1024.0)
}

/// `crash.svg`: the piece on disk after the crash, and how the reader finds its end.
pub fn crash_svg() -> anyhow::Result<String> {
    let c = crash()?;
    let torn = c.blocks.get(c.torn).context("the torn block")?;
    // the whole piece on disk and a little of what was never written; below it the end, from the newest checkpoint
    let all = View::new(0, c.cut + c.cut / 10);
    let end = View::new(c.newest, c.cut + (c.cut - c.newest) / 6);
    let (top, row_all, row_end) = (20.0, 96.0, 206.0);
    let mut s = Svg::new(
        LEFT * 2.0 + ROW_W,
        row_end + 136.0,
        &format!(
            "An unclosed piece of {} after a crash: it has no tail; each checkpoint points back to the one before; the reader starts at the newest checkpoint and scans the {} of blocks after it up to the torn block, whose CRC fails",
            kib(c.cut),
            kib(c.scanned)
        ),
    );
    s.text(LEFT, top, "start", "strong", "on disk after the crash");
    s.text(
        LEFT + ROW_W,
        top,
        "end",
        "soft small",
        "arcs: each checkpoint points back to the one before",
    );
    row(
        &mut s,
        &c.blocks,
        c.cut,
        all,
        row_all,
        Some((c.torn, c.cut)),
    );
    let xc = all.x(c.cut);
    s.rect(
        (xc + 2.0, row_all),
        (LEFT + ROW_W - xc - 2.0, ROW_H),
        1.0,
        "faint",
    );
    s.text(
        (xc + LEFT + ROW_W) / 2.0,
        row_all + 19.5,
        "middle",
        "soft small",
        "no tail",
    );
    for b in c
        .blocks
        .iter()
        .filter(|b| b.part == Part::Checkpoint && b.at < c.cut)
    {
        let Some(prev) = b.prev else { continue };
        let prev_len = c.blocks.iter().find(|p| p.at == prev).map_or(0, |p| p.len);
        let (xa, xb) = (all.x(b.at + b.len / 2), all.x(prev + prev_len / 2));
        s.path(
            &format!(
                "M{xa:.1} {:.1} C{xa:.1} {:.1} {xb:.1} {:.1} {xb:.1} {:.1}",
                row_all - 2.0,
                row_all - 30.0,
                row_all - 30.0,
                row_all - 7.0
            ),
            "thin",
        );
        s.path(
            &format!("M{xb:.1} {:.1} l-3.5 -6 h7 Z", row_all - 1.5),
            "thin head",
        );
    }
    // the stretch the lower row shows
    let (z0, z1) = (all.x(end.from), all.x(end.to));
    s.bracket_under(z0, z1, row_all + ROW_H + 8.0, "thin");
    s.path(
        &format!(
            "M{z0:.1} {:.1} L{:.1} {:.1} M{z1:.1} {:.1} L{:.1} {:.1}",
            row_all + ROW_H + 10.0,
            LEFT,
            row_end - 34.0,
            row_all + ROW_H + 10.0,
            LEFT + ROW_W,
            row_end - 34.0
        ),
        "thin dash",
    );
    s.text(
        LEFT,
        row_end - 14.0,
        "start",
        "small",
        "from the newest checkpoint on",
    );

    row(
        &mut s,
        &c.blocks,
        c.cut,
        end,
        row_end,
        Some((c.torn, c.cut)),
    );
    let xc = end.x(c.cut);
    s.rect(
        (xc + 2.0, row_end),
        (LEFT + ROW_W - xc - 2.0, ROW_H),
        1.0,
        "faint",
    );
    let (xn, xt) = (end.x(c.newest), end.x(torn.at));
    let y = row_end + ROW_H + 16.0;
    s.bracket_under(end.x(c.cut - c.scanned).max(xn), xc, y, "thin");
    s.text(
        xc,
        y + 18.0,
        "end",
        "small",
        &format!("scanned block by block: {}", kib(c.scanned)),
    );
    s.text(
        xt + (xc - xt) / 2.0,
        y + 36.0,
        "end",
        "accent small",
        "the torn block fails its CRC: the end of the piece",
    );
    legend(
        &mut s,
        row_end + 104.0,
        &[
            ("tint-purple", "puts"),
            ("tint-blue", "log lines, frames"),
            ("tint-amber", "checkpoint"),
            ("tint-accent", "torn block"),
        ],
    );
    Ok(s.finish())
}

/// `resume.svg`: the end of the piece written without a crash, the bytes the crash left, and the resumed piece.
pub fn resume_svg() -> anyhow::Result<String> {
    let c = crash()?;
    let len = u64::try_from(c.whole.len())?;
    // the three rows agree before the newest checkpoint and go on alike after a few blocks: they show the bytes
    // between, with a stub for the rest at either end
    let mut v = View::new(c.newest, (c.newest + 3 * (c.cut - c.newest)).min(len));
    v.left = LEFT + 40.0;
    v.width = ROW_W - 80.0;
    let rows = [44.0, 140.0, 256.0];
    let mut s = Svg::new(
        LEFT * 2.0 + ROW_W,
        rows[2] + ROW_H + 74.0,
        &format!(
            "Resuming: the resume keeps {} of the {} on disk and cuts off the torn block and the puts that go with it; the producer gives the lost calls again, and the piece ends with the same {} as one written without the crash",
            kib(c.kept),
            kib(c.cut),
            kib(len)
        ),
    );
    s.text(
        LEFT + ROW_W,
        14.0,
        "end",
        "soft small",
        &format!(
            "the first {} are the same in all three rows, and so is the rest of the first and the last",
            kib(c.newest)
        ),
    );
    let stub = |s: &mut Svg, x: f64, y: f64| {
        s.rect((x, y), (34.0, ROW_H), 1.0, "faint");
        s.text(x + 17.0, y + 19.5, "middle", "soft small", "...");
    };
    let right = LEFT + ROW_W - 34.0;
    s.text(
        LEFT,
        rows[0] - 10.0,
        "start",
        "strong",
        "written without a crash",
    );
    stub(&mut s, LEFT, rows[0]);
    stub(&mut s, right, rows[0]);
    row(&mut s, &c.blocks, len, v, rows[0], None);

    s.text(LEFT, rows[1] - 26.0, "start", "strong", "after the crash");
    stub(&mut s, LEFT, rows[1]);
    row(&mut s, &c.blocks, c.cut, v, rows[1], Some((c.torn, c.cut)));
    let (xk, xc) = (v.x(c.kept), v.x(c.cut));
    s.bracket_over(LEFT, xk - 1.0, rows[1] - 6.0, "thin");
    s.text(
        (LEFT + xk) / 2.0,
        rows[1] - 12.0,
        "middle",
        "small",
        &format!("kept: {}", kib(c.kept)),
    );
    s.bracket_under(xk + 1.0, xc, rows[1] + ROW_H + 6.0, "thin");
    s.text(
        xk,
        rows[1] + ROW_H + 24.0,
        "start",
        "accent small",
        "cut off: the puts that go with the torn block, and the torn block",
    );

    s.text(
        LEFT,
        rows[2] - 26.0,
        "start",
        "strong",
        "resumed, the lost calls given again",
    );
    stub(&mut s, LEFT, rows[2]);
    stub(&mut s, right, rows[2]);
    row(&mut s, &c.blocks, len, v, rows[2], None);
    s.bracket_over(xk + 1.0, LEFT + ROW_W, rows[2] - 6.0, "thin");
    s.text(
        (xk + LEFT + ROW_W) / 2.0,
        rows[2] - 12.0,
        "middle",
        "small",
        "written again: the same bytes as without the crash",
    );
    legend(
        &mut s,
        rows[2] + ROW_H + 40.0,
        &[
            ("tint-purple", "puts"),
            ("tint-blue", "log lines, frames"),
            ("tint-amber", "checkpoint"),
            ("tint-green", "final index, close, tail"),
            ("tint-accent", "torn"),
        ],
    );
    Ok(s.finish())
}
