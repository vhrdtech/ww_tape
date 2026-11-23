use exact_units::Fixed;
use exact_units::prefix::Milli;
use exact_units::units::Ampere;
use shrink_wrap::prelude::*;

use super::*;
use crate::{GridDef, Ratio64};

fn encode<T: SerializeShrinkWrap>(v: &T) -> std::vec::Vec<u8> {
    let mut buf = [0u8; 4096];
    v.to_ww_bytes(&mut buf).unwrap().to_vec()
}

const LEVELS: [i64; 3] = [1000, 925, 850];

/// A regular axis of `extent` cells in tiles of `tile`, 1 per cell from 0.
fn axis(name: &'static str, extent: u32, tile: u32) -> FixedAxis<'static> {
    FixedAxis {
        name,
        extent: UNib32(extent),
        tile_extent: UNib32(tile),
        unit: None,
        scale: Magnitude::ONE,
        coords: AxisCoords::Regular { origin: 0, step: 1 },
        downsample: true,
    }
}

/// The weather forecast of the design: member 51 in tiles of 1, pressure level 37 listed, latitude 721 in tiles of
/// 180 and longitude 1440 in tiles of 180.
fn weather() -> [(u32, u32); 4] {
    [(51, 1), (37, 1), (721, 180), (1440, 180)]
}

fn grid(axes: &'static [FixedAxis<'static>]) -> GridDef<'static> {
    GridDef {
        axes: RefVec::Slice { slice: axes },
        ..GridDef::of::<Fixed<Ampere, Milli, i32>>(1, Ratio64::new(1000, 1).unwrap())
    }
}

#[test]
fn the_weather_layout_packs_its_tiles_in_18_bits() {
    let l = TileLayout::new(&weather()).unwrap();
    assert_eq!(l.tiles(), [51, 37, 5, 8]);
    assert_eq!(l.widths(), [6, 6, 3, 3]);
    assert_eq!(l.bits(), 18);
    let t = TileCoords::new(&[7, 3, 2, 5]).unwrap();
    let n = l.pack(&t).unwrap();
    assert_eq!(n, 7 << 12 | 3 << 6 | 2 << 3 | 5);
    assert_eq!(l.unpack(n), Some(t));
    for (axis, c) in [7, 3, 2, 5].into_iter().enumerate() {
        assert_eq!(l.coord(n, axis), Some(c));
    }
    assert_eq!(l.coord(n, 4), None);
}

#[test]
fn a_series_layout_has_no_fields_and_tile_number_0() {
    let l = TileLayout::new(&[]).unwrap();
    assert_eq!(l, TileLayout::SERIES);
    assert_eq!(l.pack(&TileCoords::NONE), Some(0));
    assert_eq!(l.unpack(0), Some(TileCoords::NONE));
    assert_eq!(l.unpack(1), None, "a bit outside the fields");
    let b = l.tile_box(&[]).unwrap();
    assert!(!b.is_empty());
    assert!(b.holds(0));
}

#[test]
fn tile_numbers_outside_the_grid_are_refused() {
    let l = TileLayout::new(&weather()).unwrap();
    assert_eq!(l.pack(&TileCoords::new(&[51, 0, 0, 0]).unwrap()), None);
    assert_eq!(l.pack(&TileCoords::new(&[0, 0, 0]).unwrap()), None);
    // longitude 5 bits hold 0..8: field value 6 of latitude is past its 5 tiles
    assert_eq!(l.unpack(6 << 3), None);
    assert_eq!(l.unpack(1 << 18), None, "a bit above the fields");
    assert_eq!(l.unpack(u128::MAX), None);
}

#[test]
fn widths_are_ceil_log2_of_the_tiles() {
    for (tiles, width) in [
        (1, 0),
        (2, 1),
        (3, 2),
        (4, 2),
        (5, 3),
        (8, 3),
        (9, 4),
        (u32::MAX, 32),
    ] {
        let l = TileLayout::new(&[(tiles, 1)]).unwrap();
        assert_eq!(l.widths(), [width], "{tiles} tiles");
        let last = TileCoords::new(&[tiles - 1]).unwrap();
        assert_eq!(l.unpack(l.pack(&last).unwrap()), Some(last));
    }
}

