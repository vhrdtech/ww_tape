//! A grid's time pyramid over its tiles (FMT-15): each tile's summary blocks name their tile, their index entries
//! carry its tile number, and a region's timeline merges the nodes of the tiles a box meets, as a naive model of the
//! cells computes it.

use std::ops::Range;

use shrink_wrap::prelude::*;
use ww_tape::chunk::ChunkHead;
use ww_tape::index::IndexPolicy;
use ww_tape::{
    AxisCoords, BlockKind, ClockNum, Codec, ColumnKind, ColumnSpec, DescribeError, EndMode,
    FixedAxis, GridDef, ItemNum, Magnitude, MemSink, Ratio64, SeriesError, SyncPolicy, TapeReader,
    TapeWriter, TileCoords, TileLayout, TrackDef, TrackId, TrackKind, TrackNum, TrackRepeat,
    WriteError, track_key,
};
use ww_tape_summary::{
    SeriesStart, SeriesStats, SeriesSummaries, SeriesSummarySpec, Value, region_timeline,
};

use crate::definitions::{CLOCK, clock};

const TRACK: TrackNum = TrackNum::new(1);
/// Time steps per chunk.
const STEPS: u32 = 20;
/// Chunks per tile.
const RUNS: u64 = 3;
/// A level-0 node holds 2^4 time steps.
const BASE_SHIFT: u8 = 4;
const START: i64 = 1_000;
/// Fixed axes of (extent, tile extent): 3, 1 and 4 tiles, edge tiles short on the first and the last.
const EXTENTS: [(u32, u32); 3] = [(5, 2), (4, 4), (10, 3)];

fn period() -> Ratio64 {
    Ratio64::new(1_000, 1).unwrap()
}

fn spec() -> SeriesSummarySpec {
    SeriesSummarySpec {
        base_shift: BASE_SHIFT,
        stats: SeriesStats::all(ww_tape::ElementDesc::of(ww_tape::Element::I32)),
        clip: None,
    }
}

fn axes() -> Vec<FixedAxis<'static>> {
    EXTENTS
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
        key: track_key(0x9e, &[Segment::Name("field")]),
        clock: ClockNum::new(CLOCK),
        dimensions: RefVec::Slice { slice: &[] },
        items: ww_tape::Items::numbers::<i32>(None),
        label: "field",
        repeat: TrackRepeat::Stream,
        kind: TrackKind::Series(GridDef {
            axes: RefVec::Slice { slice: axes },
            summaries: Some(spec()),
            ..GridDef::of::<i32>(1, period())
        }),
        relations: RefVec::Slice { slice: &[] },
        owner: [0; 32],
    }
}

use ww_tape::Segment;

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

/// The cells of tile `tile`, in tile order: each cell's coordinates along the fixed axes.
fn tile_cells(tile: &TileCoords) -> Vec<[u32; 3]> {
    let r: Vec<Range<u32>> = tile
        .as_slice()
        .iter()
        .zip(EXTENTS)
        .map(|(&c, (extent, t))| c * t..((c + 1) * t).min(extent))
        .collect();
    let mut out = vec![];
    for a in r[0].clone() {
        for b in r[1].clone() {
            for c in r[2].clone() {
                out.push([a, b, c]);
            }
        }
    }
    out
}

/// The value of cell `cell` at time step `step`.
fn value(step: u64, cell: [u32; 3]) -> i32 {
    let k = step as i64 * 7919
        + i64::from(cell[0]) * 104_729
        + i64::from(cell[1]) * 31
        + i64::from(cell[2]) * 1009;
    (k % 20_011) as i32 - 10_000
}

