//! The summary leaves of a chunk a [ChunkSender](crate::ChunkSender)'s caller encoded itself: computed on the
//! caller's thread, as a typed handle computes them, and joined on the writing thread into the track's tree.
//!
//! A caller that keeps its own open chunk (its own encoding, its own view of the items not yet placed) gets the
//! summaries of a typed handle this way: the mux owns the cuts, so it ends each tree in the piece its items are in,
//! and a resumed mux rebuilds the tree from the chunks the piece kept. A caller sends leaves, never finished summary
//! blocks.

use exact_units::Magnitude;
use ww_tape::{ItemNum, Ratio64, Sample, SampleShape, SeriesGrid, TileCoords, TrackDef, TrackKind};
use ww_tape_summary::{
    BuildError, EventFields, EventRuns, ItemRun, Scalar, SeriesLeaf, SeriesSummarySpec,
};

use crate::error::CallError;
use crate::grid::Leaves as GridLeaves;
use crate::summaries::ChunkSummary;

/// The summary leaves of one chunk, for [ChunkSender::send_placed](crate::ChunkSender::send_placed).
pub struct Leaves(Of);

enum Of {
    Events {
        fields: EventFields,
        runs: Vec<(ItemNum, ItemRun)>,
    },
    Series(Vec<SeriesLeaf>),
}

impl Leaves {
    /// The leaves of an events chunk: the runs `runs` took of its items, from the chunk's first item on. The runs
    /// are completed at the chunk's end and taken; `runs` goes on with the next chunk's items.
    ///
    /// [ChunkSender::event_runs](crate::ChunkSender::event_runs) gives runs of the fields the track's tree has.
    pub fn events(runs: &mut EventRuns) -> Self {
        runs.complete();
        Self(Of::Events {
            fields: runs.fields(),
            runs: runs.take_runs(),
        })
    }

    /// The leaves of a series chunk: those `leaves` took of its samples.
    pub fn series<S: Scalar + Sample>(mut leaves: SampleLeaves<S>) -> Result<Self, BuildError> {
        Ok(Self(Of::Series(leaves.0.complete()?)))
    }
}

/// The leaves of one series chunk's samples, as a [GridHandle](crate::GridHandle) computes them: in the scalar its
/// statistics are kept in. [ChunkSender::sample_leaves](crate::ChunkSender::sample_leaves) gives one per chunk.
pub struct SampleLeaves<S: Scalar>(GridLeaves<S>);

impl<S: Scalar + Sample> SampleLeaves<S> {
    /// Push the next samples, whole time steps of the track's shape, flagged with `flags`.
    pub fn push(&mut self, samples: &[S], flags: u32) -> Result<(), BuildError> {
        self.0.push(samples, flags)
    }

    /// The scale of the samples from here on: the chunk's grid's.
    pub fn set_scale(&mut self, scale: Magnitude) -> Result<(), BuildError> {
        self.0.set_scale(scale)
    }
}

/// What the tree of a summarized sender's track is made of, from its definition: as a resume reads it back.
#[derive(Copy, Clone, Debug)]
pub(crate) enum SummaryOf {
    Events(EventFields),
    Series {
        shape: SampleShape,
        period: Ratio64,
        spec: SeriesSummarySpec,
    },
}

impl SummaryOf {
    /// The summaries of track `def` as a typed handle writes them: None for a track without them, a grid of tiles
    /// or a media track.
    pub(crate) fn of(def: &TrackDef<'_>) -> Option<Self> {
        if let TrackKind::Events(_) = def.kind {
            return crate::events::event_fields(def).map(Self::Events);
        }
        let (series, shape) = (def.series()?, def.series_shape()?);
        let spec = series.summaries.filter(|_| series.media.is_none())?;
        // a series of one cell a time step: no tiles
        let cells = series.tile_layout().ok()?.tile_cells(&TileCoords::NONE)?;
        (cells == 1).then_some(Self::Series {
            shape,
            period: series.period,
            spec,
        })
    }

    /// Runs of the track's items from `first_item` on.
    pub(crate) fn event_runs(&self, first_item: ItemNum) -> Option<EventRuns> {
        match self {
            Self::Events(fields) => Some(EventRuns::new(*fields, first_item)),
            Self::Series { .. } => None,
        }
    }

    /// Leaves of a series chunk from time step `first_item` on, on `grid`.
    pub(crate) fn sample_leaves<S: Scalar + Sample>(
        &self,
        first_item: ItemNum,
        grid: SeriesGrid,
    ) -> Result<Option<SampleLeaves<S>>, BuildError> {
        match *self {
            Self::Series { shape, spec, .. } if shape.element.scalar() == <S as Sample>::SCALAR => {
                Ok(Some(SampleLeaves(GridLeaves::new(
                    shape, grid, first_item, spec, 1,
                )?)))
            }
            _ => Ok(None),
        }
    }

    /// The summary of a chunk of tile `tile` from `leaves`; None when they are not of this track's kind or fields.
    pub(crate) fn summary(
        &self,
        leaves: Leaves,
        tile: TileCoords,
    ) -> Result<ChunkSummary, CallError> {
        match (*self, leaves.0) {
            (Self::Events(want), Of::Events { fields, runs }) if fields == want => {
                Ok(ChunkSummary::Events { fields, runs })
            }
            (
                Self::Series {
                    shape,
                    period,
                    spec,
                },
                Of::Series(leaves),
            ) if tile == TileCoords::NONE => Ok(ChunkSummary::Series {
                shape,
                period,
                spec,
                leaves,
                tile,
                tile_cells: 1,
            }),
            _ => Err(CallError::WrongLeaves),
        }
    }
}
