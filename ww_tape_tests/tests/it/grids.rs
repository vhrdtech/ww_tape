//! A grid track written and read back by its tiles (FMT-15): each chunk's index entry carries its tile number, and
//! the writer refuses coordinates outside the grid.

use shrink_wrap::prelude::*;
use ww_tape::chunk::ChunkHead;
use ww_tape::index::IndexPolicy;
use ww_tape::{
    AxisCoords, BlockKind, ClockNum, Codec, ColumnKind, ColumnSpec, DescribeError, EndMode,
    FixedAxis, GridDef, ItemNum, Magnitude, MemSink, Ratio64, Segment, SeriesError, SyncPolicy,
    TapeReader, TapeWriter, TileCoords, TileLayout, TrackDef, TrackId, TrackKind, TrackNum,
    TrackRepeat, WriteError, track_key,
};

use crate::definitions::{CLOCK, clock};

const TRACK: TrackNum = TrackNum::new(1);
/// Time steps per chunk.
const STEPS: u32 = 4;
/// Runs of [STEPS] time steps each tile is written in.
const RUNS: u64 = 3;
/// First tick of the grid.
const START: i64 = 1_000;

/// Fixed axes of (extent, tile extent): 3, 1 and 4 tiles, edge tiles short on the first and the last.
const EXTENTS: [(u32, u32); 3] = [(5, 2), (4, 4), (10, 3)];

fn period() -> Ratio64 {
    Ratio64::new(1_000, 1).unwrap()
}

fn axes(extents: &[(u32, u32)]) -> Vec<FixedAxis<'static>> {
    extents
        .iter()
        .map(|&(extent, tile)| FixedAxis {
            name: "x",
            extent: UNib32(extent),
            tile_extent: UNib32(tile),
            unit: None,
            scale: Magnitude::ONE,
            coords: AxisCoords::Regular { origin: 0, step: 1 },
            downsample: true,
        })
        .collect()
}