/// The grid written tile by tile, run after run, each tile's summaries beside its chunks; its bytes, closed or as a
/// crash leaves them, and its track id and layout.
fn write(close: bool) -> (Vec<u8>, TrackId, TileLayout) {
    let axes = axes();
    let track = grid_track(&axes);
    let layout = track.series().unwrap().tile_layout().unwrap();
    let id = track.id();
    let shape = track.series_shape().unwrap();
    let mut defs = ww_tape::Definitions::new();
    crate::definitions::hold(&mut defs, &[clock(CLOCK), ww_tape::Describe::Track(track)]).unwrap();
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    let tape = w.sink().tape();
    w.set_session(crate::id(0).session).unwrap();
    w.set_index_policy(IndexPolicy {
        bytes: exact_units::Bytes::new(4096),
        interval: None,
        ..IndexPolicy::default()
    })
    .unwrap();
    let grid = GridDef::of::<i32>(1, period()).first_grid(START);
    let tiles = all_tiles(&layout);
    let mut trees: Vec<SeriesSummaries<i32>> = tiles
        .iter()
        .map(|tile| {
            let start = SeriesStart {
                track_id: id,
                track_number: TRACK,
                shape,
                first_item: ItemNum::new(0),
                grid,
                period: period(),
                tile: *tile,
                tile_cells: layout.tile_cells(tile).unwrap(),
            };
            let mut s = SeriesSummaries::new(start, spec()).unwrap();
            // two nodes a block: a time range finds some of a tile's blocks
            s.set_max_block_bytes(exact_units::Bytes::new(256));
            s
        })
        .collect();
    for run in 0..RUNS {
        for (tile, tree) in tiles.iter().zip(&mut trees) {
            let first = run * u64::from(STEPS);
            let cells = tile_cells(tile);
            let xs: Vec<i32> = (first..first + u64::from(STEPS))
                .flat_map(|s| cells.iter().map(move |c| value(s, *c)))
                .collect();
            let bytes: Vec<u8> = xs.iter().flat_map(|x| x.to_le_bytes()).collect();
            let (first_tick, last_tick) = grid
                .chunk_ticks(period(), ItemNum::new(first), STEPS)
                .unwrap();
            let head = ChunkHead {
                kind: BlockKind::Series,
                track_id: id,
                track_number: TRACK,
                first_item: ItemNum::new(first),
                count: STEPS,
                first_tick,
                last_tick,
                latest_end: last_tick,
                time_mode: 0,
                end_mode: EndMode::Absent,
                grid: Some(grid),
                tile: *tile,
            };
            let spec = ColumnSpec {
                kind: ColumnKind::Samples,
                codec: Codec::RAW,
                param: 0,
                len: bytes.len(),
            };
            w.write_chunk(&defs, &head, &[(spec, &bytes[..])]).unwrap();
            tree.push(&xs, 0).unwrap();
            for b in tree.take_blocks() {
                w.defer_block(&defs, b).unwrap();
            }
        }
    }
    for tree in &mut trees {
        tree.finish().unwrap();
        for b in tree.take_blocks() {
            w.defer_block(&defs, b).unwrap();
        }
    }
    if close {
        w.close().unwrap();
    } else {
        drop(w);
    }
    (tape.to_vec().unwrap(), id, layout)
}

/// Every summary block of a tile carries its tile in its head and its tile number in its index entry; the piece
/// verifies.
#[test]
fn each_tile_summary_entry_carries_its_tile_number() {
    let (bytes, id, layout) = write(true);
    let r = TapeReader::open(&bytes[..]).unwrap();
    assert!(r.verify(0).unwrap().holds);
    let entries: Vec<_> = r
        .track_entries(0, id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == BlockKind::Summary)
        .collect();
    assert!(entries.len() >= 12 * 3, "every tile, several blocks");
    assert!(entries.iter().any(|e| e.level > 0));
    let mut buf = vec![];
    for e in &entries {
        let block = r.read_block(0, e, &mut buf).unwrap();
        let head = block.summary_head().unwrap();
        assert_eq!(e.tile, layout.pack(&head.tile), "{e:?}");
        assert!(e.tile.is_some());
    }
}

/// The naive statistics of the cells of `tiles` over time steps `steps`: minimum, maximum and sum.
fn naive(tiles: &[TileCoords], steps: Range<u64>) -> (i64, i64, f64) {
    let (mut lo, mut hi, mut sum) = (i64::MAX, i64::MIN, 0i64);
    for t in tiles {
        for c in tile_cells(t) {
            for s in steps.clone() {
                let v = i64::from(value(s, c));
                lo = lo.min(v);
                hi = hi.max(v);
                sum += v;
            }
        }
    }
    (lo, hi, sum as f64)
}

