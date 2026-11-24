//! Grid tracks: the describe record of a grid track whose fixed axes take hostile values (more axes than
//! `GRID_FIXED_AXES_MAX`, extents and tile extents at 0, 1 and past the caps, listed coordinates out of order or of
//! another length, steps of 0, names empty or past `AXIS_NAME_LEN_MAX`), and grid chunks whose tile coordinates (their
//! count at byte 82, their values) and overview level (byte 83) take hostile values, overview chunks whose min, max
//! and mean columns hold the cells of their level's tile or not, or lack one, and tile summary blocks whose tile (its
//! axis count and coordinates after the scale) takes hostile values.

use super::chunk::{build, reseal, table};
use super::index::{index_block, reseal_index};
use super::summary::hit_tile;
use super::{Draw, SESSION, clock_item, definition, get, put, seal};
use shrink_wrap::prelude::*;
use ww_tape::chunk::COLUMN_ENTRY_LEN;
use ww_tape::grid::{
    GRID_FIXED_AXES_MAX, GRID_LISTED_MAX, GRID_OVERVIEW_LEVELS_MAX, GRID_TILE_CELLS_MAX,
};
use ww_tape::index::{IndexEntry, IndexKind};
use ww_tape::{
    AxisCoords, Block, BlockId, BlockKind, BlockWriter, ChunkHead, ClockNum, Codec, ColumnKind,
    ColumnSpec, Describe, Element, ElementDesc, EndMode, FixedAxis, GridDef, ItemNum, Magnitude,
    Ratio64, SampleShape, SeriesGrid, TileCoords, TileLayout, TrackDef, TrackId, TrackKind,
    TrackNum, set_id, track_key,
};
use ww_tape_summary::{SeriesStart, SeriesStats, SeriesSummaries, SeriesSummarySpec};

/// The track number of the hostile grid.
pub(crate) const TRACK: u32 = 1;
/// Most listed coordinates an axis of the hostile grid carries: the definition stays small.
const LISTED_DRAWN_MAX: usize = 12;
/// Names an axis takes: empty, short, at and past `AXIS_NAME_LEN_MAX` (64 bytes).
const NAMES: [&str; 4] = [
    "",
    "x",
    "0123456789012345678901234567890123456789012345678901234567890123",
    "01234567890123456789012345678901234567890123456789012345678901234",
];

/// The period of the hostile grid's time axis.
pub(crate) fn period() -> Ratio64 {
    Ratio64::new(1_000, 1).expect("a ratio")
}

