//! `summary_tree.svg`: the summary nodes of `phase_a` in the drive's hour ([ww_tape_example]) at 1/1000 of its data,
//! read from its summary blocks: level 0 over the item numbers, level 1 above it with each node over its 16 children,
//! each node a bar from its minimum to its maximum; below, a viewer's pixel columns, each merging the level-0 nodes
//! that start in it ([ChannelStats::merge]).

use anyhow::{Context, ensure};
use ww_tape::{BlockKind, Element, ElementDesc, FileBlocks, Recovery};
use ww_tape_example::{Drive, key};
use ww_tape_summary::{ChannelStats, Summary};

use crate::OrDebug;
use crate::svg::Svg;

/// The scale the drive is drawn at.
const SCALE: u32 = 1000;
/// The viewer's pixel columns in the drawing.
const COLUMNS: u64 = 24;

/// A node as read: its level, first item, items and statistics.
struct Node {
    level: u8,
    first: u64,
    items: u64,
    stats: ChannelStats,
}

fn bounds(s: &ChannelStats) -> (f64, f64) {
    (s.min[0].as_f64(), s.max[0].as_f64())
}

pub fn summary_tree() -> anyhow::Result<String> {
    let drive = Drive::scaled(SCALE).context("1000 divides the drive's rates")?;
    let recording = drive.record()?;
    let mut nodes = Vec::new();
    for piece in &recording.pieces {
        let mut walk = FileBlocks::with_recovery(&piece[..], Recovery::Prefix);
        while let Some(b) = walk.next_block() {
            let b = b.or_debug()?;
            if b.kind != BlockKind::Summary {
                continue;
            }
            let summary = Summary::of_block(&b).context("a summary block")?;
            let head = summary.head();
            ensure!(head.track_id == key("phase_a"), "phase_a's summaries");
            let span = 1u64 << head.node_shift;
            let end = head.first_item + head.items;
            for n in 0..summary.node_count() {
                let first = head.first_item + n as u64 * span;
                nodes.push(Node {
                    level: head.level,
                    first,
                    items: span.min(end - first),
                    stats: summary.channel_stats(0, n).context("a node")?,
                });
            }
        }
    }
    let total = nodes
        .iter()
        .map(|n| n.first + n.items)
        .max()
        .context("nodes")?;
    let level = |l: u8| nodes.iter().filter(move |n| n.level == l);
    ensure!(level(1).count() > 1, "level 1 has nodes");
    let top = level(1)
        .map(|n| bounds(&n.stats).1)
        .fold(f64::MIN, f64::max);
    let bottom = level(1)
        .map(|n| bounds(&n.stats).0)
        .fold(f64::MAX, f64::min);

    let (left, width) = (90.0, 560.0);
    let x = |i: u64| left + i as f64 / total as f64 * width;
    let mut s = Svg::new(
        680.0,
        360.0,
        "The summary nodes of phase_a over its item numbers: level 0 nodes of 1024 samples each drawn as a bar from minimum to maximum, level 1 nodes of 16 of them above, and below a viewer's pixel columns, each merging the level-0 nodes that start in it",
    );
    s.text(
        20.0,
        22.0,
        "start",
        "strong",
        &format!("phase_a's summary nodes over its {total} samples, minimum to maximum"),
    );
    // a lane per level, and the columns below; each bar spans minimum to maximum
    let lane = |y0: f64, h: f64, (lo, hi): (f64, f64)| {
        let y = |v: f64| y0 + h - (v - bottom) / (top - bottom) * h;
        (y(hi), (y(lo) - y(hi)).max(1.0))
    };
    let lanes = [(1u8, 44.0, "level 1"), (0u8, 132.0, "level 0")];
    for (l, y0, name) in lanes {
        s.text(20.0, y0 + 40.0, "start", "small", name);
        s.line((left, y0 + 70.0), (left + width, y0 + 70.0), "faint");
        for n in level(l) {
            let (x0, x1) = (x(n.first) + 0.5, x(n.first + n.items) - 0.5);
            let (y, h) = lane(y0, 70.0, bounds(&n.stats));
            s.rect((x0, y), ((x1 - x0).max(1.0), h), 1.0, "tint-blue");
        }
    }
    // one level-1 node and the 16 children it merges
    let parent = level(1).nth(1).context("a second level-1 node")?;
    let (p0, p1) = (x(parent.first), x(parent.first + parent.items));
    s.rect((p0 - 1.0, 40.0), (p1 - p0 + 2.0, 168.0), 3.0, "accent-line");
    s.text(
        p1 + 4.0,
        52.0,
        "start",
        "accent small",
        "one node of level 1 merges 16 of level 0",
    );
    // the viewer's columns: level-0 nodes merged by where they start
    let mut columns = vec![None::<(ChannelStats, u64)>; COLUMNS as usize];
    for n in level(0) {
        let c = (n.first * COLUMNS / total) as usize;
        let col = columns.get_mut(c).context("a column")?;
        let (stats, items) =
            col.get_or_insert((ChannelStats::empty(ElementDesc::of(Element::I16)), 0));
        stats.merge(&n.stats, *items, n.items);
        *items += n.items;
    }
    let y0 = 232.0;
    s.text(20.0, y0 + 40.0, "start", "small", "columns");
    let cw = width / COLUMNS as f64;
    for (c, col) in columns.iter().enumerate() {
        let x0 = left + c as f64 * cw;
        s.rect((x0 + 0.5, y0), (cw - 1.0, 70.0), 0.0, "faint");
        if let Some((stats, _)) = col {
            let (y, h) = lane(y0, 70.0, bounds(stats));
            s.rect((x0 + 3.0, y), (cw - 6.0, h), 1.0, "tint-teal");
        }
    }
    s.text(
        20.0,
        330.0,
        "start",
        "soft small",
        &format!(
            "{} level-0 nodes of 1024 samples, {} of level 1; {COLUMNS} pixel columns, each the merge of the level-0 nodes that start in it",
            level(0).count(),
            level(1).count()
        ),
    );
    s.text(
        20.0,
        348.0,
        "start",
        "soft small",
        "the bars run from each node's minimum to its maximum current, on one scale for all three rows",
    );
    Ok(s.finish())
}
