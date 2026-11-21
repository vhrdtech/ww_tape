//! `autorange.svg`: an autoranging current meter at 1 kHz that switches from its milliampere range to its microampere
//! range at sample 10 000. Its chunks are written as blocks and read back: one grid throughout, the scale 10⁻³ A
//! up to sample 9 999 and 10⁻⁶ A from 10 000. Its summary nodes of 4096 samples come from the blocks
//! [SeriesSummaries] writes: the node across the change is kept at the common scale of both.

use anyhow::{Context, ensure};
use ww_tape::TrackNum;
use ww_tape::chunk::{ChunkHead, chunk_len, write_chunk};
use ww_tape::samples::{SampleShape, Samples};
use ww_tape::{
    Block, BlockId, BlockKind, Codec, ColumnKind, ColumnSpec, Element, ElementDesc, EndMode,
    Magnitude, Ratio64, SeriesGrid, SessionTag,
};
use ww_tape_summary::{SeriesStart, SeriesStats, SeriesSummaries, SeriesSummarySpec, Summary};

use crate::OrDebug;
use crate::svg::Svg;

/// Samples in the drawing, the sample the range changes at, and samples per chunk.
const SAMPLES: u64 = 20_000;
const SWITCH: u64 = 10_000;
const CHUNK: u64 = 1_000;
/// The summaries' level-0 node: 2^12 samples.
const NODE_SHIFT: u8 = 12;

/// The meter's current at sample `i`, in A: a capacitor's charging current dying away.
fn amperes(i: u64) -> f64 {
    1.5 * (-(i as f64) / 1_500.0).exp()
}

/// The stored number of sample `i`: milliamperes before the switch, microamperes after it.
fn stored(i: u64) -> i16 {
    let unit = if i < SWITCH { 1e3 } else { 1e6 };
    (amperes(i) * unit).round() as i16
}

/// A series chunk of samples `first..first + CHUNK` on `grid`, written as a block and read back: its head and samples.
fn chunk(grid: SeriesGrid, period: Ratio64, first: u64) -> anyhow::Result<(ChunkHead, Vec<i16>)> {
    let count = CHUNK as u32;
    let (first_tick, last_tick) = grid.chunk_ticks(period, first, count).or_debug()?;
    let head = ChunkHead {
        kind: BlockKind::Series,
        track_id: 11,
        track_number: TrackNum::new(1),
        first_item: first,
        count,
        first_tick,
        last_tick,
        latest_end: last_tick,
        time_mode: 0,
        end_mode: EndMode::Absent,
        grid: Some(grid),
    };
    let samples: Vec<u8> = (first..first + CHUNK)
        .flat_map(|i| stored(i).to_le_bytes())
        .collect();
    let spec = ColumnSpec {
        kind: ColumnKind::Samples,
        codec: Codec::RAW,
        param: 0,
        len: samples.len(),
    };
    let mut buf = vec![0u8; chunk_len(&head, &[spec], 0)];
    let id = BlockId::new(SessionTag::from_u32(1), first / CHUNK);
    let n = write_chunk(&mut buf, id, &head, &[(spec, &samples)], 0).or_debug()?;
    let block = Block::read(buf.get(..n).context("the block")?).or_debug()?;
    let c = block.chunk().context("a chunk")?;
    let col = c.find(ColumnKind::Samples, 0).context("samples")?;
    let read = Samples::of(
        &col,
        SampleShape::scalar(ElementDesc::of(Element::I16)),
        CHUNK as usize,
    )
    .or_debug()?;
    let mut values = vec![0i16; CHUNK as usize];
    read.decode(0, &mut values).or_debug()?;
    Ok((c.head(), values))
}

/// What a scale is called in the drawing.
fn name(m: Magnitude) -> &'static str {
    if m == Magnitude::pow10(-3) {
        "10⁻³ A"
    } else if m == Magnitude::pow10(-6) {
        "10⁻⁶ A"
    } else {
        "another scale"
    }
}