/// The definition of grid track [TRACK] on clock 1 with `axes`.
pub(crate) fn grid_track<'a>(axes: &'a [FixedAxis<'a>]) -> TrackDef<'a> {
    TrackDef {
        number: TrackNum::new(TRACK),
        key: track_key(0x9d, &[ww_tape::Segment::Name("hostile-grid")]),
        clock: ClockNum::new(1),
        dimensions: RefVec::Slice { slice: &[] },
        items: ww_tape::Items::numbers::<i32>(None),
        label: "hostile",
        repeat: ww_tape::TrackRepeat::Stream,
        kind: TrackKind::Series(GridDef {
            axes: RefVec::Slice { slice: axes },
            ..GridDef::of::<i32>(1, period())
        }),
        relations: RefVec::Slice { slice: &[] },
        owner: [0; 32],
    }
}

/// Listed coordinates for an axis of `extent` cells: one per cell or not, ascending, descending or out of order.
fn listed(d: &mut Draw<'_>, extent: u32) -> Vec<i64> {
    let n = match d.u8() % 4 {
        0 => (extent as usize).saturating_sub(1),
        1 => extent as usize + 1,
        _ => extent as usize,
    }
    .min(LISTED_DRAWN_MAX);
    let mut bits = d.bits();
    let mut v: Vec<i64> = (0..n).map(|_| bits.u64() as i64 >> 8).collect();
    match d.u8() % 4 {
        0 => v.sort_unstable(),
        1 => v.sort_unstable_by(|a, b| b.cmp(a)),
        2 => {
            v.sort_unstable();
            if let Some(i) = (n > 1).then(|| d.below(n - 1)) {
                v[i + 1] = v[i];
            }
        }
        _ => {}
    }
    v
}

/// An axis drawn: its extent, tile extent, name and whether its coordinates are regular.
type AxisShape = (u32, u32, &'static str, bool);

/// The axes drawn from the input, their listed coordinates beside them.
fn axes(d: &mut Draw<'_>) -> (Vec<AxisShape>, Vec<Vec<i64>>) {
    let n = d.value(8, &[GRID_FIXED_AXES_MAX as u64], &[]) as usize % (GRID_FIXED_AXES_MAX + 3);
    let mut shape = vec![];
    let mut lists = vec![];
    for _ in 0..n {
        // mostly small extents, so most grids hold: a hostile one now and then
        let extent = match d.u8() % 8 {
            0 => d.value(32, &[u64::from(GRID_LISTED_MAX), GRID_TILE_CELLS_MAX], &[]) as u32,
            _ => 1 + u32::from(d.u8() % 12),
        };
        let tile = match d.u8() % 6 {
            0 => d.value(32, &[u64::from(extent)], &[u64::from(extent)]) as u32,
            _ => 1 + u32::from(d.u8()) % extent.max(1),
        };
        let name = NAMES[match d.u8() % 16 {
            0 => 0,
            1 => 2,
            2 => 3,
            _ => 1,
        }];
        let regular = !d.u8().is_multiple_of(3);
        lists.push(if regular { vec![] } else { listed(d, extent) });
        shape.push((extent, tile, name, regular));
    }
    (shape, lists)
}

/// A block of definitions holding `items`, large enough for an axis list of hostile names and coordinates.
fn describe_block(id: BlockId, items: &[&[u8]]) -> Vec<u8> {
    let mut buf = vec![0u8; 64 << 10];
    let mut w =
        BlockWriter::new(&mut buf, BlockKind::Records, id, &[]).expect("a block of definitions");
    for item in items {
        w.push_describe(&[item]).expect("items fit 64 KiB");
    }
    let n = w.finish();
    buf.truncate(n);
    buf
}

/// A block of definitions of a grid track with hostile axes, then grid chunks of it with hostile tile coordinates.
///
/// Each chunk is written with coordinates drawn near its axes' tile counts, at level 0 or as an overview chunk of a
/// level drawn near the grid's levels (`overview_columns`); then byte 82 (the count), a coordinate or byte 83 (the
/// level) may take a hostile value, the head CRC made to hold, so a reader takes the head. A checkpoint index may
/// follow, whose chunk entries carry the tile number the grid packs, a hostile one or none.
pub fn grid(data: &[u8]) -> Vec<u8> {
    let mut d = Draw::new(data);
    let (shape, lists) = axes(&mut d);
    let fixed: Vec<FixedAxis<'_>> = shape
        .iter()
        .zip(&lists)
        .map(|(&(extent, tile, name, regular), list)| FixedAxis {
            name,
            extent: UNib32(extent),
            tile_extent: UNib32(tile),
            unit: None,
            scale: Magnitude::ONE,
            coords: if regular {
                AxisCoords::Regular {
                    origin: i64::from(extent),
                    step: i64::from(tile % 3) - 1,
                }
            } else {
                AxisCoords::Listed(RefVec::Slice { slice: list })
            },
            downsample: tile % 2 == 0,
        })
        .collect();
    let item = definition(&Describe::Track(grid_track(&fixed)));
    let mut out = describe_block(BlockId::new(SESSION, 0), &[&clock_item(), &item]);
    let grid = SeriesGrid::start(0, 0, Magnitude::ONE);
    let extents: Vec<(u32, u32)> = shape.iter().map(|&(e, t, ..)| (e, t)).collect();
    let downsample = shape
        .iter()
        .enumerate()
        .filter(|(_, (_, tile, ..))| tile % 2 == 0)
        .fold(0u32, |m, (i, _)| {
            m | 1u32.checked_shl(i as u32).unwrap_or(0)
        });
    let layout = TileLayout::new(&extents)
        .ok()
        .map(|l| l.with_downsample(downsample));
    let levels = layout.map_or(0, |l| l.overview_levels());
    for k in 0..=d.below(4) {
        let coords: Vec<u32> = shape
            .iter()
            .take(GRID_FIXED_AXES_MAX)
            .map(|&(extent, tile, ..)| {
                // half the time a tile of the grid, so chunks are found and their overviews read
                let tiles = u64::from(extent.div_ceil(tile.max(1)));
                if d.bool() {
                    (d.u64() % tiles.max(1)) as u32
                } else {
                    d.value(32, &[tiles], &[tiles.saturating_sub(1), 0]) as u32
                }
            })
            .collect();
        let first = k as u64;
        let (first_tick, last_tick) = grid
            .chunk_ticks(period(), ItemNum::new(first), 1)
            .unwrap_or((0, 0));
        let head = ChunkHead {
            kind: BlockKind::Series,
            track_id: grid_track(&fixed).id(),
            track_number: TrackNum::new(TRACK),
            first_item: ItemNum::new(first),
            count: 1,
            first_tick,
            last_tick,
            latest_end: last_tick,
            time_mode: 0,
            end_mode: EndMode::Absent,
            grid: Some(grid),
            tile: TileCoords::new(&coords).expect("at most GRID_FIXED_AXES_MAX coordinates"),
            level: 0,
        };
        let level = match d.u8() % 4 {
            _ if coords.is_empty() => 0,
            0 => 0,
            1 | 2 => 1 + d.below(usize::from(levels.max(1))) as u8,
            _ => d.value(8, &[u64::from(levels)], &[u64::from(levels) + 1]) as u8,
        }
        .min(GRID_OVERVIEW_LEVELS_MAX);
        let head = ChunkHead { level, ..head };
        let columns = overview_columns(&mut d, &head, layout);
        let columns: Vec<(ColumnSpec, &[u8])> = columns.iter().map(|(s, b)| (*s, &b[..])).collect();
        let mut b = build(&head, &columns);
        // one chunk in two keeps its head: a scan stops at the first that doesn't read
        match d.u8() % 8 {
            // a column's kind: one of the three missing, twice or beside a samples column
            5 if level > 0 => {
                let at = table(&b) + COLUMN_ENTRY_LEN * d.below(3);
                put(&mut b, at, 1, d.value(8, &[], &[9, 10, 11, 1]));
            }
            4 => put(
                &mut b,
                83,
                1,
                d.value(
                    8,
                    &[u64::from(GRID_OVERVIEW_LEVELS_MAX), u64::from(levels)],
                    &[u64::from(level)],
                ),
            ),
            0 => put(
                &mut b,
                82,
                1,
                d.value(8, &[GRID_FIXED_AXES_MAX as u64], &[coords.len() as u64]),
            ),
            1 if !coords.is_empty() => {
                let at = table(&b) - 4 * (1 + d.below(coords.len()));
                let own = get(&b, at, 4);
                put(&mut b, at, 4, d.value(32, &[], &[own]));
            }
            _ => {}
        }
        reseal(&mut b, false);
        out.extend(b);
    }
    // past the input's end too: every draw is 0 there
    if !d.bool() {
        summaries(&mut d, &mut out, &shape, grid_track(&fixed).id(), layout);
    }
    if d.u8() % 3 != 1 {
        checkpoint(&mut d, &mut out, layout);
    }
    out
}

/// Most cells an overview column holds: the blocks stay small.
const OVERVIEW_CELLS_DRAWN_MAX: u64 = 64;

/// The columns of chunk `head`: a samples column of one cell at level 0; at an overview level the min, max and mean
/// columns, each of the cells the level's tile holds (at most [OVERVIEW_CELLS_DRAWN_MAX], else a few) or one more or
/// less now and then.
fn overview_columns(
    d: &mut Draw<'_>,
    head: &ChunkHead,
    layout: Option<TileLayout>,
) -> Vec<(ColumnSpec, Vec<u8>)> {
    let raw = |kind, bytes: Vec<u8>| {
        let spec = ColumnSpec {
            kind,
            codec: Codec::RAW,
            param: 0,
            len: bytes.len(),
        };
        (spec, bytes)
    };
    if head.level == 0 {
        return vec![raw(ColumnKind::Samples, vec![0; 4])];
    }
    let cells = layout
        .and_then(|l| l.overview(head.level))
        .and_then(|o| o.tile_cells(&head.tile))
        .filter(|c| *c <= OVERVIEW_CELLS_DRAWN_MAX)
        .unwrap_or(1 + d.below(4) as u64);
    let cells = match d.u8() % 8 {
        0 => cells + 1,
        1 => cells - 1,
        _ => cells,
    };
    let mut bits = d.bits();
    [
        ColumnKind::OverviewMin,
        ColumnKind::OverviewMax,
        ColumnKind::OverviewMean,
    ]
    .into_iter()
    .map(|kind| {
        let bytes = (0..cells)
            .flat_map(|_| (bits.u64() as i32 >> 8).to_le_bytes())
            .collect();
        raw(kind, bytes)
    })
    .collect()
}

/// Most cells of a hostile tile's time step: the blocks stay small.
const TILE_CELLS_DRAWN_MAX: u64 = 6;

/// Summary blocks of one to three tiles, mostly of the grid, put after the blocks in `f`: a few
/// time steps each, at most [TILE_CELLS_DRAWN_MAX] cells a step; then the axis count or a coordinate of a block's
/// tile may take a hostile value.
fn summaries(
    d: &mut Draw<'_>,
    f: &mut Vec<u8>,
    shape: &[AxisShape],
    track_id: TrackId,
    layout: Option<TileLayout>,
) {
    let samples = SampleShape::new(ElementDesc::of(Element::I32), 1);
    for _ in 0..=d.below(3) {
        let coords: Vec<u32> = shape
            .iter()
            .take(GRID_FIXED_AXES_MAX)
            .map(|&(extent, tile, ..)| {
                // mostly a tile of the grid: a hostile coordinate now and then
                let tiles = extent.div_ceil(tile.max(1));
                match d.u8() % 8 {
                    7 => d.value(32, &[u64::from(tiles)], &[u64::from(tiles)]) as u32,
                    _ => d.u32() % tiles.max(1),
                }
            })
            .collect();
        let tile = TileCoords::new(&coords).expect("at most GRID_FIXED_AXES_MAX coordinates");
        let cells = if tile.is_empty() {
            1
        } else {
            layout
                .and_then(|l| l.tile_cells(&tile))
                .unwrap_or(1)
                .min(TILE_CELLS_DRAWN_MAX)
        };
        let first = d.below(40) as u64;
        let start = SeriesStart {
            track_id,
            track_number: TrackNum::new(TRACK),
            shape: samples,
            first_item: ItemNum::new(first),
            grid: SeriesGrid::start(0, 0, Magnitude::ONE),
            period: period(),
            tile,
            tile_cells: cells,
        };
        let spec = SeriesSummarySpec {
            base_shift: SeriesSummarySpec::BASE_SHIFT_MIN + d.below(3) as u8,
            stats: SeriesStats::all(ElementDesc::of(Element::I32)),
            clip: None,
        };
        let Ok(mut s) = SeriesSummaries::<i32>::new(start, spec) else {
            continue;
        };
        s.set_max_block_bytes(exact_units::Bytes::new(384));
        let mut bits = d.bits();
        let steps = 1 + d.below(80);
        let xs: Vec<i32> = (0..steps as u64 * cells)
            .map(|_| bits.u64() as i32 >> 8)
            .collect();
        if s.push(&xs, 0).is_err() || s.finish().is_err() {
            continue;
        }
        for mut b in s.take_blocks() {
            if set_id(&mut b, BlockId::new(SESSION, 1)).is_err() {
                continue;
            }
            if d.u8() % 4 == 3 {
                hit_tile(d, &mut b);
                seal(&mut b);
            }
            f.extend(b);
        }
    }
}

/// A checkpoint index of the blocks in `f` put after them: each chunk's or summary block's entry carries the tile
/// number `layout` packs its head's coordinates to, a hostile number (bits outside the fields, any bits) or none; a
/// chunk's entry carries its level or now and then another.
fn checkpoint(d: &mut Draw<'_>, f: &mut Vec<u8>, layout: Option<TileLayout>) {
    let mut entries = vec![];
    let mut at = 0;
    while let Some(b) = f.get(at..).and_then(|r| Block::read(r).ok()) {
        let len = b.bytes().len();
        for mut e in IndexEntry::all_of_block(at as u64, &b) {
            let tile = match e.kind {
                BlockKind::Series => ChunkHead::read_head(b.bytes()).ok().map(|h| h.tile),
                BlockKind::Summary => b.summary_head().map(|h| h.tile),
                _ => None,
            };
            if matches!(e.kind, BlockKind::Series | BlockKind::Summary) {
                let packed = tile.zip(layout).and_then(|(t, l)| l.pack(&t));
                let outside = layout.map_or(0, |l| !l.field_bits());
                let mut bits = d.bits();
                e.tile = match d.u8() % 4 {
                    0 => None,
                    1 => Some(packed.unwrap_or(0) | (outside & u128::from(bits.u64()) << 64)),
                    2 => Some(u128::from(bits.u64()) << 64 | u128::from(bits.u64())),
                    _ => packed,
                };
            }
            if e.kind == BlockKind::Series && d.u8().is_multiple_of(8) {
                // a level other than the chunk's: the box of that level finds the entry, and reads the chunk
                e.level = d.below(usize::from(GRID_OVERVIEW_LEVELS_MAX) + 1) as u8;
            }
            entries.push(e);
        }
        at += len;
    }
    let mut b = index_block(d, BlockId::new(SESSION, 9), IndexKind::Checkpoint, &entries);
    put(&mut b, 40, 8, f.len() as u64);
    put(&mut b, 48, 8, u64::MAX);
    reseal_index(&mut b);
    f.extend(b);
}
