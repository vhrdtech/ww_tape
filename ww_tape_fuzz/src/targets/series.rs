//! ww_tape series tracks on any bytes: a describe record (a track with its kind) decodes without panic; series chunk
//! heads built from the input on grids (starts, changes of scale, gaps), then damaged by input bytes, pushed in an
//! order the input picks, give a timeline whose kept stretches hold its rules, and every tick, scale and grid query
//! on it answers without panic.
use shrink_wrap::prelude::*;
use ww_tape::chunk::ChunkHead;
use ww_tape::{
    BlockKind, Describe, EndMode, Magnitude, Ratio64, SeriesGrid, SeriesTimeline, TrackKind,
};

/// A series chunk head of `count` samples from `first` on `grid`, its ticks the grid's (or 0 where they overflow).
pub(crate) fn head_on(period: Ratio64, grid: SeriesGrid, first: u64, count: u32) -> ChunkHead {
    let (first_tick, last_tick) = grid.chunk_ticks(period, first, count).unwrap_or((0, 0));
    ChunkHead {
        kind: BlockKind::Series,
        track_id: 1,
        track_number: 1,
        first_item: first,
        count,
        first_tick,
        last_tick,
        latest_end: last_tick,
        time_mode: 0,
        end_mode: EndMode::Absent,
        grid: Some(grid),
    }
}

/// Ask the timeline everything about samples around `at`.
pub(crate) fn query(t: &SeriesTimeline, at: u64, count: u32) {
    for i in [0, at, at.wrapping_sub(1), at.saturating_add(1), u64::MAX] {
        let _ = (t.tick(i), t.scale(i), t.grid_of(i), t.stretch_of(i));
        if let Some(g) = t.grid_of(i)
            && let Ok((first, last)) = g.chunk_ticks(t.period(), i, count)
        {
            assert!(first <= last, "ticks of a chunk ascend");
            let head = head_on(t.period(), *g, i, count);
            assert_eq!(SeriesGrid::of_chunk(&head, t.period()), Ok(*g));
        }
    }
    for i in 0..t.stretches().len() + 1 {
        let _ = t.gap_before(i);
    }
}

/// The kept stretches hold the timeline's rules: ascending, apart, each on a grid at or before its first sample, a
/// new grid after the last tick of the stretch before.
pub(crate) fn check(t: &SeriesTimeline) {
    let s = t.stretches();
    for w in s.windows(2) {
        assert!(w[0].end_item <= w[1].first_item, "ascending, apart");
        if !w[0].grid.same_grid(&w[1].grid) {
            let last = w[0].grid.tick(t.period(), w[0].end_item - 1);
            let first = w[1].grid.tick(t.period(), w[1].first_item);
            assert!(last < first, "a new grid after the one before");
        }
        assert!(
            w[0].grid != w[1].grid || w[0].end_item < w[1].first_item,
            "touching stretches of one grid and scale are one"
        );
    }
    for x in s {
        assert!(x.first_item < x.end_item, "a stretch holds samples");
        assert!(x.grid.grid_index <= x.first_item, "grid at or before");
        let tick = x
            .grid
            .tick(t.period(), x.first_item)
            .expect("a kept stretch's first tick");
        assert_eq!(t.tick(x.first_item), Some(tick));
        assert_eq!(t.scale(x.end_item - 1), Some(x.grid.scale));
    }
}

fn u64_at(data: &[u8], at: usize) -> u64 {
    let mut b = [0u8; 8];
    for (o, x) in b.iter_mut().zip(data.iter().skip(at)) {
        *o = *x;
    }
    u64::from_le_bytes(b)
}

pub fn series(data: &[u8]) {
    // any bytes as a describe record
    if let Ok(Describe::Track(t)) = Describe::from_ww_bytes(data) {
        let _ = (t.check_kind(), t.id());
        if let TrackKind::Series(s) = &t.kind {
            let _ = (s.check(), s.first_grid(0));
        }
    }

    let period = Ratio64::new(u64_at(data, 0) % 100_000 + 1, u64_at(data, 8) % 1000 + 1)
        .expect("parts above 0");
    let input = data.get(16..).unwrap_or(&[]);

    // chunks from the input: a grid at the start, the same grid at another scale, a new grid after a gap
    let mut heads = vec![];
    let mut first = 0u64;
    let mut grid = SeriesGrid::start(0, 0, Magnitude::ONE);
    for (k, c) in input.chunks(4).take(64).enumerate() {
        let count = 1 + u32::from(c.first().copied().unwrap_or(0));
        let scale = Magnitude::pow10(-((c.get(3).copied().unwrap_or(0) % 12) as i8));
        let lost = u64::from(c.get(2).copied().unwrap_or(0) % 4 == 3);
        grid = match k % 3 {
            _ if k == 0 => SeriesGrid::start(first, 0, scale),
            1 => grid.rescaled(scale),
            2 => {
                let last = grid.tick(period, first.saturating_sub(1)).unwrap_or(0);
                let gap = i64::from(c.get(1).copied().unwrap_or(0))
                    << (c.get(2).copied().unwrap_or(0) % 40);
                SeriesGrid::start(first, last.saturating_add(1).saturating_add(gap), scale)
            }
            _ => grid,
        };
        heads.push(head_on(period, grid, first, count));
        // a chunk lost now and then: samples missing on the same grid
        first = first
            .saturating_add(u64::from(count))
            .saturating_add(lost * 7);
    }
    let (t, _) = SeriesTimeline::from_chunks(period, &heads);
    check(&t);
    query(&t, first / 2, input.len() as u32 % 300);

    // the same heads damaged by input bytes and pushed in an order the input picks
    for (k, h) in heads.iter_mut().enumerate() {
        let Some(x) = input.get(k) else { break };
        match x % 8 {
            0 => h.first_tick = h.first_tick.wrapping_add(1),
            1 => h.first_item = h.first_item.wrapping_add(u64::from(*x)),
            2 => {
                h.grid = h.grid.map(|g| SeriesGrid {
                    grid_tick: g.grid_tick ^ i64::from(*x),
                    ..g
                })
            }
            3 => {
                h.grid = h.grid.map(|g| SeriesGrid {
                    grid_index: g.grid_index.wrapping_add(u64::from(*x)),
                    ..g
                })
            }
            4 => h.grid = None,
            _ => {}
        }
    }
    let n = heads.len();
    if n > 1 {
        let rot = input.first().map_or(0, |x| usize::from(*x) % n);
        heads.rotate_left(rot);
    }
    let (t, _) = SeriesTimeline::from_chunks(period, &heads);
    check(&t);
    query(&t, first / 3, 1);
}
