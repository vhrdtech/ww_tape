//! Grids: a series track with fixed axes beside time ([GridDef](crate::GridDef)), its tiles and where the index
//! finds them.
//!
//! Axis 0 of a grid is time, the append axis a series has. A grid may add up to [GRID_FIXED_AXES_MAX] fixed axes
//! ([FixedAxis]), each of a known extent in cells: the cells of a thermal field, the pixels of an image, the members
//! of an ensemble. A series is the grid of no fixed axes, and its bytes are those of a series before grids.
//!
//! # Tiles
//!
//! A chunk of a grid holds one **tile**: a run of time steps along time and one box of cells along the fixed axes.
//! Each fixed axis is cut into tiles of its tile extent; the last tile of an axis holds the cells left over. A tile is
//! named by its **tile coordinates** ([TileCoords]), one per fixed axis: its first cell divided by the axis' tile
//! extent. Inside a tile the cells run time outermost, then the fixed axes in definition order, row-major, so one
//! time step of a tile is a contiguous slab and a series' samples are in the order they always were.
//!
//! A chunk head carries its tile coordinates ([ChunkHead::tile](crate::ChunkHead::tile)); the index entry of the
//! chunk carries them packed into a **tile number** ([TileLayout]): a u128 of bit fields, ⌈log2(tile count)⌉ bits per
//! fixed axis, the last axis in the lowest bits. The widths come from the definition and are never stored; together
//! they are at most [TILE_NUMBER_BITS]. A box query ([TapeReader::grid_tiles](crate::TapeReader::grid_tiles)) finds a
//! box's tiles from the index alone, unpacking each entry's coordinates by shift and mask.
//!
//! # Coordinates
//!
//! A fixed axis says where its cells are, as numbers of an exact_units type ([FixedAxis::unit], [FixedAxis::scale],
//! as a series' samples): regular, an origin and a step ([AxisCoords::Regular]), or listed, one value per cell
//! ([AxisCoords::Listed]): pressure levels, ensemble members, the values of a sweep. A fixed axis has no clock; time
//! stays the one axis with a clock.

#[cfg(test)]
mod tests;

use core::ops::Range;

use shrink_wrap::prelude::*;

#[cfg(feature = "std")]
use crate::TypeRefOwned;
use crate::{MAX_BLOCK_LEN, Magnitude, SeriesError, TypeRef};

/// Most axes of a grid, time included.
pub const GRID_AXES_MAX: usize = 32;
/// Most fixed axes of a grid: all but time.
pub const GRID_FIXED_AXES_MAX: usize = GRID_AXES_MAX - 1;
/// Most cells of an axis with listed coordinates: the values a definition carries for it.
pub const GRID_LISTED_MAX: u32 = 1 << 16;
/// Most bytes of a fixed axis' name.
pub const AXIS_NAME_LEN_MAX: usize = 64;
/// Bits of a tile number: the bit fields of all fixed axes together take at most this many.
pub const TILE_NUMBER_BITS: u32 = u128::BITS;
/// Most cells of one time step of a tile: the product of the tile extents of the fixed axes. One step's cells must
/// fit a block.
#[expect(
    clippy::as_conversions,
    reason = "usize is at most 64 bits (chunk's const assert); From is not const"
)]
pub const GRID_TILE_CELLS_MAX: u64 = MAX_BLOCK_LEN as u64;

/// A fixed axis of a grid: its name, extent, tiles and coordinates; see the [module docs](self).
#[derive_shrink_wrap(owned(feature = "std"), derive(Debug, PartialEq, Eq, Clone))]
pub struct FixedAxis<'i> {
    /// The axis' name (`x`, `latitude`, `member`): 1 to [AXIS_NAME_LEN_MAX] bytes.
    pub name: &'i str,
    /// Cells along the axis, 1 and up.
    pub extent: UNib32,
    /// Cells of the axis one tile holds, 1 to the extent.
    pub tile_extent: UNib32,
    /// The exact_units type of a coordinate, its unit and scale; None: a plain number.
    pub unit: Option<TypeRef<'i>>,
    /// What a coordinate number is worth in the unit.
    pub scale: Magnitude,
    /// Where the cells are along the axis.
    pub coords: AxisCoords<'i>,
    /// Space overviews halve this axis (x, y, z); not an ensemble member or a pressure level.
    pub downsample: bool,
}

