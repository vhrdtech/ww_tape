//! A series track of i16 samples at 48 kHz on a 1 ns clock, its definitions and the heads of its chunks.

use shrink_wrap::prelude::*;
use ww_tape::chunk::ChunkHead;
use ww_tape::{
    BlockKind, ClockOrigin, ClockSpec, Definitions, EndMode, GridDef, ItemNum, Magnitude, Ratio64,
    Segment, SeriesGrid, TrackKind, TrackNum, TrackSpec, clock_key, track_key,
};

/// The track's period: 48 kHz on a 1 ns clock.
pub fn period() -> Ratio64 {
    Ratio64::new(62_500, 3).expect("48 kHz on a 1 ns clock")
}

/// The definitions of the series track and its clock, and the track's number.
pub fn definitions() -> anyhow::Result<(Definitions, TrackNum)> {
    let d = |e: ww_tape::DescribeError| anyhow::anyhow!("{e:?}");
    let mut defs = Definitions::new();
    let clock = defs
        .define_clock(ClockSpec {
            key: clock_key(0xc4, &[], 1),
            rate: Magnitude::pow10(9).into(),
            resolution: Magnitude::pow10(-9).into(),
            origin: ClockOrigin::Boot,
            label: "bench",
        })
        .map_err(d)?;
    let track = defs
        .define_track(TrackSpec {
            key: track_key(0xc4, &[Segment::Name("samples")]),
            clock,
            dimensions: RefVec::Slice { slice: &[] },
            items: ww_tape::Items::numbers::<i16>(None),
            label: "bench",
            repeat: ww_tape::TrackRepeat::Stream,
            kind: TrackKind::Series(GridDef::of::<i16>(1, period())),
            relations: RefVec::Slice { slice: &[] },
            owner: [0; 32],
        })
        .map_err(d)?;
    Ok((defs, track))
}

/// The head of chunk `k` of `count` i16 samples on track `track`.
pub fn head(track: TrackNum, k: u64, count: u32) -> anyhow::Result<ChunkHead> {
    let grid = SeriesGrid::start(0, 0, Magnitude::ONE);
    let first = k * u64::from(count);
    let (first_tick, last_tick) = grid
        .chunk_ticks(period(), ItemNum::new(first), count)
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    Ok(ChunkHead {
        kind: BlockKind::Series,
        // the track's id: its key, it has no dimensions
        track_id: track_key(0xc4, &[Segment::Name("samples")]).id(),
        track_number: track,
        first_item: ItemNum::new(first),
        count,
        first_tick,
        last_tick,
        latest_end: last_tick,
        time_mode: 0,
        end_mode: EndMode::Absent,
        grid: Some(grid),
    })
}