fn grid_track<'a>(axes: &'a [FixedAxis<'a>]) -> TrackDef<'a> {
    TrackDef {
        number: TRACK,
        key: track_key(0x9d, &[Segment::Name("field")]),
        clock: ClockNum::new(CLOCK),
        dimensions: RefVec::Slice { slice: &[] },
        items: ww_tape::Items::numbers::<i32>(None),
        label: "field",
        repeat: TrackRepeat::Stream,
        kind: TrackKind::Series(GridDef {
            axes: RefVec::Slice { slice: axes },
            ..GridDef::of::<i32>(1, period())
        }),
        relations: RefVec::Slice { slice: &[] },
        owner: [0; 32],
    }
}

/// A grid of `extents` written tile by tile, run after run: its bytes, closed or as a crash leaves them, its track
/// id and layout.
struct Written {
    bytes: Vec<u8>,
    id: TrackId,
    layout: TileLayout,
}

/// Every tile of `layout`, the first axis outermost.
fn all_tiles(layout: &TileLayout) -> Vec<TileCoords> {
    let mut out = vec![vec![]];
    for &tiles in layout.tiles() {
        out = out
            .into_iter()
            .flat_map(|c: Vec<u32>| {
                (0..tiles).map(move |t| {
                    let mut c = c.clone();
                    c.push(t);
                    c
                })
            })
            .collect();
    }
    out.iter().map(|c| TileCoords::new(c).unwrap()).collect()
}

/// The head of the chunk of run `run` at tile `tile` of the track `id`.
fn head(id: TrackId, run: u64, tile: TileCoords) -> ChunkHead {
    let grid = GridDef::of::<i32>(1, period()).first_grid(START);
    let first = ItemNum::new(run * u64::from(STEPS));
    let (first_tick, last_tick) = grid.chunk_ticks(period(), first, STEPS).unwrap();
    ChunkHead {
        kind: BlockKind::Series,
        track_id: id,
        track_number: TRACK,
        first_item: first,
        count: STEPS,
        first_tick,
        last_tick,
        latest_end: last_tick,
        time_mode: 0,
        end_mode: EndMode::Absent,
        grid: Some(grid),
        tile,
    }
}

/// Write the grid of `extents`, every tile of every run, with a checkpoint every `checkpoint` bytes; closed when
/// `close`, else the bytes a crash leaves.
fn write(extents: &[(u32, u32)], checkpoint: u64, close: bool) -> Written {
    let axes = axes(extents);
    let track = grid_track(&axes);
    let layout = track.series().unwrap().tile_layout().unwrap();
    let id = track.id();
    let mut defs = ww_tape::Definitions::new();
    crate::definitions::hold(&mut defs, &[clock(CLOCK), ww_tape::Describe::Track(track)]).unwrap();
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    let tape = w.sink().tape();
    w.set_index_policy(IndexPolicy {
        bytes: exact_units::Bytes::new(checkpoint),
        interval: None,
        ..IndexPolicy::default()
    })
    .unwrap();
    let samples = [0u8; 4 * STEPS as usize];
    let spec = ColumnSpec {
        kind: ColumnKind::Samples,
        codec: Codec::RAW,
        param: 0,
        len: samples.len(),
    };
    for run in 0..RUNS {
        for tile in all_tiles(&layout) {
            w.write_chunk(&defs, &head(id, run, tile), &[(spec, &samples[..])])
                .unwrap();
        }
    }
    if close {
        w.close().unwrap();
    } else {
        drop(w);
    }
    Written {
        bytes: tape.to_vec().unwrap(),
        id,
        layout,
    }
}

/// Every entry of a grid chunk in the final index carries the tile number its head's coordinates pack to; the
/// piece verifies, its index the same set as its walk.
#[test]
fn each_grid_entry_carries_its_tile_number() {
    let g = write(&EXTENTS, 1 << 20, true);
    assert_eq!(g.layout.tiles(), [3, 1, 4]);
    assert_eq!(g.layout.widths(), [2, 0, 2]);
    let r = TapeReader::open(&g.bytes[..]).unwrap();
    assert!(r.verify(0).unwrap().holds);
    let entries = r.track_entries(0, g.id).unwrap();
    assert_eq!(entries.len(), RUNS as usize * 12);
    let mut buf = vec![];
    for e in &entries {
        let head = r.read_chunk_head(0, e, &mut buf).unwrap();
        assert_eq!(e.tile, g.layout.pack(&head.tile), "{e:?}");
        assert!(e.range.is_none());
    }
    let last = entries.last().unwrap();
    assert_eq!(last.tile, Some(2 << 2 | 3), "tile (2, 0, 3)");
}

/// A series' entries carry no tile number: bytes 72..88 stay zero, as before grids.
#[test]
fn a_series_entry_carries_no_tile_number() {
    let g = write(&[], 1 << 20, true);
    assert_eq!(g.layout, TileLayout::SERIES);
    let r = TapeReader::open(&g.bytes[..]).unwrap();
    let entries = r.track_entries(0, g.id).unwrap();
    assert_eq!(entries.len(), RUNS as usize);
    assert!(entries.iter().all(|e| e.tile.is_none()));
}

/// A chunk whose coordinates name no tile of its grid is refused before anything is written: one past an axis'
/// tiles, one too few, and coordinates on a series.
#[test]
fn coordinates_outside_the_grid_are_refused() {
    for (extents, coords) in [
        (&EXTENTS[..], &[3, 0, 0][..]),
        (&EXTENTS[..], &[0, 1, 0][..]),
        (&EXTENTS[..], &[0, 0][..]),
        (&[][..], &[0][..]),
    ] {
        let axes = axes(extents);
        let track = grid_track(&axes);
        let id = track.id();
        let mut defs = ww_tape::Definitions::new();
        crate::definitions::hold(&mut defs, &[clock(CLOCK), ww_tape::Describe::Track(track)])
            .unwrap();
        let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
        let samples = [0u8; 4 * STEPS as usize];
        let spec = ColumnSpec {
            kind: ColumnKind::Samples,
            codec: Codec::RAW,
            param: 0,
            len: samples.len(),
        };
        let h = head(id, 0, TileCoords::new(coords).unwrap());
        let r = w.write_chunk(&defs, &h, &[(spec, &samples[..])]);
        assert!(
            matches!(
                r,
                Err(WriteError::Describe(DescribeError::Kind {
                    error: SeriesError::TileOutside { .. },
                    ..
                }))
            ),
            "{coords:?}: {r:?}"
        );
        assert_eq!(w.chunks(), 0);
    }
}