/// Where the cells of a fixed axis are, as numbers of its unit at its scale.
#[derive_shrink_wrap(owned(feature = "std"), derive(Debug, PartialEq, Eq, Clone), ww_repr = u4)]
pub enum AxisCoords<'i> {
    /// Cell `i` is at `origin + i · step`; the step is not 0.
    Regular { origin: i64, step: i64 },
    /// One coordinate per cell, as many as the extent and at most [GRID_LISTED_MAX], strictly increasing or strictly
    /// decreasing.
    Listed(RefVec<'i, i64>),
}

// A kind of coordinates this build doesn't know reads as `Unknown`: the axis' tiles still place its chunks.

/// The parts of a fixed axis its check looks at, borrowed or owned.
pub(crate) trait AxisParts {
    fn name_len(&self) -> usize;
    /// The extent and the tile extent.
    fn extents(&self) -> (u32, u32);
    /// Whether the coordinates are valid for the axis' extent ([check_coords]).
    fn coords_valid(&self) -> bool;
}

impl AxisParts for FixedAxis<'_> {
    fn name_len(&self) -> usize {
        self.name.len()
    }

    fn extents(&self) -> (u32, u32) {
        (self.extent.0, self.tile_extent.0)
    }

    fn coords_valid(&self) -> bool {
        match &self.coords {
            AxisCoords::Regular { step, .. } => *step != 0,
            AxisCoords::Listed(v) => listed_valid(self.extent.0, v.len(), v.iter().map(Result::ok)),
            AxisCoords::Unknown(_) => true,
        }
    }
}

#[cfg(feature = "std")]
impl AxisParts for FixedAxisOwned {
    fn name_len(&self) -> usize {
        self.name.len()
    }

    fn extents(&self) -> (u32, u32) {
        (self.extent.0, self.tile_extent.0)
    }

    fn coords_valid(&self) -> bool {
        match &self.coords {
            AxisCoordsOwned::Regular { step, .. } => *step != 0,
            AxisCoordsOwned::Listed(v) => {
                listed_valid(self.extent.0, v.len(), v.iter().copied().map(Some))
            }
            AxisCoordsOwned::Unknown(_) => true,
        }
    }
}

/// Whether `len` listed values, read by `values` (None: one that doesn't read), are one per cell of an axis of
/// `extent` cells, at most [GRID_LISTED_MAX], and strictly increasing or strictly decreasing.
fn listed_valid(extent: u32, len: usize, values: impl Iterator<Item = Option<i64>>) -> bool {
    if extent > GRID_LISTED_MAX || u32::try_from(len) != Ok(extent) {
        return false;
    }
    let mut prev: Option<i64> = None;
    let mut order = None;
    for v in values {
        let Some(v) = v else { return false };
        if let Some(p) = prev {
            let o = v.cmp(&p);
            if o == core::cmp::Ordering::Equal || order.is_some_and(|d| d != o) {
                return false;
            }
            order = Some(o);
        }
        prev = Some(v);
    }
    true
}

/// Check a grid's fixed axes as a decoded definition is checked: at most [GRID_FIXED_AXES_MAX]; each named in 1 to
/// [AXIS_NAME_LEN_MAX] bytes, of an extent of 1 and up, a tile extent of 1 to the extent and valid coordinates; one
/// time step of a tile at most [GRID_TILE_CELLS_MAX] cells; the tile number's fields at most [TILE_NUMBER_BITS].
pub(crate) fn check_axes<'a, A: AxisParts + 'a>(
    axes: impl Iterator<Item = &'a A>,
) -> Result<TileLayout, SeriesError> {
    use SeriesError::Def;
    let mut tile_cells = 1u64;
    let mut extents = [(0u32, 0u32); GRID_FIXED_AXES_MAX];
    let mut n = 0usize;
    for a in axes {
        let slot = extents
            .get_mut(n)
            .ok_or(Def("more fixed axes than GRID_FIXED_AXES_MAX"))?;
        if a.name_len() == 0 || a.name_len() > AXIS_NAME_LEN_MAX {
            return Err(Def(
                "an axis name of no bytes or more than AXIS_NAME_LEN_MAX",
            ));
        }
        let (extent, tile) = a.extents();
        if extent == 0 || tile == 0 || tile > extent {
            return Err(Def(
                "an axis extent of 0, or a tile extent not 1 to the extent",
            ));
        }
        if !a.coords_valid() {
            return Err(Def(
                "a step of 0, or listed coordinates not one per cell, monotone and at most GRID_LISTED_MAX",
            ));
        }
        tile_cells = tile_cells
            .checked_mul(u64::from(tile))
            .filter(|c| *c <= GRID_TILE_CELLS_MAX)
            .ok_or(Def(
                "a tile's cells of one time step over GRID_TILE_CELLS_MAX",
            ))?;
        *slot = (extent, tile);
        n = n.saturating_add(1);
    }
    TileLayout::new(extents.get(..n).unwrap_or(&[]))
}

