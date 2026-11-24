//! ww_tape grids on any bytes: a grid definition decodes without panic, and one that decodes has a tile layout (its
//! axes were checked on decode); tile layouts of extents from the input are made exactly when their axes hold, and
//! their tile numbers pack, unpack and test boxes alike by shift and mask and by coordinates; the space overview
//! levels of a layout halve its downsampled axes and keep its tiles ([overview_laws]). Read as a tape, a grid track's
//! chunks are found at each overview level and an overview chunk's columns read ([chunks]), and its tile summaries
//! are found and merged into a region's timeline without panic ([summaries]).

use std::cmp::Ordering;
use std::ops::Range;

use super::hostile::GridOf;
use crate::hostile::Draw;
use shrink_wrap::prelude::*;
use ww_tape::chunk::{Chunk, ColumnKind};
use ww_tape::grid::{GRID_FIXED_AXES_MAX, GRID_OVERVIEW_LEVELS_MAX, TILE_NUMBER_BITS};
use ww_tape::reader::ByteSource;
use ww_tape::{
    GridDef, Overview, SampleShape, TapeReader, TileBox, TileCoords, TileLayout, TrackId,
};

/// Bits of a field of `tiles` tiles, ⌈log2(tiles)⌉, computed apart from the layout.
fn width(tiles: u32) -> u32 {
    (0..=32)
        .find(|b| u64::from(tiles) <= 1u64 << b)
        .expect("tiles fit 2^32")
}

/// Whether a layout of `extents` holds: at most [GRID_FIXED_AXES_MAX] axes, each of an extent of 1 and up and a tile
/// extent of 1 to it, the fields at most [TILE_NUMBER_BITS].
fn holds(extents: &[(u32, u32)]) -> bool {
    extents.len() <= GRID_FIXED_AXES_MAX
        && extents.iter().all(|&(e, t)| e > 0 && t > 0 && t <= e)
        && extents
            .iter()
            .map(|&(e, t)| width(e.div_ceil(t)))
            .sum::<u32>()
            <= TILE_NUMBER_BITS
}

/// Whether tile `k` of an axis of `(extent, tile)` holds a cell of `cells`.
fn tile_meets(k: u32, (extent, tile): (u32, u32), cells: &Range<u32>) -> bool {
    let lo = u64::from(k) * u64::from(tile);
    let hi = (lo + u64::from(tile)).min(u64::from(extent));
    let (a, b) = (
        u64::from(cells.start),
        u64::from(cells.end).min(u64::from(extent)),
    );
    a.max(lo) < b.min(hi)
}

/// Tile numbers of `l`, whose axes are `extents`, drawn from `d`: packed, unpacked, by axis, and tested against boxes
/// by shift and mask, by coordinates and cell by cell.
fn laws(l: &TileLayout, extents: &[(u32, u32)], d: &mut Draw<'_>) {
    overview_laws(&l.with_downsample(d.u32()), d);
    assert_eq!(l.axes(), extents.len());
    assert!(l.bits() <= TILE_NUMBER_BITS, "the fields fit a tile number");
    for _ in 0..4 {
        let coords: Vec<u32> = l
            .tiles()
            .iter()
            .map(|&t| match d.u8() % 4 {
                0 => t,
                1 => t.saturating_sub(1),
                _ => d.u32() % t.max(1),
            })
            .collect();
        let c = TileCoords::new(&coords).expect("at most GRID_FIXED_AXES_MAX");
        let packed = l.pack(&c);
        assert_eq!(packed.is_some(), l.holds(&c));
        if let Some(n) = packed {
            assert_eq!(l.unpack(n), Some(c));
            assert_eq!(n & !l.field_bits(), 0, "only the fields' bits");
            for (axis, k) in coords.iter().enumerate() {
                assert_eq!(l.coord(n, axis), Some(*k));
            }
        }
        let any = u128::from(d.u64()) << (d.u8() % 64) | u128::from(d.u64());
        if let Some(back) = l.unpack(any) {
            assert_eq!(l.pack(&back), Some(any));
        }
        let cells: Vec<Range<u32>> = extents
            .iter()
            .map(|&(e, _)| {
                let a = d.u32() % e.saturating_add(2);
                a..a.saturating_add(d.u32() % e.saturating_add(2))
            })
            .collect();
        let b = l.tile_box(&cells).expect("one range per axis");
        if let Some(n) = packed {
            let meets = coords
                .iter()
                .zip(extents)
                .zip(&cells)
                .all(|((k, e), r)| tile_meets(*k, *e, r));
            assert_eq!(b.holds(n), meets, "{cells:?} {coords:?}");
            assert_eq!(b.holds_coords(&c), meets);
        }
        assert!(
            !b.holds(any) || l.unpack(any).is_some_and(|c| b.holds_coords(&c)),
            "a number in the box is a tile of the grid in the box"
        );
    }
}

