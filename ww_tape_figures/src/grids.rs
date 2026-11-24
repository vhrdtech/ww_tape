//! `grids.svg`: series chunks on their grids, as [SeriesTimeline] reads them from their heads. Samples of 48 kHz on a
//! 1 ns clock: chunks of one grid, a lost chunk whose ticks are still known, a prefix change that keeps the grid,
//! and a gap to a new grid. Each chunk is written as a block by `chunk::write_chunk` and its head read back.

use ww_tape::ItemNum;

use anyhow::{Context, ensure};
use ww_tape::TrackId;
use ww_tape::TrackNum;
use ww_tape::chunk::{ChunkHead, chunk_len, write_chunk};
use ww_tape::series::is_gap;
use ww_tape::{
    BlockId, BlockKind, Codec, ColumnKind, ColumnSpec, EndMode, Magnitude, Ratio64, SeriesGrid,
    SeriesTimeline, SessionTag,
};

use crate::OrDebug;
use crate::svg::Svg;

/// Samples per chunk.
const COUNT: u32 = 6;
/// Tick of sample 0.
const START: i64 = 1_000;
/// Ticks from the last sample before the gap to the first after it.
const GAP: i64 = 90_000;

fn period() -> anyhow::Result<Ratio64> {
    Ratio64::new(62_500, 3).context("48 kHz on a 1 ns clock")
}

/// The head of a series chunk of `COUNT` samples from `first` on `grid`, as read back from the block written.
fn head(grid: SeriesGrid, first: u64) -> anyhow::Result<ChunkHead> {
    let (first_tick, last_tick) = grid
        .chunk_ticks(period()?, ItemNum::new(first), COUNT)
        .or_debug()?;
    let head = ChunkHead {
        kind: BlockKind::Series,
        track_id: TrackId::new(7),
        track_number: TrackNum::new(1),
        first_item: ItemNum::new(first),
        count: COUNT,
        first_tick,
        last_tick,
        latest_end: last_tick,
        time_mode: 0,
        end_mode: EndMode::Absent,
        grid: Some(grid),
        tile: ww_tape::TileCoords::NONE,
        level: 0,
    };
    let samples: Vec<u8> = (0..COUNT).flat_map(|i| (i as i16).to_le_bytes()).collect();
    let spec = ColumnSpec {
        kind: ColumnKind::Samples,
        codec: Codec::RAW,
        param: 0,
        len: samples.len(),
    };
    let mut block = vec![0u8; chunk_len(&head, &[spec], 0)];
    let id = BlockId::new(SessionTag::from_u32(1), first);
    let n = write_chunk(&mut block, id, &head, &[(spec, &samples)], 0).or_debug()?;
    let read = ChunkHead::read_head(block.get(..n).context("the block")?).or_debug()?;
    ensure!(read == head, "the head reads back as written");
    Ok(read)
}

fn scale_name(m: &Magnitude) -> &'static str {
    if *m == Magnitude::pow10(-3) {
        "10⁻³ A"
    } else {
        "10⁻⁶ A"
    }
}

