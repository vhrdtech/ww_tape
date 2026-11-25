//! `crash.svg` and `resume.svg`: the drive's story crash ([example_drive]) at 1/1000 of its data, its checkpoints
//! every 3 KiB instead of every 4 MiB so that several fit the drawing. The piece the crash tore, read back by
//! [TapeReader] and resumed; the resumed chain is checked to hold the bytes of the one written without the crash.

use anyhow::{Context, ensure};
use example_drive::Drive;
use ww_tape::index::IndexBlock;
use ww_tape::reader::Found;
use ww_tape::{BlockKind, FLAG_REPEAT, FLAG_WITH_NEXT, FileBlocks, Recovery, TapeReader};

use crate::OrDebug;
use crate::svg::Svg;

/// The drive's run as the figures draw it: 1/1000 of the data, a checkpoint every 3 KiB.
pub(crate) fn drive() -> anyhow::Result<Drive> {
    let scaled = Drive::scaled(1000).context("1000 divides the drive's rates")?;
    Ok(Drive {
        checkpoint_bytes: exact_units::Bytes::new(3 << 10),
        ..scaled
    })
}

/// What a block of the drawing is.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum Part {
    Head,
    Puts,
    Log,
    Frames,
    Samples,
    Summary,
    Checkpoint,
    FinalIndex,
    Close,
    Tail,
    Torn,
}

impl Part {
    pub(crate) fn class(self) -> &'static str {
        match self {
            Self::Puts => "tint-purple",
            Self::Log | Self::Frames | Self::Samples => "tint-blue",
            Self::Summary => "tint-teal",
            Self::Checkpoint => "tint-amber",
            Self::Head | Self::FinalIndex | Self::Close | Self::Tail => "tint-green",
            Self::Torn => "tint-accent",
        }
    }
}

/// A block: offset, length, what it is, its flags, and for a checkpoint the offset of the one before.
pub(crate) struct Span {
    pub(crate) at: u64,
    pub(crate) len: u64,
    pub(crate) part: Part,
    pub(crate) flags: u16,
    pub(crate) prev: Option<u64>,
}

/// The good blocks of `bytes` in file order.
pub(crate) fn spans(bytes: &[u8]) -> anyhow::Result<Vec<Span>> {
    let mut walk = FileBlocks::with_recovery(bytes, Recovery::Prefix);
    let mut out = Vec::new();
    while let Some(Ok(b)) = walk.next_block() {
        let (len, flags) = (u64::try_from(b.len())?, b.flags);
        let (part, prev) = match b.kind {
            BlockKind::Records if flags & FLAG_REPEAT != 0 => (Part::Puts, None),
            BlockKind::KeyedPuts => (Part::Puts, None),
            BlockKind::Records => (Part::Log, None),
            BlockKind::Events => (Part::Frames, None),
            BlockKind::Series => (Part::Samples, None),
            BlockKind::Summary | BlockKind::TileSummary => (Part::Summary, None),
            BlockKind::PieceHead => (Part::Head, None),
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
    let drive = drive()?;
    let recording = drive.record().or_debug()?;
    let crashed = drive
        .crash(recording.story_crash_byte().or_debug()?)
        .or_debug()?;
    let seq = usize::try_from(crashed.crash.piece)?;
    let whole = recording.pieces.get(seq).context("the torn piece")?.clone();
    let disk = crashed
        .pieces()
        .or_debug()?
        .get(seq)
        .context("the torn piece on disk")?
        .clone();
    let blocks = spans(&whole)?;
    let cut = crashed.crash.offset;
    let torn = blocks
        .iter()
        .position(|b| b.at <= cut && cut < b.at + b.len)
        .context("the block the crash tore")?;
    let b = blocks.get(torn).context("the torn block")?;
    ensure!(b.part == Part::Frames, "the crash tears a chunk of frames");

    let r = TapeReader::open(&disk[..]).or_debug()?;
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

    let resume = crashed.resume().or_debug()?;
    let kept = resume.resumed.kept_bytes;
    ensure!(
        resume.recording.pieces == recording.pieces,
        "the resumed chain holds the bytes of the one written without the crash"
    );
    ensure!(
        blocks
            .iter()
            .filter(|s| s.at >= kept && s.at < b.at)
            .all(|s| s.part == Part::Puts && s.flags & FLAG_WITH_NEXT != 0),
        "the resume cuts off the torn block and only the puts that go with it"
    );
    Ok(Crash {
        whole,
        blocks,
        cut,
        torn,
        newest,
        scanned,
        kept,
    })
}

/// Drawing scale: a view of the piece fills a row of [ROW_W] units after [LEFT].
pub(crate) const LEFT: f64 = 24.0;
pub(crate) const ROW_W: f64 = 640.0;
pub(crate) const ROW_H: f64 = 30.0;

/// The bytes `from..to` of the piece, drawn across a row.
#[derive(Copy, Clone)]
pub(crate) struct View {
    from: u64,
    to: u64,
    left: f64,
    width: f64,
}

impl View {
    pub(crate) fn new(from: u64, to: u64) -> Self {
        Self {
            from,
            to,
            left: LEFT,
            width: ROW_W,
        }
    }

    pub(crate) fn x(&self, at: u64) -> f64 {
        let at = at.clamp(self.from, self.to);
        self.left + (at - self.from) as f64 / (self.to - self.from) as f64 * self.width
    }
}

impl Part {
    fn name(self) -> &'static str {
        match self {
            Self::Head => "head",
            Self::Puts => "puts",
            Self::Log => "log",
            Self::Frames => "frames",
            Self::Samples => "samples",
            Self::Summary => "summary",
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
pub(crate) fn row(
    s: &mut Svg,
    blocks: &[Span],
    end: u64,
    v: View,
    y: f64,
    torn: Option<(usize, u64)>,
) {
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

pub(crate) fn legend(s: &mut Svg, y: f64, items: &[(&str, &str)]) {
    let mut x = LEFT;
    for (class, name) in items {
        s.rect((x, y), (16.0, 12.0), 2.0, class);
        s.text(x + 22.0, y + 10.5, "start", "small", name);
        x += 30.0 + 6.6 * name.len() as f64;
    }
}

pub(crate) fn kib(n: u64) -> String {
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
            ("tint-blue", "data"),
            ("tint-teal", "summaries"),
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
        f64::midpoint(LEFT, xk),
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
            ("tint-blue", "data"),
            ("tint-teal", "summaries"),
            ("tint-amber", "checkpoint"),
            ("tint-green", "head, index, close, tail"),
            ("tint-accent", "torn"),
        ],
    );
    Ok(s.finish())
}