/// Whether overviews of `l` halve each of its axes.
fn halved(l: &TileLayout) -> Vec<bool> {
    (0..l.axes())
        .map(|i| {
            u32::try_from(i)
                .ok()
                .and_then(|i| l.downsample().checked_shr(i))
                .is_some_and(|b| b & 1 != 0)
        })
        .collect()
}

/// The space overview levels of `l`: as many as every downsampled tile extent halves evenly, at most
/// [GRID_OVERVIEW_LEVELS_MAX]; each level as [level_laws] checks it; the level for a pixel as [pixel_laws] does.
fn overview_laws(l: &TileLayout, d: &mut Draw<'_>) {
    let halved = halved(l);
    let levels = l.overview_levels();
    let even = l
        .extents()
        .iter()
        .zip(&halved)
        .filter(|(_, h)| **h)
        .map(|((_, t), _)| t.trailing_zeros())
        .min()
        .unwrap_or(0);
    assert_eq!(
        u32::from(levels),
        even.min(u32::from(GRID_OVERVIEW_LEVELS_MAX)),
        "levels while every downsampled tile extent halves"
    );
    assert_eq!(
        l.overview(levels.saturating_add(1)),
        None,
        "no level past them"
    );
    for k in 0..=levels {
        level_laws(l, k, &halved, d);
    }
    pixel_laws(l, &halved, d);
}

/// Level `k` of `l`, whose axes `halved` overviews halve: each halved axis halved k times, the others kept, the same
/// tiles and tile numbers, an overview tile of at most its tile's cells; an overview has no levels of its own.
fn level_laws(l: &TileLayout, k: u8, halved: &[bool], d: &mut Draw<'_>) {
    let o = l.overview(k).expect("a level up to the grid's");
    assert_eq!(o.level(), k, "the layout of level k");
    assert_eq!(o.tiles(), l.tiles(), "the same tiles at every level");
    assert_eq!(o.widths(), l.widths(), "the same tile number fields");
    for ((&(e, t), &at_level), h) in l.extents().iter().zip(o.extents()).zip(halved) {
        match h {
            true => assert_eq!(at_level, (e.div_ceil(1 << k), t >> k), "halved k times"),
            false => assert_eq!(at_level, (e, t), "kept"),
        }
    }
    if k > 0 {
        assert_eq!(o.overview_levels(), 0, "an overview has no levels");
        assert_eq!(o.overview(0), None, "nor layouts of its own");
    }
    let coords: Vec<u32> = l.tiles().iter().map(|&t| d.u32() % t.max(1)).collect();
    let c = TileCoords::new(&coords).expect("at most GRID_FIXED_AXES_MAX");
    assert_eq!(o.pack(&c), l.pack(&c), "the same tile numbers");
    // a layout made apart from a definition may hold more cells a tile than a u64 counts; its overview fewer
    if let Some(cells) = o.tile_cells(&c) {
        assert!(cells >= 1, "an overview tile holds a cell");
        if let Some(whole) = l.tile_cells(&c) {
            assert!(
                cells <= whole,
                "an overview tile holds at most the tile's cells"
            );
        }
    }
}

