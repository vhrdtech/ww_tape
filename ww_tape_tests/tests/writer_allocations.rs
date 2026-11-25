#![allow(
    clippy::unwrap_used,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    reason = "tests fail by panicking and compute expected values freely"
)]
//! The allocations a writer makes per chunk in steady state, its tracks defined and their definitions near: a few
//! per call at most, never one per candidate definition looked at.

use std::convert::Infallible;
use std::hint::black_box;

use shrink_wrap::prelude::RefVec;
use ww_tape::{
    BlockKind, ChunkHead, ClockOrigin, ClockSpec, Codec, ColumnKind, ColumnSpec, Definitions,
    EndMode, EventsDef, GridDef, ItemNum, Magnitude, Ratio64, Segment, SeriesGrid, StatePolicy,
    SyncPolicy, TapeSink, TapeWriter, TrackKind, TrackNum, TrackRepeat, TrackSpec, clock_key,
    track_key,
};
use ww_tape_testalloc::{Counting, thread_heap_of};

#[global_allocator]
static ALLOC: Counting = Counting::new();

/// Series tracks the chunks go to, in turn.
const SERIES: u32 = 4;
/// Samples per chunk.
const COUNT: u32 = 256;
/// Calls before the count: every definition written and near, the writer's buffers grown.
const WARM: u64 = 256;
/// Calls counted.
const CALLS: u64 = 1024;
/// Allocations per chunk written at most in steady state.
const CHUNK_ALLOCATIONS_MAX: u64 = 2;

/// A sink that keeps no bytes: only the writer's own allocations count.
struct Discard {
    bytes: u64,
}

impl TapeSink for Discard {
    type Error = Infallible;

    fn write_all(&mut self, bytes: &[u8]) -> Result<(), Infallible> {
        self.bytes += bytes.len() as u64;
        Ok(())
    }

    fn sync(&mut self) -> Result<(), Infallible> {
        Ok(())
    }

    fn close(&mut self) -> Result<(), Infallible> {
        Ok(())
    }

    fn size(&self) -> Result<u64, Infallible> {
        Ok(self.bytes)
    }

    fn read_at(&self, _: u64, buf: &mut [u8]) -> Result<(), Infallible> {
        buf.fill(0);
        Ok(())
    }

    fn truncate(&mut self, len: u64) -> Result<(), Infallible> {
        self.bytes = len;
        Ok(())
    }

    fn rebase(&mut self, _: u64) -> Result<(), Infallible> {
        Ok(())
    }
}

/// The key of series track `k`.
fn series_key(k: u32) -> ww_tape::TrackKey {
    track_key(0xa1, &[Segment::Index(k)])
}

/// A clock, [SERIES] series tracks of i16 on it and a state track; their numbers.
fn definitions() -> (Definitions, Vec<TrackNum>, TrackNum) {
    let mut defs = Definitions::new();
    let clock = defs
        .define_clock(ClockSpec {
            key: clock_key(0xa1, &[], 1),
            rate: Magnitude::pow10(9).into(),
            resolution: Magnitude::pow10(-9).into(),
            origin: ClockOrigin::Boot,
            label: "host",
        })
        .unwrap();
    let series = (0..SERIES)
        .map(|k| {
            defs.define_track(TrackSpec {
                key: series_key(k),
                clock,
                dimensions: RefVec::Slice { slice: &[] },
                items: ww_tape::Items::numbers::<i16>(None),
                label: "series",
                repeat: TrackRepeat::Stream,
                kind: TrackKind::Series(GridDef::of::<i16>(1, period())),
                relations: RefVec::Slice { slice: &[] },
                owner: [0; 32],
                defined_twice: false,
            })
            .unwrap()
        })
        .collect();
    let state = defs
        .define_track(TrackSpec {
            key: track_key(0xa1, &[Segment::Name("state")]),
            clock,
            dimensions: RefVec::Slice { slice: &[] },
            items: ww_tape::Items::numbers::<u64>(None),
            label: "state",
            repeat: TrackRepeat::State(StatePolicy::DEFAULT),
            kind: TrackKind::Events(EventsDef::INSTANTS),
            relations: RefVec::Slice { slice: &[] },
            owner: [0; 32],
            defined_twice: false,
        })
        .unwrap();
    (defs, series, state)
}

/// The sample period: 1 µs on a clock of ns.
fn period() -> Ratio64 {
    Ratio64::new(1000, 1).unwrap()
}

/// The head of chunk `k` of series track `k` mod [SERIES], `track` its number.
fn head(track: TrackNum, k: u64) -> ChunkHead {
    let grid = SeriesGrid::start(0, 0, Magnitude::ONE);
    let first = (k / u64::from(SERIES)) * u64::from(COUNT);
    let (first_tick, last_tick) = grid
        .chunk_ticks(period(), ItemNum::new(first), COUNT)
        .unwrap();
    ChunkHead {
        kind: BlockKind::Series,
        track_id: series_key((k % u64::from(SERIES)) as u32).id(),
        track_number: track,
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
    }
}

/// Series chunks on [SERIES] tracks in turn: the allocations of a chunk written in steady state.
#[test]
fn a_chunk_written_in_steady_state_allocates_little() {
    let (defs, series, _) = definitions();
    let samples: Vec<u8> = (0..COUNT as usize * 2).map(|i| (i * 31) as u8).collect();
    let spec = ColumnSpec {
        kind: ColumnKind::Samples,
        codec: Codec::RAW,
        param: 0,
        len: samples.len(),
    };
    let columns = [(spec, samples.as_slice())];
    let heads: Vec<ChunkHead> = (0..WARM + CALLS)
        .map(|k| head(series[(k % u64::from(SERIES)) as usize], k))
        .collect();
    let mut w = TapeWriter::create_on(Discard { bytes: 0 }, SyncPolicy::None);
    let (warm, counted) = heads.split_at(WARM as usize);
    for h in warm {
        w.write_chunk(&defs, h, &columns).unwrap();
    }
    let ((), heap) = thread_heap_of(|| {
        for h in counted {
            black_box(w.write_chunk(&defs, h, &columns).unwrap());
        }
    });
    let per = heap.allocations / CALLS;
    assert!(
        per <= CHUNK_ALLOCATIONS_MAX,
        "{} allocations for {CALLS} chunks: {per} a chunk",
        heap.allocations
    );
}