/// A region's timeline at levels 0 and 1 is the naive statistics of the cells of the tiles its box meets, node by
/// node, a tile partly inside counted whole; over a crashed piece whose stretch a reader scanned too.
#[test]
fn a_region_timeline_merges_the_tiles_of_its_box() {
    for close in [true, false] {
        let (bytes, id, layout) = write(close);
        let r = TapeReader::open(&bytes[..]).unwrap();
        let scanned = r
            .track_entries(0, id)
            .unwrap()
            .iter()
            .any(|e| e.kind == BlockKind::Summary && e.tile.is_none());
        assert_eq!(scanned, !close, "a scan lists blocks without tile numbers");
        let steps = RUNS * u64::from(STEPS);
        for cells in [
            vec![0..5, 0..4, 0..10],
            vec![1..3, 0..1, 4..7],
            vec![4..5, 2..3, 9..10],
        ] {
            let tiles = layout.tile_box(&cells).unwrap();
            let met: Vec<TileCoords> = all_tiles(&layout)
                .into_iter()
                .filter(|t| tiles.holds_coords(t))
                .collect();
            for level in [0u8, 1] {
                let got = region_timeline(&r, 0, id, i64::MIN..i64::MAX, &tiles, level).unwrap();
                let shift = BASE_SHIFT + 4 * level;
                let nodes = steps.div_ceil(1 << shift);
                assert_eq!(got.len() as u64, nodes, "{cells:?} level {level}");
                for n in &got {
                    let from = n.node << shift;
                    let to = ((n.node + 1) << shift).min(steps);
                    assert_eq!((n.first_item.get(), n.items), (from, to - from));
                    assert_eq!(n.tiles as usize, met.len());
                    let cells: u64 = met.iter().map(|t| layout.tile_cells(t).unwrap()).sum();
                    assert_eq!(n.samples, cells * (to - from));
                    let (lo, hi, sum) = naive(&met, from..to);
                    let ch = &n.stats.channels[0];
                    assert_eq!(ch.min[0], Value::I64(lo), "{cells:?} node {}", n.node);
                    assert_eq!(ch.max[0], Value::I64(hi));
                    assert_eq!(ch.sum[0], sum);
                }
            }
        }
        // a time range keeps the blocks meeting it, a box of no tile finds none
        let tiles = layout.tile_box(&[0..5, 0..4, 0..10]).unwrap();
        let late = START + 1_000 * 50;
        let got = region_timeline(&r, 0, id, late..late + 1, &tiles, 0).unwrap();
        assert!(got.iter().any(|n| n.node == 50 >> BASE_SHIFT));
        assert!(got.iter().all(|n| n.node != 0));
    }
}

/// A summary block naming a tile its grid doesn't hold, or none on a grid, is refused before it is written.
#[test]
fn a_tile_summary_outside_the_grid_is_refused() {
    let axes = axes();
    let track = grid_track(&axes);
    let id = track.id();
    let shape = track.series_shape().unwrap();
    let mut defs = ww_tape::Definitions::new();
    crate::definitions::hold(&mut defs, &[clock(CLOCK), ww_tape::Describe::Track(track)]).unwrap();
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    w.set_session(crate::id(0).session).unwrap();
    for (tile, cells) in [
        (TileCoords::new(&[3, 0, 0]).unwrap(), 12),
        (TileCoords::new(&[0, 0]).unwrap(), 8),
        (TileCoords::NONE, 1),
    ] {
        let start = SeriesStart {
            track_id: id,
            track_number: TRACK,
            shape,
            first_item: ItemNum::new(0),
            grid: GridDef::of::<i32>(1, period()).first_grid(START),
            period: period(),
            tile,
            tile_cells: cells,
        };
        let mut s = SeriesSummaries::<i32>::new(start, spec()).unwrap();
        s.push(&vec![1; cells as usize * 16], 0).unwrap();
        s.finish().unwrap();
        let b = s.take_blocks().pop().unwrap();
        assert!(
            matches!(
                w.defer_block(&defs, b),
                Err(WriteError::Describe(DescribeError::Kind {
                    error: SeriesError::TileOutside { .. },
                    ..
                }))
            ),
            "{tile:?}"
        );
    }
}