#[test]
fn fields_over_128_bits_are_refused_and_128_taken() {
    // 4 axes of 2^32 tiles: 128 bits
    let full = [(u32::MAX, 1); 4];
    let l = TileLayout::new(&full).unwrap();
    assert_eq!(l.bits(), 128);
    let top = TileCoords::new(&[u32::MAX - 1; 4]).unwrap();
    assert_eq!(l.unpack(l.pack(&top).unwrap()), Some(top));
    assert!(TileLayout::new(&[(u32::MAX, 1); 5]).is_err());
    // 20 axes of 10 values: 80 bits, past a u64 of tiles
    let sweep = TileLayout::new(&[(10, 1); 20]).unwrap();
    assert_eq!(sweep.bits(), 80);
    // 31 axes of 2 tiles fit; 32 fixed axes don't
    assert_eq!(TileLayout::new(&[(2, 1); 31]).unwrap().bits(), 31);
    assert!(TileLayout::new(&[(2, 1); 32]).is_err());
}

#[test]
fn a_one_tile_axis_packs_beside_128_bits() {
    // an axis of one tile takes no bits, its field at shift 128 above the others
    let one = [
        (1, 1),
        (u32::MAX, 1),
        (u32::MAX, 1),
        (u32::MAX, 1),
        (u32::MAX, 1),
    ];
    let l = TileLayout::new(&one).unwrap();
    assert_eq!(l.bits(), 128);
    for c in [[0, 0, 0, 0, 0], [0, u32::MAX - 1, 7, 0, u32::MAX - 1]] {
        let c = TileCoords::new(&c).unwrap();
        assert!(l.holds(&c));
        assert_eq!(l.unpack(l.pack(&c).unwrap()), Some(c));
    }
}

#[test]
fn a_box_turns_into_tile_ranges_clipped_to_the_extent() {
    let l = TileLayout::new(&weather()).unwrap();
    let b = l.tile_box(&[7..8, 0..37, 179..361, 1000..5000]).unwrap();
    assert_eq!(b.ranges(), [(7, 8), (0, 37), (0, 3), (5, 8)]);
    let inside = l.pack(&TileCoords::new(&[7, 30, 2, 7]).unwrap()).unwrap();
    let outside = l.pack(&TileCoords::new(&[7, 30, 3, 7]).unwrap()).unwrap();
    assert!(b.holds(inside));
    assert!(!b.holds(outside));
    let empty = l.tile_box(&[7..7, 0..37, 0..721, 0..1440]).unwrap();
    assert!(empty.is_empty());
    let past = l.tile_box(&[60..70, 0..37, 0..721, 0..1440]).unwrap();
    assert!(past.is_empty(), "a box past the extent meets no tile");
    assert_eq!(
        l.tile_box(&[0..1, 0..1, 0..1]),
        None,
        "one range per fixed axis"
    );
}

static WEATHER_AXES: [FixedAxis<'static>; 4] = [
    FixedAxis {
        name: "member",
        extent: UNib32(51),
        tile_extent: UNib32(1),
        unit: None,
        scale: Magnitude::ONE,
        coords: AxisCoords::Regular { origin: 0, step: 1 },
        downsample: false,
    },
    FixedAxis {
        name: "level",
        extent: UNib32(3),
        tile_extent: UNib32(1),
        unit: None,
        scale: Magnitude::ONE,
        coords: AxisCoords::Listed(RefVec::Slice { slice: &LEVELS }),
        downsample: false,
    },
    FixedAxis {
        name: "latitude",
        extent: UNib32(721),
        tile_extent: UNib32(180),
        unit: None,
        scale: Magnitude::pow10(-3),
        coords: AxisCoords::Regular {
            origin: 90_000,
            step: -250,
        },
        downsample: true,
    },
    FixedAxis {
        name: "longitude",
        extent: UNib32(1440),
        tile_extent: UNib32(180),
        unit: None,
        scale: Magnitude::pow10(-3),
        coords: AxisCoords::Regular {
            origin: 0,
            step: 250,
        },
        downsample: true,
    },
];

#[test]
fn a_grid_def_reads_back_with_its_axes_and_layout() {
    let def = grid(&WEATHER_AXES);
    def.check().unwrap();
    let bytes = encode(&def);
    let back = GridDef::from_ww_bytes(&bytes).unwrap();
    assert_eq!(back, def);
    let l = back.tile_layout().unwrap();
    assert_eq!(l.tiles(), [51, 3, 5, 8]);
    assert_eq!(l.widths(), [6, 2, 3, 3]);
    #[cfg(feature = "std")]
    {
        let owned = crate::GridDefOwned::from_ww_bytes_owned(&bytes).unwrap();
        assert_eq!(owned.axes.len(), 4);
    }
}

