//! ww_tape grids on any bytes: a grid definition decodes without panic, and one that decodes has a tile layout (its
//! axes were checked on decode); tile layouts of extents from the input are made exactly when their axes hold, and
//! their tile numbers pack, unpack and test boxes alike by shift and mask and by coordinates.

use std::ops::Range;

use crate::hostile::Draw;
use shrink_wrap::prelude::*;
use ww_tape::grid::{GRID_FIXED_AXES_MAX, TILE_NUMBER_BITS};
use ww_tape::{GridDef, TileCoords, TileLayout};

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

/// `tape-grid`: grid definitions and tile layouts from any bytes; see the module docs.
pub fn grid(data: &[u8]) {
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
