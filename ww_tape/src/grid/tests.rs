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
fn edge_tiles_hold_the_cells_left_over() {
    let l = TileLayout::new(&weather()).unwrap();
    assert_eq!(l.cell_range(2, 0), Some(0..180));
    // latitude 721 in tiles of 180: the fifth tile holds 1 row
    assert_eq!(l.cell_range(2, 4), Some(720..721));
    assert_eq!(l.cell_range(2, 5), None);
    assert_eq!(l.cell_range(4, 0), None);
    let inner = TileCoords::new(&[7, 3, 2, 5]).unwrap();
    assert_eq!(l.tile_cells(&inner), Some(180 * 180));
    let edge = TileCoords::new(&[50, 36, 4, 7]).unwrap();
    assert_eq!(l.tile_cells(&edge), Some(180));
    assert_eq!(
        l.tile_cells(&TileCoords::new(&[51, 0, 0, 0]).unwrap()),
        None
    );
    assert_eq!(TileLayout::SERIES.tile_cells(&TileCoords::NONE), Some(1));
    // every tile's cells together are the grid's cells
    let small = TileLayout::new(&[(5, 2), (7, 3)]).unwrap();
    let mut all = 0;
    for a in 0..3 {
        for b in 0..3 {
            all += small
                .tile_cells(&TileCoords::new(&[a, b]).unwrap())
                .unwrap();
        }
    }
    assert_eq!(all, 35);
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

/// A series takes 0 or 1 byte for its empty list of axes, one nibble: the bytes of `SeriesDef` before fixed axes,
/// then those of the grid of no fixed axes.
#[test]
fn a_series_def_takes_0_or_1_byte_for_its_axes() {
    let series = grid(&[]);
    assert_eq!(series.tile_layout().unwrap(), TileLayout::SERIES);
    let hex = |v: &[u8]| {
        v.iter()
            .map(|b| std::format!("{b:02x}"))
            .collect::<std::string::String>()
    };
    // the nibble fills the last byte's free half
    let plain = GridDef::of::<i32>(1, Ratio64::new(1000, 1).unwrap());
    assert_eq!(hex(&encode(&plain)), "108768010000");
    // the drive's phase_a, with summaries: one byte more
    let summarized = GridDef {
        scale: Magnitude::pow10(-3),
        summaries: Some(crate::summary::SeriesSummarySpec {
            base_shift: 10,
            stats: crate::summary::SeriesStats {
                min: true,
                max: true,
                sum: true,
                sum_sq: true,
                ..crate::summary::SeriesStats::default()
            },
            clip: None,
        }),
        ..GridDef::of::<i16>(1, Ratio64::new(1_000_000, 25_000).unwrap())
    };
    let before = "102801fa400af00103";
    assert_eq!(hex(&encode(&summarized)), "102801fa400af0010003");
    assert_eq!(encode(&summarized).len(), before.len() / 2 + 1);
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

/// A thermal field of 1000 × 37 × 600 cells: x in tiles of 256, a listed band of 37 kept whole, y in tiles of 64.
fn field() -> TileLayout {
    TileLayout::new(&[(1000, 256), (37, 37), (600, 64)])
        .unwrap()
        .with_downsample(0b101)
}

/// A definition's layout marks its downsampled axes; levels go on while every downsampled tile extent halves evenly,
/// the axes not downsampled not looked at.
#[test]
fn overview_levels_stop_where_a_tile_extent_is_odd() {
    static AXES: [FixedAxis<'static>; 3] = [
        FixedAxis {
            name: "x",
            extent: UNib32(1000),
            tile_extent: UNib32(256),
            unit: None,
            scale: Magnitude::ONE,
            coords: AxisCoords::Regular { origin: 0, step: 1 },
            downsample: true,
        },
        FixedAxis {
            name: "band",
            extent: UNib32(37),
            tile_extent: UNib32(37),
            unit: None,
            scale: Magnitude::ONE,
            coords: AxisCoords::Regular { origin: 0, step: 1 },
            downsample: false,
        },
        FixedAxis {
            name: "y",
            extent: UNib32(600),
            tile_extent: UNib32(64),
            unit: None,
            scale: Magnitude::ONE,
            coords: AxisCoords::Regular { origin: 0, step: 1 },
            downsample: true,
        },
    ];
    let l = grid(&AXES).tile_layout().unwrap();
    assert_eq!(l, field());
    assert_eq!(l.downsample(), 0b101);
    assert_eq!(
        l.overview_levels(),
        6,
        "64 halves 6 times; 37 is not downsampled"
    );
    let forecast = TileLayout::new(&weather()).unwrap().with_downsample(0b1100);
    assert_eq!(forecast.overview_levels(), 2, "180 halves twice");
    assert_eq!(TileLayout::new(&weather()).unwrap().overview_levels(), 0);
    assert_eq!(TileLayout::SERIES.overview_levels(), 0);
    assert_eq!(TileLayout::SERIES.with_downsample(u32::MAX).downsample(), 0);
    let big = TileLayout::new(&[(u32::MAX, 1 << 31)])
        .unwrap()
        .with_downsample(1);
    assert_eq!(
        big.overview_levels(),
        GRID_OVERVIEW_LEVELS_MAX,
        "31 halvings, capped"
    );
}

/// Level k halves each downsampled axis' extent (rounded up) and tile extent k times and keeps the tiles: the same
/// coordinates and tile numbers, edge tiles holding the cells left over.
#[test]
fn an_overview_layout_halves_the_downsampled_axes() {
    let l = field();
    assert_eq!(l.overview(0), Some(l));
    let o = l.overview(3).unwrap();
    assert_eq!(o.level(), 3);
    assert_eq!(o.extents(), [(125, 32), (37, 37), (75, 8)]);
    assert_eq!(o.tiles(), l.tiles());
    assert_eq!(o.widths(), l.widths());
    let edge = TileCoords::new(&[3, 0, 9]).unwrap();
    assert_eq!(o.pack(&edge), l.pack(&edge));
    // x: 1000 - 768 = 232 cells as written, 29 at level 3; y: 600 - 576 = 24, 3 at level 3
    assert_eq!(l.tile_cells(&edge), Some(232 * 37 * 24));
    assert_eq!(o.tile_cells(&edge), Some(29 * 37 * 3));
    // an odd edge: 1001 cells leave 233 in the last x tile, 30 cells at level 3 (the last covering 1)
    let odd = TileLayout::new(&[(1001, 256)]).unwrap().with_downsample(1);
    let last = TileCoords::new(&[3]).unwrap();
    assert_eq!(odd.overview(3).unwrap().tile_cells(&last), Some(30));
    assert_eq!(odd.overview(8).unwrap().extents(), [(4, 1)]);
    assert_eq!(l.overview(7), None, "past the levels");
    assert_eq!(o.overview(1), None, "an overview's own layout");
    assert_eq!(o.overview_levels(), 0);
}

/// A view picks the highest level whose cell is at most a pixel along every downsampled axis.
#[test]
fn level_for_picks_the_coarsest_cell_within_a_pixel() {
    let l = field();
    assert_eq!(l.level_for(&[1.0, 1.0, 1.0]), 0);
    assert_eq!(l.level_for(&[4.0, 1.0, 4.0]), 2);
    assert_eq!(l.level_for(&[7.9, 99.0, 100.0]), 2, "x decides");
    assert_eq!(l.level_for(&[8.0, 1.0, 8.0]), 3);
    assert_eq!(l.level_for(&[1e9, 1.0, 1e9]), 6, "capped at the levels");
    assert_eq!(l.level_for(&[f64::NAN, 1.0, 8.0]), 0);
    assert_eq!(l.level_for(&[8.0, 8.0]), 0, "one value per fixed axis");
    assert_eq!(TileLayout::SERIES.level_for(&[]), 0);
}

/// An overview chunk of a 10 × 6 grid in tiles of 4 × 2 at `level` over `tile`: three steps, raw i16 columns of
/// `cells` values each, the min column `short` values shorter.
fn overview_chunk(level: u8, tile: &[u32], cells: usize, short: usize) -> std::vec::Vec<u8> {
    use crate::chunk::{ColumnSpec, chunk_len, write_chunk};
    let head = crate::ChunkHead {
        kind: crate::BlockKind::Series,
        track_id: crate::TrackId::new(9),
        track_number: crate::TrackNum::new(1),
        first_item: crate::ItemNum::new(0),
        count: 3,
        first_tick: 0,
        last_tick: 2,
        latest_end: 2,
        time_mode: 0,
        end_mode: crate::chunk::EndMode::Absent,
        grid: Some(crate::SeriesGrid::start(0, 0, Magnitude::ONE)),
        tile: TileCoords::new(tile).unwrap(),
        level,
    };
    let col = |base: i16, n: usize| -> std::vec::Vec<u8> {
        (0..n)
            .flat_map(|i| (base + i as i16).to_le_bytes())
            .collect()
    };
    let cols = [
        (crate::ColumnKind::OverviewMin, col(0, cells - short)),
        (crate::ColumnKind::OverviewMax, col(100, cells)),
        (crate::ColumnKind::OverviewMean, col(50, cells)),
    ];
    let cols: std::vec::Vec<(ColumnSpec, &[u8])> = cols
        .iter()
        .map(|(kind, b)| {
            let spec = ColumnSpec {
                kind: *kind,
                codec: crate::Codec::RAW,
                param: 0,
                len: b.len(),
            };
            (spec, &b[..])
        })
        .collect();
    let specs: std::vec::Vec<ColumnSpec> = cols.iter().map(|c| c.0).collect();
    let mut buf = std::vec![0u8; chunk_len(&head, &specs, 0)];
    write_chunk(&mut buf, crate::tests::id(1), &head, &cols, 0).unwrap();
    buf
}

/// An overview chunk's columns read as the tile's cells at its level times its steps: 2 × 1 cells in tile (0, 0) at
/// level 1, one in the edge tile (2, 2). A chunk as written, a level past the grid's, a tile outside it and a short
/// column are refused.
#[test]
fn an_overview_reads_its_cells_from_the_level_layout() {
    use crate::samples::{Element, ElementDesc, SampleShape};
    let layout = TileLayout::new(&[(10, 4), (6, 2)])
        .unwrap()
        .with_downsample(0b11);
    assert_eq!(layout.overview_levels(), 1);
    let shape = SampleShape::new(ElementDesc::of(Element::I16), 1);
    let read = |bytes: &[u8]| {
        let b = crate::Block::read(bytes).unwrap();
        let c = b.chunk().unwrap();
        Overview::of(&c, &layout, shape).map(|o| {
            let mut min = std::vec![0i16; o.cells()];
            o.min().decode(0, &mut min).unwrap();
            let mut mean = std::vec![0i16; o.cells()];
            o.mean().decode(0, &mut mean).unwrap();
            (o.level(), o.cells(), min, mean)
        })
    };
    let (level, cells, min, mean) = read(&overview_chunk(1, &[0, 0], 6, 0)).unwrap();
    assert_eq!((level, cells), (1, 6));
    assert_eq!(min, [0, 1, 2, 3, 4, 5]);
    assert_eq!(mean, [50, 51, 52, 53, 54, 55]);
    assert_eq!(read(&overview_chunk(1, &[2, 2], 3, 0)).unwrap().1, 3);
    assert!(matches!(
        read(&overview_chunk(1, &[0, 0], 6, 1)),
        Err(OverviewError::Samples(crate::ColumnKind::OverviewMin, _))
    ));
    assert_eq!(
        read(&overview_chunk(2, &[0, 0], 6, 0)).unwrap_err(),
        OverviewError::NotOverview
    );
    assert_eq!(
        read(&overview_chunk(1, &[3, 0], 6, 0)).unwrap_err(),
        OverviewError::NotOverview
    );
}