/// The level of `l` for a pixel drawn from `d`: the highest whose cell fits the pixel along every axis `halved`
/// overviews halve; 0 for a pixel of another axis count.
fn pixel_laws(l: &TileLayout, halved: &[bool], d: &mut Draw<'_>) {
    let levels = l.overview_levels();
    let px: Vec<f64> = (0..l.axes())
        .map(|_| match d.u8() % 4 {
            0 => f64::NAN,
            1 => f64::from(d.u32()) / 1024.0,
            _ => f64::from(d.u8() % 40),
        })
        .collect();
    let fits =
        |cell: f64, p: f64| matches!(cell.partial_cmp(&p), Some(Ordering::Less | Ordering::Equal));
    let k = l.level_for(&px);
    assert!(k <= levels, "the level for a pixel is one of the grid's");
    let cell = f64::from(1u32 << k);
    for (p, h) in px.iter().zip(halved) {
        if *h && k > 0 {
            assert!(fits(cell, *p), "level {k}'s cell fits the pixel {p}");
        }
    }
    if k < levels {
        assert!(
            px.iter()
                .zip(halved)
                .any(|(p, h)| *h && !fits(2.0 * cell, *p)),
            "the next level's cell doesn't fit: {px:?} at {k} of {levels}"
        );
    }
    let mut short = px;
    short.push(1e9);
    assert_eq!(l.level_for(&short), 0, "a pixel of another axis count");
}

/// Overview chunk `c` of a grid laid out by `l`, cells of `shape`: its columns read as [Overview::of] finds them,
/// each holding the cells its level's tile holds over its time steps, decoded as the shape says.
pub(crate) fn overview(c: &Chunk<'_>, l: &TileLayout, shape: SampleShape) -> bool {
    let h = c.head();
    let Ok(o) = Overview::of(c, l, shape) else {
        return false;
    };
    assert_eq!(o.level(), h.level);
    let at = l
        .overview(h.level)
        .expect("an overview reads at a level of the grid");
    let tile = at
        .tile_cells(&h.tile)
        .expect("an overview reads of a tile of the grid");
    assert_eq!(
        u64::try_from(o.cells()).ok(),
        tile.checked_mul(u64::from(h.count))
    );
    for kind in [
        ColumnKind::OverviewMin,
        ColumnKind::OverviewMax,
        ColumnKind::OverviewMean,
    ] {
        let col = c
            .find(kind, 0)
            .expect("an overview that reads has its columns");
        super::hostile::decode_column(&col, shape, o.cells(), o.cells() / 2);
    }
    true
}

/// The chunks of grid `g` in the first piece of `r` at each overview level, in the box of the whole grid at that
/// level: each found tile is one the level's layout holds; its chunk, read, is of that level and tile, and an
/// overview chunk's columns read as [overview] checks. Returns the overview chunks whose columns read.
pub(crate) fn chunks<S: ByteSource>(r: &TapeReader<S>, g: &GridOf) -> usize
where
    S::Error: core::fmt::Debug,
{
    let mut read = 0;
    let mut buf = vec![];
    for k in 0..=g.layout.overview_levels() {
        let (Some(at), Some(whole)) = (g.layout.overview(k), g.whole(k)) else {
            continue;
        };
        let Ok(found) = r.grid_tiles(0, g.id, i64::MIN..i64::MAX, &whole) else {
            continue;
        };
        for t in &found {
            assert!(at.holds(&t.tile), "{:?} at level {k}", t.tile);
            assert_eq!(t.entry.level, k, "an entry of the box's level");
            let Ok(b) = r.read_block(0, &t.entry, &mut buf) else {
                continue;
            };
            let Some(c) = b.chunk() else {
                continue;
            };
            let h = c.head();
            if h.level == k
                && h.tile == t.tile
                && k > 0
                && let Some(shape) = g.shape
            {
                read += usize::from(overview(&c, &g.layout, shape));
            }
        }
    }
    read
}

/// `tape-grid`: grid definitions and tile layouts from any bytes, and the bytes as a tape; see the module docs.
pub fn grid(data: &[u8]) {
    tape(data);
    if let Ok(def) = GridDef::from_ww_bytes(data) {
        let l = def
            .tile_layout()
            .expect("a decoded grid's axes were checked");
        let extents: Vec<(u32, u32)> = def
            .axes
            .iter()
            .map(|a| a.map(|a| (a.extent.0, a.tile_extent.0)))
            .collect::<Result<_, _>>()
            .expect("a decoded grid's axes read");
        laws(&l, &extents, &mut Draw::new(data));
    }
    // [n][extent u32, tile u32] × n
    let Some((&n, rest)) = data.split_first() else {
        return;
    };
    let (pairs, after) = rest.as_chunks::<8>();
    let extents: Vec<(u32, u32)> = pairs
        .iter()
        .take(usize::from(n) % (GRID_FIXED_AXES_MAX + 4))
        .map(|p| {
            let [a, b] = [&p[..4], &p[4..]].map(|q| u32::from_le_bytes(q.try_into().expect("4")));
            (a, b)
        })
        .collect();
    match TileLayout::new(&extents) {
        Ok(l) => {
            assert!(holds(&extents), "{extents:?}");
            laws(&l, &extents, &mut Draw::new(after));
        }
        Err(_) => assert!(!holds(&extents), "{extents:?}"),
    }
}