/// The tile coordinates of a chunk: one per fixed axis of its grid, none for a series.
#[derive(Copy, Clone, PartialEq, Eq, Hash)]
pub struct TileCoords {
    len: u8,
    coords: [u32; GRID_FIXED_AXES_MAX],
}

impl TileCoords {
    /// The coordinates of a series' chunk: none.
    pub const NONE: Self = Self {
        len: 0,
        coords: [0; GRID_FIXED_AXES_MAX],
    };

    /// The coordinates `coords`, one per fixed axis; None for more than [GRID_FIXED_AXES_MAX].
    pub fn new(coords: &[u32]) -> Option<Self> {
        let len = u8::try_from(coords.len())
            .ok()
            .filter(|n| usize::from(*n) <= GRID_FIXED_AXES_MAX)?;
        let mut out = Self { len, ..Self::NONE };
        for (d, s) in out.coords.iter_mut().zip(coords) {
            *d = *s;
        }
        Some(out)
    }

    /// The coordinates, one per fixed axis.
    pub fn as_slice(&self) -> &[u32] {
        self.coords
            .get(..usize::from(self.len))
            .unwrap_or(&self.coords)
    }

    /// Fixed axes the coordinates name, 0 for a series.
    pub fn len(&self) -> usize {
        usize::from(self.len)
    }

    /// Whether these are a series chunk's: no fixed axes.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Default for TileCoords {
    fn default() -> Self {
        Self::NONE
    }
}

impl core::fmt::Debug for TileCoords {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_list().entries(self.as_slice()).finish()
    }
}

/// The tiles of a grid's fixed axes and the bit fields of its tile number; see the [module docs](self#tiles).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TileLayout {
    axes: u8,
    /// Tiles along each axis.
    tiles: [u32; GRID_FIXED_AXES_MAX],
    /// Cells of each axis, and of its tiles.
    extents: [(u32, u32); GRID_FIXED_AXES_MAX],
    /// Bits of each axis' field, ⌈log2(tiles)⌉.
    widths: [u8; GRID_FIXED_AXES_MAX],
    /// Where each axis' field starts in the number: the widths of the axes after it.
    shifts: [u8; GRID_FIXED_AXES_MAX],
}

/// Bits a field of `tiles` values takes: ⌈log2(tiles)⌉, 0 for one tile.
const fn field_width(tiles: u32) -> u32 {
    if tiles <= 1 {
        0
    } else {
        u32::BITS.saturating_sub(tiles.saturating_sub(1).leading_zeros())
    }
}

impl TileLayout {
    /// The layout of a series: no fixed axes, tile number 0.
    pub const SERIES: Self = Self {
        axes: 0,
        tiles: [0; GRID_FIXED_AXES_MAX],
        extents: [(0, 0); GRID_FIXED_AXES_MAX],
        widths: [0; GRID_FIXED_AXES_MAX],
        shifts: [0; GRID_FIXED_AXES_MAX],
    };

    /// The layout of fixed axes of `(extent, tile extent)` each, in definition order; refused past
    /// [GRID_FIXED_AXES_MAX] axes, with an extent or tile extent out of range, or fields over [TILE_NUMBER_BITS].
    pub fn new(axes: &[(u32, u32)]) -> Result<Self, SeriesError> {
        use SeriesError::Def;
        let mut out = Self::SERIES;
        out.axes = u8::try_from(axes.len())
            .ok()
            .filter(|n| usize::from(*n) <= GRID_FIXED_AXES_MAX)
            .ok_or(Def("more fixed axes than GRID_FIXED_AXES_MAX"))?;
        let mut bits = 0u32;
        // last axis lowest: walk from the last, each field above the ones after it
        for (i, &(extent, tile)) in axes.iter().enumerate().rev() {
            if extent == 0 || tile == 0 || tile > extent {
                return Err(Def(
                    "an axis extent of 0, or a tile extent not 1 to the extent",
                ));
            }
            let tiles = extent.div_ceil(tile);
            let width = field_width(tiles);
            let over = Def("tile number fields over 128 bits");
            // bits stays at most 128 and a width at most 32: both fit a u8
            let (Ok(shift), Ok(w)) = (u8::try_from(bits), u8::try_from(width)) else {
                return Err(over);
            };
            bits = bits
                .checked_add(width)
                .filter(|b| *b <= TILE_NUMBER_BITS)
                .ok_or(over)?;
            set(&mut out.tiles, i, tiles);
            set(&mut out.extents, i, (extent, tile));
            set(&mut out.widths, i, w);
            set(&mut out.shifts, i, shift);
        }
        Ok(out)
    }