pub fn autorange() -> anyhow::Result<String> {
    let period = Ratio64::new(1_000, 1).context("1 kHz on a µs clock")?;
    let (milli, micro) = (Magnitude::pow10(-3), Magnitude::pow10(-6));
    let grid = SeriesGrid::start(0, 0, milli);
    let mut chunks = Vec::new();
    for first in (0..SAMPLES).step_by(CHUNK as usize) {
        let g = if first < SWITCH {
            grid
        } else {
            grid.rescaled(micro)
        };
        chunks.push(chunk(g, period, first)?);
    }
    // the summaries, built as a writer builds them, the grid rescaled at the switch
    let start = SeriesStart {
        track_id: 11,
        track_number: TrackNum::new(1),
        shape: SampleShape::scalar(ElementDesc::of(Element::I16)),
        first_item: 0,
        grid,
        period,
    };
    let spec = SeriesSummarySpec {
        base_shift: NODE_SHIFT,
        stats: SeriesStats {
            min: true,
            max: true,
            ..SeriesStats::default()
        },
        clip: None,
    };
    let mut sums = SeriesSummaries::<i16>::new(start, spec).or_debug()?;
    // a block per node, so that each block states its own node's scale
    sums.set_max_block_bytes(0);
    for (head, values) in &chunks {
        sums.set_grid(head.grid.context("a series chunk")?)
            .or_debug()?;
        sums.push(values, 0).or_debug()?;
    }
    sums.finish().or_debug()?;
    let mut nodes = Vec::new();
    for b in sums.take_blocks() {
        let summary = Summary::read(&b).or_debug()?;
        if summary.head().level != 0 {
            continue;
        }
        let first = summary.head().first_item;
        for n in 0..summary.node_count() {
            let node = summary.series_node(n).context("a node")?;
            let stats = node.channels.first().context("a channel")?;
            let at = first + ((n as u64) << NODE_SHIFT);
            nodes.push((at, node.scale, stats.min[0].as_f64(), stats.max[0].as_f64()));
        }
    }
    let across = nodes
        .iter()
        .find(|n| n.0 < SWITCH && SWITCH < n.0 + (1 << NODE_SHIFT))
        .context("a node across the switch")?;
    ensure!(across.1 == micro, "the node across the switch is at 10⁻⁶ A");

    let (left, width) = (60.0, 600.0);
    let x = |i: u64| left + i as f64 / SAMPLES as f64 * width;
    // the current on a log scale, 1 µA to 10 A
    let (y_top, y_h) = (40.0, 140.0);
    let y = |a: f64| y_top + (1.0 - (a.log10() + 6.0) / 7.0) * y_h;
    let mut s = Svg::new(
        680.0,
        330.0,
        "An autoranging current meter's samples over 20 000 samples on a log scale, in chunks of 1000 on one grid: milliamperes at scale 10⁻³ up to sample 9 999, microamperes at scale 10⁻⁶ from sample 10 000. Below, summary nodes of 4096 samples with the scale each is kept at: the node across the switch at the common scale 10⁻⁶.",
    );
    s.text(
        20.0,
        22.0,
        "start",
        "strong",
        "samples, read back from their chunks, in amperes",
    );
    for (label, a) in [("1 A", 1.0), ("1 mA", 1e-3), ("1 µA", 1e-6)] {
        s.line((left, y(a)), (left + width, y(a)), "faint");
        s.text(left - 6.0, y(a) + 4.0, "end", "soft small", label);
    }
    let mut d = String::new();
    for (head, values) in &chunks {
        let g = head.grid.context("a grid")?;
        let unit = if g.scale == milli { 1e-3 } else { 1e-6 };
        for (k, v) in values.iter().enumerate().step_by(50) {
            let i = head.first_item + k as u64;
            let a = f64::from(*v).max(1.0) * unit;
            d.push_str(&format!(
                "{}{:.1} {:.1} ",
                if d.is_empty() { "M" } else { "L" },
                x(i),
                y(a)
            ));
        }
    }
    s.path(d.trim_end(), "line");
    for (head, _) in &chunks {
        let x0 = x(head.first_item);
        s.line((x0, y_top + y_h), (x0, y_top + y_h + 6.0), "thin");
    }
    s.line(
        (x(SWITCH), 32.0),
        (x(SWITCH), y_top + y_h + 26.0),
        "accent-line",
    );
    s.text(
        x(SWITCH) + 4.0,
        36.0,
        "start",
        "accent small",
        "sample 10 000: micro range",
    );
    let band = y_top + y_h + 16.0;
    s.text(
        x(SWITCH / 2),
        band + 4.0,
        "middle",
        "small",
        "chunks at 10⁻³ A: stored mA",
    );
    s.text(
        x(SWITCH + (SAMPLES - SWITCH) / 2),
        band + 4.0,
        "middle",
        "small",
        "chunks at 10⁻⁶ A: stored µA, the same grid",
    );
    // the summary nodes
    let ny = 240.0;
    s.text(
        20.0,
        ny - 10.0,
        "start",
        "strong",
        "summary nodes of 4096 samples, each at its scale",
    );
    for &(at, scale, lo, hi) in &nodes {
        let (x0, x1) = (x(at) + 1.0, x((at + (1 << NODE_SHIFT)).min(SAMPLES)) - 1.0);
        let class = if at == across.0 {
            "tint-accent"
        } else {
            "tint-blue"
        };
        s.rect((x0, ny), (x1 - x0, 26.0), 2.0, class);
        s.text((x0 + x1) / 2.0, ny + 17.0, "middle", "small", name(scale));
        s.text(
            (x0 + x1) / 2.0,
            ny + 42.0,
            "middle",
            "mono small",
            &format!("{lo:.0} to {hi:.0}"),
        );
    }
    s.text(
        20.0,
        322.0,
        "start",
        "soft small",
        "below each node: its minimum and maximum, stored numbers at the node's scale",
    );
    Ok(s.finish())
}