/// The summary blocks of levels 0 and 1 of grid track `id` (layout `l`) in the first piece of `r` that lie in the box
/// `tiles`: each found tile is one the grid holds and the box meets, by entry tile number or by the block's head where
/// a scan left none; merged into a region's timeline, nodes in order, each holding a tile, a time step and a sample
/// per tile.
pub(crate) fn summaries<S: ByteSource>(
    r: &TapeReader<S>,
    id: TrackId,
    l: &TileLayout,
    tiles: &TileBox,
) where
    S::Error: core::fmt::Debug,
{
    for level in [0u8, 1] {
        if let Ok(found) = r.grid_summaries(0, id, i64::MIN..i64::MAX, tiles, level) {
            for t in &found {
                assert!(l.holds(&t.tile), "{:?}", t.tile);
                assert!(tiles.holds_coords(&t.tile), "{:?}", t.tile);
            }
        }
        if let Ok(nodes) =
            ww_tape_summary::region_timeline(r, 0, id, i64::MIN..i64::MAX, tiles, level)
        {
            for w in nodes.windows(2) {
                assert!(w[0].node < w[1].node, "nodes in order, once each");
            }
            for n in &nodes {
                assert!(n.tiles >= 1, "a node holds a tile");
                assert!(n.items >= 1, "a node holds a time step");
                assert!(n.samples >= u64::from(n.tiles), "a sample a tile at least");
            }
        }
    }
}

/// `data` read as a tape whose first block defines a grid track: its chunks at each level and its tile summaries in
/// the box of the whole grid.
fn tape(data: &[u8]) {
    let Ok(r) = TapeReader::open(data) else {
        return;
    };
    let first = ww_tape::block_len(data).ok().and_then(|n| data.get(..n));
    let Some(g) = first.and_then(super::hostile::grid_def) else {
        return;
    };
    if r.pieces().is_empty() {
        return;
    }
    chunks(&r, &g);
    if let Some(whole) = g.whole(0) {
        summaries(&r, g.id, &g.layout, &whole);
    }
}

/// How far a hostile grid file reaches into the grid readers.
///
/// Whether its grid definition decodes, the level-0 summary blocks found in the box of the whole grid, those of them
/// whose entry held no tile number (the head read), region nodes merged, and overview chunks found whose columns
/// read.
#[derive(Clone, Copy, Debug, Default)]
pub struct GridReach {
    pub defined: bool,
    pub found: usize,
    pub scanned: usize,
    pub nodes: usize,
    pub overviews: usize,
}

/// [GridReach] of `file`, a grid file [crate::hostile::grid] builds.
pub fn grid_reach(file: &[u8]) -> GridReach {
    let mut reach = GridReach::default();
    let first = ww_tape::block_len(file).ok().and_then(|n| file.get(..n));
    let Some(g) = first.and_then(super::hostile::grid_def) else {
        return reach;
    };
    reach.defined = true;
    let (Ok(r), Some(whole)) = (TapeReader::open(file), g.whole(0)) else {
        return reach;
    };
    if r.pieces().is_empty() {
        return reach;
    }
    if let Ok(found) = r.grid_summaries(0, g.id, i64::MIN..i64::MAX, &whole, 0) {
        reach.found = found.len();
        reach.scanned = found.iter().filter(|t| t.entry.tile.is_none()).count();
    }
    if let Ok(nodes) = ww_tape_summary::region_timeline(&r, 0, g.id, i64::MIN..i64::MAX, &whole, 0)
    {
        reach.nodes = nodes.len();
    }
    reach.overviews = chunks(&r, &g);
    reach
}