#[test]
fn a_series_def_takes_one_byte_for_its_axes() {
    let series = grid(&[]);
    assert_eq!(series.tile_layout().unwrap(), TileLayout::SERIES);
    let one = grid(core::slice::from_ref(&WEATHER_AXES[0]));
    assert!(encode(&one).len() > encode(&series).len());
}

static BAD_LISTED: [[i64; 3]; 3] = [[1000, 1000, 850], [1000, 925, 950], [1, 2, 3]];

#[test]
fn bad_axes_are_refused_by_check_and_on_decode() {
    let base = || WEATHER_AXES[1].clone();
    let listed = |v: &'static [i64]| AxisCoords::Listed(RefVec::Slice { slice: v });
    let bad: std::vec::Vec<FixedAxis<'static>> = std::vec![
        FixedAxis { name: "", ..base() },
        FixedAxis {
            name: "a_name_of_sixty_five_bytes_is_one_more_than_the_cap_allows_ok____",
            ..base()
        },
        FixedAxis {
            extent: UNib32(0),
            ..base()
        },
        FixedAxis {
            tile_extent: UNib32(0),
            ..base()
        },
        FixedAxis {
            tile_extent: UNib32(4),
            ..base()
        },
        // a repeated value, a turn, a count not the extent
        FixedAxis {
            coords: listed(&BAD_LISTED[0]),
            ..base()
        },
        FixedAxis {
            coords: listed(&BAD_LISTED[1]),
            ..base()
        },
        FixedAxis {
            extent: UNib32(4),
            tile_extent: UNib32(1),
            coords: listed(&BAD_LISTED[2]),
            ..base()
        },
        FixedAxis {
            coords: AxisCoords::Regular { origin: 0, step: 0 },
            ..base()
        },
        // one step of the tile past GRID_TILE_CELLS_MAX
        FixedAxis {
            extent: UNib32(u32::MAX),
            tile_extent: UNib32(u32::MAX),
            coords: AxisCoords::Regular { origin: 0, step: 1 },
            ..base()
        },
    ];
    assert_eq!(bad[1].name.len(), AXIS_NAME_LEN_MAX + 1);
    for a in bad {
        let axes: &'static [FixedAxis<'static>] = std::vec![a.clone()].leak();
        let def = grid(axes);
        assert!(def.check().is_err(), "{a:?}");
        assert!(GridDef::from_ww_bytes(&encode(&def)).is_err(), "{a:?}");
    }
    let too_many: &'static [FixedAxis<'static>] =
        std::vec![axis("x", 2, 1); GRID_FIXED_AXES_MAX + 1].leak();
    assert!(grid(too_many).check().is_err());
    assert!(GridDef::from_ww_bytes(&encode(&grid(too_many))).is_err());
    let most: &'static [FixedAxis<'static>] =
        std::vec![axis("x", 2, 1); GRID_FIXED_AXES_MAX].leak();
    grid(most).check().unwrap();
    // 5 axes of 2^32 tiles: 160 bits
    let wide: &'static [FixedAxis<'static>] = std::vec![axis("x", u32::MAX, 1); 5].leak();
    assert!(grid(wide).check().is_err());
}

#[test]
fn a_listed_axis_of_more_than_grid_listed_max_cells_is_refused() {
    let values: &'static [i64] = (0..=i64::from(GRID_LISTED_MAX))
        .collect::<std::vec::Vec<_>>()
        .leak();
    let a = FixedAxis {
        extent: UNib32(GRID_LISTED_MAX + 1),
        tile_extent: UNib32(1),
        coords: AxisCoords::Listed(RefVec::Slice { slice: values }),
        ..axis("p", 1, 1)
    };
    assert!(grid(std::vec![a.clone()].leak()).check().is_err());
    let a = FixedAxis {
        extent: UNib32(GRID_LISTED_MAX),
        coords: AxisCoords::Listed(RefVec::Slice {
            slice: &values[..values.len() - 1],
        }),
        ..a
    };
    grid(std::vec![a].leak()).check().unwrap();
}