    /// Fixed axes, 0 for a series.
    pub fn axes(&self) -> usize {
        usize::from(self.axes)
    }

    /// Tiles along each fixed axis: the extent over the tile extent, rounded up.
    pub fn tiles(&self) -> &[u32] {
        self.tiles.get(..self.axes()).unwrap_or(&[])
    }

    /// Cells of each fixed axis and of its tiles, `(extent, tile extent)`, in definition order.
    pub fn extents(&self) -> &[(u32, u32)] {
        self.extents.get(..self.axes()).unwrap_or(&[])
    }

    /// The cells of fixed axis `axis` that its tile `coord` holds: a tile extent of them, fewer in the axis' last
    /// tile. None past the fixed axes or the axis' tiles.
    pub fn cell_range(&self, axis: usize, coord: u32) -> Option<Range<u32>> {
        let &(extent, tile) = self.extents().get(axis)?;
        let start = coord.checked_mul(tile).filter(|s| *s < extent)?;
        Some(start..start.saturating_add(tile).min(extent))
    }

    /// Cells of one time step of the tile at `coords`: the product of its cells along each fixed axis, fewer for a
    /// tile at an axis' far edge; 1 for a series. None when the grid holds no such tile ([Self::holds]).
    pub fn tile_cells(&self, coords: &TileCoords) -> Option<u64> {
        if !self.holds(coords) {
            return None;
        }
        let mut cells = 1u64;
        for (axis, c) in coords.as_slice().iter().enumerate() {
            let r = self.cell_range(axis, *c)?;
            cells = cells.checked_mul(u64::from(r.end.checked_sub(r.start)?))?;
        }
        Some(cells)
    }

    /// Bits of each axis' field in the tile number.
    pub fn widths(&self) -> &[u8] {
        self.widths.get(..self.axes()).unwrap_or(&[])
    }

    /// Bits the fields take together, at most [TILE_NUMBER_BITS].
    pub fn bits(&self) -> u32 {
        self.widths().iter().map(|w| u32::from(*w)).sum()
    }

    /// Whether `coords` name a tile of the grid: one per fixed axis, each below its axis' tiles.
    pub fn holds(&self, coords: &TileCoords) -> bool {
        coords.len() == self.axes()
            && coords
                .as_slice()
                .iter()
                .zip(self.tiles())
                .all(|(c, t)| c < t)
    }

    /// The tile number of `coords`, None when they name no tile of the grid ([Self::holds]).
    pub fn pack(&self, coords: &TileCoords) -> Option<u128> {
        if !self.holds(coords) {
            return None;
        }
        let mut n = 0u128;
        let fields = coords
            .as_slice()
            .iter()
            .zip(self.widths())
            .zip(&self.shifts);
        // a field of width 0 holds coordinate 0 only (holds checked it) and may sit at shift 128
        for ((c, _), shift) in fields.filter(|((_, w), _)| **w != 0) {
            n |= u128::from(*c).checked_shl(u32::from(*shift))?;
        }
        Some(n)
    }

    /// The tile coordinates a tile number holds, by shift and mask; None when it has bits set outside the fields or a
    /// coordinate past its axis' tiles: a number no writer of this grid writes.
    pub fn unpack(&self, number: u128) -> Option<TileCoords> {
        let mut out = TileCoords {
            len: self.axes,
            ..TileCoords::NONE
        };
        let mut rest = number;
        for ((d, w), shift) in out.coords.iter_mut().zip(self.widths()).zip(&self.shifts) {
            let mask = field_mask(*w);
            let field = number.checked_shr(u32::from(*shift)).unwrap_or(0) & mask;
            rest &= !mask.checked_shl(u32::from(*shift)).unwrap_or(0);
            *d = u32::try_from(field).ok()?;
        }
        (rest == 0 && self.holds(&out)).then_some(out)
    }