pub fn grids() -> anyhow::Result<String> {
    let p = period()?;
    let micro = Magnitude::pow10(-6);
    let first = SeriesGrid::start(0, START, micro);
    // chunks 0, 1 and 3 of the first grid (chunk 2 lost), chunk 3 after a prefix change, chunk 4 on a new grid
    let c0 = head(first, 0)?;
    let c1 = head(first, 6)?;
    let lost = head(first, 12)?;
    let c3 = head(first.rescaled(Magnitude::pow10(-3)), 18)?;
    let second = SeriesGrid::start(24, c3.last_tick + GAP, micro);
    let c4 = head(second, 24)?;
    ensure!(
        !is_gap(p, c1.last_tick, lost.first_tick),
        "the lost chunk continues the grid"
    );
    ensure!(
        is_gap(p, c3.last_tick, c4.first_tick),
        "the new grid is a gap"
    );

    let (timeline, errors) = SeriesTimeline::from_chunks(p, [&c4, &c0, &c3, &c1]);
    ensure!(errors.is_empty(), "every head is in line: {errors:?}");
    let stretches = timeline.stretches();
    ensure!(
        stretches.len() == 3,
        "two scales of the first grid, then the second"
    );
    ensure!(!timeline.gap_before(1) && timeline.gap_before(2));
    for i in 12..18 {
        ensure!(
            timeline.stretch_of(ItemNum::new(i)).is_none(),
            "sample {i} is lost"
        );
        ensure!(
            timeline.tick(ItemNum::new(i)).is_some(),
            "its tick is known"
        );
    }

    let (left, plot_w) = (24.0, 640.0);
    let span = (c4.last_tick - START) as f64;
    let x = |tick: i64| left + (tick - START) as f64 / span * plot_w;
    let (lane, axis) = (96.0, 150.0);
    let mut s = Svg::new(
        left + plot_w + 24.0,
        axis + 44.0,
        "Five series chunks of six samples: three chunks on one grid with a lost chunk between them whose ticks are known, the last of them at another scale, then a gap and a chunk on a new grid",
    );
    let chunks = [
        (&c0, false, "chunk"),
        (&c1, false, "chunk"),
        (&lost, true, "lost"),
        (&c3, false, "chunk"),
        (&c4, false, "chunk"),
    ];
    for (h, is_lost, name) in chunks {
        let (x0, x1) = (x(h.first_tick) - 6.0, x(h.last_tick) + 6.0);
        let class = if is_lost { "thin dash" } else { "tint-blue" };
        s.rect((x0, lane - 14.0), (x1 - x0, 28.0), 3.0, class);
        for i in h.first_item.items(u64::from(h.count)) {
            let t = timeline.tick(i).context("every sample has a tick")?;
            s.circle((x(t), lane), 3.2, if is_lost { "hollow" } else { "blue" });
        }
        let label = if is_lost {
            name.to_owned()
        } else {
            format!(
                "{name}, {}",
                scale_name(&h.grid.context("a series head")?.scale)
            )
        };
        s.text(
            f64::midpoint(x0, x1),
            lane - 22.0,
            "middle",
            "small",
            &label,
        );
        s.text(
            x0 + 2.0,
            lane + 32.0,
            "start",
            "soft mono small",
            &h.first_tick.to_string(),
        );
    }
    // the grids' origins, and what each chunk's head carries
    for (g, n) in [(first, 1), (second, 2)] {
        let gx = x(g.grid_tick);
        s.line((gx, 30.0), (gx, lane - 14.0), "accent-line");
        // the second grid's label runs left of its line, inside the drawing
        let (lx, anchor) = if n == 1 {
            (gx + 5.0, "start")
        } else {
            (gx - 5.0, "end")
        };
        s.text(
            lx,
            34.0,
            anchor,
            "accent small",
            &format!("grid {n}: sample {} at tick {}", g.grid_index, g.grid_tick),
        );
    }
    // the gap
    let (g0, g1) = (x(c3.last_tick) + 8.0, x(c4.first_tick) - 8.0);
    s.bracket_over(g0, g1, lane - 2.0, "thin");
    s.text(
        f64::midpoint(g0, g1),
        lane - 12.0,
        "middle",
        "soft small",
        "gap",
    );
    // the axis
    s.line((left, axis), (left + plot_w, axis), "thin");
    s.text(
        left,
        axis + 18.0,
        "start",
        "soft small",
        "first tick of each chunk below it, in ns: sample i at grid tick + ⌊(i − grid sample) · 62500/3⌋",
    );
    s.text(
        left,
        axis + 36.0,
        "start",
        "soft small",
        "hollow: samples of a lost chunk, ticks known from the grid around it, values not",
    );
    Ok(s.finish())
}
