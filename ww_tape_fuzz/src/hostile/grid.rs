//! Grid tracks: the describe record of a grid track whose fixed axes take hostile values (more axes than
//! `GRID_FIXED_AXES_MAX`, extents and tile extents at 0, 1 and past the caps, listed coordinates out of order or of
//! another length, steps of 0, names empty or past `AXIS_NAME_LEN_MAX`), and grid chunks whose tile coordinates (their
//! count at byte 82, their values) take hostile values.

use super::chunk::{build, reseal, table};
use super::{Draw, SESSION, clock_item, definition, get, put};
use shrink_wrap::prelude::*;
use ww_tape::grid::{GRID_FIXED_AXES_MAX, GRID_LISTED_MAX, GRID_TILE_CELLS_MAX};
use ww_tape::{
    AxisCoords, BlockId, BlockKind, BlockWriter, ChunkHead, ClockNum, Codec, ColumnKind,
    ColumnSpec, Describe, EndMode, FixedAxis, GridDef, ItemNum, Magnitude, Ratio64, SeriesGrid,
    TileCoords, TrackDef, TrackKind, TrackNum, track_key,
};

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
/// Each chunk is written with coordinates drawn near its axes' tile counts; then byte 82 (the count) or a
/// coordinate may take a hostile value, the head CRC made to hold, so a reader takes the head.
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
            coords: match regular {
                true => AxisCoords::Regular {
                    origin: i64::from(extent),
                    step: i64::from(tile % 3) - 1,
                },
                false => AxisCoords::Listed(RefVec::Slice { slice: list }),
            },
            downsample: tile % 2 == 0,
        })
        .collect();
    let item = definition(&Describe::Track(grid_track(&fixed)));
    let mut out = describe_block(BlockId::new(SESSION, 0), &[&clock_item(), &item]);
    let grid = SeriesGrid::start(0, 0, Magnitude::ONE);
    let samples = [0u8; 4];
    let spec = ColumnSpec {
        kind: ColumnKind::Samples,
        codec: Codec::RAW,
        param: 0,
        len: samples.len(),
    };
    for k in 0..1 + d.below(4) {
        let coords: Vec<u32> = shape
            .iter()
            .take(GRID_FIXED_AXES_MAX)
            .map(|&(extent, tile, ..)| {
                let tiles = u64::from(extent.div_ceil(tile.max(1)));
                d.value(32, &[tiles], &[tiles.saturating_sub(1), 0]) as u32
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
        };
        let mut b = build(&head, &[(spec, &samples)]);
        match d.u8() % 4 {
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
    out
}
