//! Series tracks: the describe record of the track (shape, period, scale) in a block of definitions, and series
//! chunks of it whose heads (first sample, count, ticks, grid index, grid tick, scale) take hostile values.

use super::chunk::{build, hit_head, reseal};
use super::{Draw, SESSION, clock_item, definition, describe_block};
use shrink_wrap::prelude::*;
use ww_tape::{
    BlockId, BlockKind, ChunkHead, Codec, ColumnKind, ColumnSpec, Describe, EndMode, FullVersion,
    Magnitude, Ratio64, SampleShape, Segment, SeriesDef, SeriesGrid, TrackDef, TrackKind, Version,
    track_key,
};

/// The track number of the hostile series.
pub(crate) const TRACK: u32 = 1;

/// The describe record of series track [TRACK] on clock 1 with `shape`, `period` and `scale`; empty when it doesn't
/// encode.
pub(crate) fn track_item(shape: SampleShape, period: Ratio64, scale: Magnitude) -> Vec<u8> {
    let def = SeriesDef {
        sample: None,
        shape,
        period,
        scale,
        summaries: None,
        media: None,
    };
    let track = Describe::Track(TrackDef {
        number: UNib32(TRACK),
        key: track_key(0x5e, &[Segment::Name("hostile")]),
        clock: UNib32(1),
        dimensions: RefVec::Slice { slice: &[] },
        api: FullVersion::new("hostile_api", Version::new(0, 1, 0)),
        bundle: RefVec::Slice { slice: &[] },
        path: RefVec::Slice {
            slice: &[UNib32(1)],
        },
        keys: RefVec::Slice { slice: &[] },
        item: None,
        label: "hostile",
        repeat: ww_tape::TrackRepeat::Stream,
        kind: TrackKind::Series(def),
        relations: RefVec::Slice { slice: &[] },
        owner: [0; 32],
    });
    definition(&track)
}

/// A power of ten from the input, the extremes a magnitude holds (10^-63, 10^63) included.
fn magnitude(d: &mut Draw<'_>) -> Magnitude {
    let e = match d.u8() % 4 {
        0 => -63,
        1 => 63,
        _ => -((d.u8() % 19) as i8),
    };
    Magnitude::pow10(e)
}

/// A period whose parts take hostile values; 1 when they make no ratio.
fn period(d: &mut Draw<'_>) -> Ratio64 {
    let n = d.value(64, &[1 << 53, u64::from(u32::MAX)], &[1000]);
    let m = d.value(64, &[1 << 53, u64::from(u32::MAX)], &[3]);
    Ratio64::new(n, m)
        .or_else(|| Ratio64::new(1 + n % 100_000, 1 + m % 1000))
        .expect("parts above 0")
}

/// A block of definitions of a series track and series chunks of it, whose first samples, counts, grid indexes,
/// grid ticks, scales, head ticks and period take hostile values; each chunk's head CRC holds, so a reader takes
/// the head.
pub fn series(data: &[u8]) -> Vec<u8> {
    let mut d = Draw::new(data);
    let period = period(&mut d);
    let scale = magnitude(&mut d);
    let channels = UNib32(1 + d.below(3) as u32);
    let shape = SampleShape {
        element: ww_tape::Element::I16,
        channels,
    };
    let item = track_item(shape, period, scale);
    let mut out = describe_block(BlockId::new(SESSION, 0), &[&clock_item(), &item]);
    // chunks on valid grids first, then hostile fields
    let mut heads: Vec<ChunkHead> = vec![];
    let mut first = u64::from(d.u16());
    let mut grid = SeriesGrid::start(first, i64::from(d.u32() as i32), magnitude(&mut d));
    for k in 0..1 + d.below(5) {
        if k % 3 == 2 {
            grid = grid.rescaled(magnitude(&mut d));
        } else if k > 0 {
            let tick = grid
                .tick(period, first)
                .unwrap_or(0)
                .saturating_add(i64::from(d.u32()));
            grid = SeriesGrid::start(first, tick, magnitude(&mut d));
        }
        let count = 1 + u32::from(d.u8());
        let (first_tick, last_tick) = grid.chunk_ticks(period, first, count).unwrap_or((0, 0));
        heads.push(ChunkHead {
            kind: BlockKind::Series,
            track_id: 7,
            track_number: TRACK,
            first_item: first,
            count,
            first_tick,
            last_tick,
            latest_end: last_tick,
            time_mode: 0,
            end_mode: EndMode::Absent,
            grid: Some(grid),
        });
        first = first.saturating_add(u64::from(count) + u64::from(d.u8() % 3));
    }
    let caps = &[u64::MAX / 2, i64::MAX as u64, 1 << 53];
    for _ in 0..1 + d.below(3) {
        let i = d.below(heads.len());
        let h = &mut heads[i];
        let Some(g) = h.grid.as_mut() else { continue };
        let own = [h.first_item, g.grid_index];
        match d.u8() % 6 {
            0 => h.first_item = d.value(64, caps, &own),
            1 => g.grid_index = d.value(64, caps, &own).min(h.first_item),
            2 => g.grid_tick = d.value(64, caps, &[g.grid_tick as u64]) as i64,
            3 => g.scale = magnitude(&mut d),
            4 => h.first_tick = d.value(64, caps, &[h.first_tick as u64]) as i64,
            _ => h.count = 1 + d.value(32, &[u64::from(u32::MAX - 1)], &[]) as u32 % 1_000_000,
        }
        h.last_tick = h.last_tick.max(h.first_tick);
        h.latest_end = h.last_tick;
        h.first_item = h.first_item.min(u64::MAX - u64::from(h.count));
        if g.grid_index > h.first_item {
            g.grid_index = h.first_item;
        }
    }
    let samples = vec![0u8; 2 * shape.channels.0 as usize];
    for h in &heads {
        let spec = ColumnSpec {
            kind: ColumnKind::Samples,
            codec: Codec::RAW,
            param: 0,
            len: samples.len(),
        };
        let mut b = build(h, &[(spec, &samples)]);
        // a scale length or grid field over the written one, the head CRC made to hold
        if d.u8() % 4 == 3 {
            hit_head(&mut d, &mut b);
            reseal(&mut b, false);
        }
        out.extend(b);
    }
    out
}