    /// The bits the fields take in a tile number.
    pub fn field_bits(&self) -> u128 {
        // bits is at most 128: a shift of 0 to 128, 128 leaving none
        u128::MAX
            .checked_shr(TILE_NUMBER_BITS.saturating_sub(self.bits()))
            .filter(|_| self.bits() != 0)
            .unwrap_or(0)
    }

    /// The coordinate of axis `axis` in a tile number by shift and mask alone, without the others; None past the
    /// fixed axes.
    pub fn coord(&self, number: u128, axis: usize) -> Option<u32> {
        let w = *self.widths().get(axis)?;
        let shift = *self.shifts.get(axis)?;
        let field = number.checked_shr(u32::from(shift)).unwrap_or(0) & field_mask(w);
        u32::try_from(field).ok()
    }

    /// The tiles a box of cells meets along each fixed axis, as ranges of tile coordinates: `cells`, one range per
    /// fixed axis, clipped to the axis' extent. None when `cells` doesn't name each fixed axis once; an empty range
    /// where a box meets no cell of its axis.
    pub fn tile_box(&self, cells: &[Range<u32>]) -> Option<TileBox> {
        if cells.len() != self.axes() {
            return None;
        }
        let mut out = TileBox {
            layout: *self,
            ranges: [(0, 0); GRID_FIXED_AXES_MAX],
        };
        for ((r, c), &(extent, tile)) in out.ranges.iter_mut().zip(cells).zip(&self.extents) {
            let end = c.end.min(extent);
            // tile is at least 1, checked when the layout was made
            let first = c.start.checked_div(tile);
            let past = end
                .checked_sub(1)
                .and_then(|last| last.checked_div(tile))
                .and_then(|t| t.checked_add(1));
            *r = match (first, past) {
                (Some(lo), Some(hi)) if c.start < end => (lo, hi),
                _ => (0, 0),
            };
        }
        Some(out)
    }
}

/// Mask of a field of `width` bits.
fn field_mask(width: u8) -> u128 {
    u128::MAX
        .checked_shr(TILE_NUMBER_BITS.saturating_sub(u32::from(width)))
        .filter(|_| width != 0)
        .unwrap_or(0)
}

/// Put `v` at `i` of `a`; `i` is below its length.
fn set<T>(a: &mut [T], i: usize, v: T) {
    if let Some(d) = a.get_mut(i) {
        *d = v;
    }
}

/// The tiles a box of cells meets in a grid, a half-open range of tile coordinates per fixed axis
/// ([TileLayout::tile_box]).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TileBox {
    /// The grid's layout, whose tile numbers the box tests.
    layout: TileLayout,
    ranges: [(u32, u32); GRID_FIXED_AXES_MAX],
}

impl TileBox {
    /// Whether the box meets no tile: an empty range on some axis.
    pub fn is_empty(&self) -> bool {
        self.ranges().iter().any(|(lo, hi)| lo >= hi)
    }

    /// The range of tile coordinates per fixed axis, half-open.
    pub fn ranges(&self) -> &[(u32, u32)] {
        self.ranges.get(..self.layout.axes()).unwrap_or(&[])
    }

    /// The layout of the grid the box is in.
    pub fn layout(&self) -> &TileLayout {
        &self.layout
    }

    /// Whether the tile of tile number `number` lies in the box, each axis tested by shift and mask alone; a number
    /// with bits set outside the fields lies in no box.
    pub fn holds(&self, number: u128) -> bool {
        number & !self.layout.field_bits() == 0
            && self.ranges().iter().enumerate().all(|(axis, (lo, hi))| {
                self.layout
                    .coord(number, axis)
                    .is_some_and(|c| *lo <= c && c < *hi)
            })
    }

    /// Whether the tile at `coords` lies in the box: one coordinate per fixed axis, each in its range.
    pub fn holds_coords(&self, coords: &TileCoords) -> bool {
        coords.len() == self.layout.axes()
            && coords
                .as_slice()
                .iter()
                .zip(self.ranges())
                .all(|(c, (lo, hi))| *lo <= *c && *c < *hi)
    }
}
