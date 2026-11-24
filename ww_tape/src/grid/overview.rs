//! The statistics columns of an overview chunk, read in place.

use crate::TileLayout;
use crate::chunk::{Chunk, ColumnKind};
use crate::samples::{SampleError, SampleShape, Samples};

/// The min, max and mean columns of an overview chunk, each a samples column of the overview tile's cells
/// ([grid](super#overviews)).
#[derive(Copy, Clone, Debug)]
pub struct Overview<'b> {
    level: u8,
    cells: usize,
    min: Samples<'b>,
    max: Samples<'b>,
    mean: Samples<'b>,
}

/// Why an overview chunk's columns don't read.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum OverviewError {
    /// The chunk is no overview (level 0), its level is past the grid's levels or its tile is no tile of the grid.
    NotOverview,
    /// The chunk's tile cells over its time steps don't fit a usize.
    TooManyCells,
    /// A statistics column is missing.
    Missing(ColumnKind),
    /// A statistics column doesn't hold the cells it must, or in a codec that doesn't code the element.
    Samples(ColumnKind, SampleError),
}

impl core::fmt::Display for OverviewError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotOverview => f.write_str("not an overview chunk of the grid"),
            Self::TooManyCells => f.write_str("an overview's cells past usize"),
            Self::Missing(k) => write!(f, "no {} column", k.named()),
            Self::Samples(k, e) => write!(f, "the {} column: {e}", k.named()),
        }
    }
}

impl core::error::Error for OverviewError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Samples(_, e) => Some(e),
            _ => None,
        }
    }
}

impl<'b> Overview<'b> {
    /// The statistics columns of overview chunk `chunk` of a grid whose cells as written are laid out by `layout`
    /// ([GridDef::tile_layout](crate::GridDef::tile_layout)) and whose cells are `shape`. The cell count comes from
    /// the layout of the chunk's level and its tile, times its time steps; each column must hold that many cells.
    /// Nothing is decoded yet.
    pub fn of(
        chunk: &Chunk<'b>,
        layout: &TileLayout,
        shape: SampleShape,
    ) -> Result<Self, OverviewError> {
        let head = chunk.head();
        let level = head.level;
        if level == 0 {
            return Err(OverviewError::NotOverview);
        }
        let at = layout.overview(level).ok_or(OverviewError::NotOverview)?;
        let tile = at
            .tile_cells(&head.tile)
            .ok_or(OverviewError::NotOverview)?;
        // both factors come from a checked head and layout; the product is bounded by each column's length below
        let cells = tile
            .checked_mul(u64::from(head.count))
            .and_then(|n| usize::try_from(n).ok())
            .ok_or(OverviewError::TooManyCells)?;
        let column = |kind| {
            let c = chunk.find(kind, 0).ok_or(OverviewError::Missing(kind))?;
            Samples::of(&c, shape, cells).map_err(|e| OverviewError::Samples(kind, e))
        };
        Ok(Self {
            level,
            cells,
            min: column(ColumnKind::OverviewMin)?,
            max: column(ColumnKind::OverviewMax)?,
            mean: column(ColumnKind::OverviewMean)?,
        })
    }

    /// The overview level, 1 and up.
    pub fn level(&self) -> u8 {
        self.level
    }

    /// Cells in each column: the tile's cells at the level times the chunk's time steps, time outermost.
    pub fn cells(&self) -> usize {
        self.cells
    }

    /// The smallest value of each cell's source cells.
    pub fn min(&self) -> &Samples<'b> {
        &self.min
    }

    /// The largest value of each cell's source cells.
    pub fn max(&self) -> &Samples<'b> {
        &self.max
    }

    /// The mean of each cell's source cells: rounded to the nearest, ties to even, for an integer element.
    pub fn mean(&self) -> &Samples<'b> {
        &self.mean
    }
}
