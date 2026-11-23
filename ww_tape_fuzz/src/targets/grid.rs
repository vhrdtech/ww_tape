//! ww_tape grids on any bytes: a grid definition decodes without panic, and one that decodes has a tile layout (its
//! axes were checked on decode); tile layouts of extents from the input are made exactly when their axes hold, and
//! their tile numbers pack, unpack and test boxes alike by shift and mask and by coordinates. Read as a tape, a grid
//! track's tile summaries are found and merged into a region's timeline without panic ([summaries]).

use std::ops::Range;

use crate::hostile::Draw;
use shrink_wrap::prelude::*;
use ww_tape::grid::{GRID_FIXED_AXES_MAX, TILE_NUMBER_BITS};
use ww_tape::reader::ByteSource;
use ww_tape::{GridDef, TapeReader, TileBox, TileCoords, TileLayout, TrackId};

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

/// `data` read as a tape whose first block defines a grid track: its tile summaries in the box of the whole grid.
fn tape(data: &[u8]) {
    let Ok(r) = TapeReader::open(data) else {
        return;
    };
    let first = ww_tape::block_len(data).ok().and_then(|n| data.get(..n));
    let Some((id, l, extents)) = first.and_then(super::hostile::grid_def) else {
        return;
    };
    let cells: Vec<Range<u32>> = extents.iter().map(|e| 0..*e).collect();
    if !r.pieces().is_empty()
        && let Some(whole) = l.tile_box(&cells)
    {
        summaries(&r, id, &l, &whole);
    }
}

/// How far a hostile grid file reaches into the tile summaries' readers.
///
/// Whether its grid definition decodes, the level-0 summary blocks found in the box of the whole grid, those of them
/// whose entry held no tile number (the head read), and region nodes merged.
#[derive(Clone, Copy, Debug, Default)]
pub struct SummaryReach {
    pub defined: bool,
    pub found: usize,
    pub scanned: usize,
    pub nodes: usize,
}

/// [SummaryReach] of `file`, a grid file [crate::hostile::grid] builds.
pub fn summary_reach(file: &[u8]) -> SummaryReach {
    let mut reach = SummaryReach::default();
    let first = ww_tape::block_len(file).ok().and_then(|n| file.get(..n));
    let Some((id, l, extents)) = first.and_then(super::hostile::grid_def) else {
        return reach;
    };
    reach.defined = true;
    let cells: Vec<Range<u32>> = extents.iter().map(|e| 0..*e).collect();
    let (Ok(r), Some(whole)) = (TapeReader::open(file), l.tile_box(&cells)) else {
        return reach;
    };
    if r.pieces().is_empty() {
        return reach;
    }
    if let Ok(found) = r.grid_summaries(0, id, i64::MIN..i64::MAX, &whole, 0) {
        reach.found = found.len();
        reach.scanned = found.iter().filter(|t| t.entry.tile.is_none()).count();
    }
    if let Ok(nodes) = ww_tape_summary::region_timeline(&r, 0, id, i64::MIN..i64::MAX, &whole, 0) {
        reach.nodes = nodes.len();
    }
    reach
}
