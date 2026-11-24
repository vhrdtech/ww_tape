//! Space overviews of a grid tile: its cells halved along the downsampled axes, a level at a time.
//!
//! [OverviewCells::of_cells] takes a tile's time steps as written and gives level 1: per cell of the halved tile the
//! smallest, the largest and the sum of its source cells per lane, and how many source cells it covers.
//! [OverviewCells::halve] takes level k to level k + 1 from those alone, so every level's min and max are exact and its
//! sums are those of the cells as written, summed pairwise level by level. [OverviewCells::means] divides: an integer
//! mean is rounded to the nearest, ties to even; a float's is summed in f64 and stored in the element's type.
//!
//! Cells run time outermost, then the fixed axes in definition order, row-major, `lanes` scalars each (a cell's
//! channels and components interleaved): a tile's order ([ww_tape::grid#tiles]).

use alloc::vec;
use alloc::vec::Vec;

use crate::Scalar;

/// Why a tile's cells don't halve.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum OverviewError {
    /// No lanes, or a cell count past usize.
    Shape,
    /// The cells are not the time steps times the tile's cells times the lanes.
    Len { len: usize, want: usize },
}

impl core::fmt::Display for OverviewError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Shape => f.write_str("a tile of no lanes, or of cells past usize"),
            Self::Len { len, want } => write!(f, "{len} scalars where the tile holds {want}"),
        }
    }
}

impl core::error::Error for OverviewError {}

/// The statistics of one overview level of a tile, before the means: see the [module docs](self).
#[derive(Clone, Debug, PartialEq)]
pub struct OverviewCells<S: Scalar> {
    /// Cells of the tile along each fixed axis at this level.
    extents: Vec<u32>,
    lanes: usize,
    steps: usize,
    min: Vec<S>,
    max: Vec<S>,
    /// Sums of an integer element, exact; empty for a float.
    isum: Vec<i128>,
    /// Sums of a float element; empty for an integer.
    fsum: Vec<f64>,
    /// Source cells as written per cell, the same for every lane and time step.
    count: Vec<u32>,
}

/// What a level is halved from: the cells as written, or the level below.
trait Source<S> {
    fn lanes(&self) -> usize;
    /// The smallest, largest and sum (integer, float) of scalar `i`; the source cells cell `c` covers.
    fn at(&self, i: usize) -> Option<(S, S, i128, f64)>;
    fn count(&self, c: usize) -> Option<u32>;
}

/// The cells of a tile as written.
struct Written<'a, S> {
    cells: &'a [S],
    lanes: usize,
}

impl<S: Scalar> Source<S> for Written<'_, S> {
    fn lanes(&self) -> usize {
        self.lanes
    }

    fn at(&self, i: usize) -> Option<(S, S, i128, f64)> {
        let v = *self.cells.get(i)?;
        Some((v, v, v.to_i128(), v.to_f64()))
    }

    fn count(&self, _c: usize) -> Option<u32> {
        Some(1)
    }
}

impl<S: Scalar> Source<S> for OverviewCells<S> {
    fn lanes(&self) -> usize {
        self.lanes
    }

    fn at(&self, i: usize) -> Option<(S, S, i128, f64)> {
        let isum = if S::INT { *self.isum.get(i)? } else { 0 };
        let fsum = if S::INT { 0.0 } else { *self.fsum.get(i)? };
        Some((*self.min.get(i)?, *self.max.get(i)?, isum, fsum))
    }

    fn count(&self, c: usize) -> Option<u32> {
        self.count.get(c).copied()
    }
}

/// Cells of a box of `extents`; None past usize.
fn cells_of(extents: &[u32]) -> Option<usize> {
    extents
        .iter()
        .try_fold(1usize, |n, e| n.checked_mul(usize::try_from(*e).ok()?))
}

/// Whether bit `axis` of `mask` is set.
fn halved(mask: u32, axis: usize) -> bool {
    u32::try_from(axis)
        .ok()
        .and_then(|a| mask.checked_shr(a))
        .is_some_and(|b| b & 1 != 0)
}

impl<S: Scalar> OverviewCells<S> {
    /// Level 1 of `steps` time steps of a tile of `extents` cells along its fixed axes, `lanes` scalars a cell: each
    /// axis whose bit is set in `halve` halved, rounded up. `cells` holds the steps one after the other.
    pub fn of_cells(
        cells: &[S],
        steps: usize,
        extents: &[u32],
        lanes: usize,
        halve: u32,
    ) -> Result<Self, OverviewError> {
        let want = cells_of(extents)
            .and_then(|n| n.checked_mul(steps)?.checked_mul(lanes))
            .ok_or(OverviewError::Shape)?;
        if cells.len() != want {
            return Err(OverviewError::Len {
                len: cells.len(),
                want,
            });
        }
        Self::halved(&Written { cells, lanes }, steps, extents, halve)
    }

    /// The next level up: each axis whose bit is set in `halve` halved again, rounded up.
    pub fn halve(&self, halve: u32) -> Result<Self, OverviewError> {
        Self::halved(self, self.steps, &self.extents, halve)
    }

    /// `src`, of `steps` time steps of `extents` cells, halved along the axes of `mask`.
    fn halved(
        src: &impl Source<S>,
        steps: usize,
        extents: &[u32],
        mask: u32,
    ) -> Result<Self, OverviewError> {
        let lanes = src.lanes();
        if lanes == 0 {
            return Err(OverviewError::Shape);
        }
        let to: Vec<u32> = extents
            .iter()
            .enumerate()
            .map(|(a, e)| if halved(mask, a) { e.div_ceil(2) } else { *e })
            .collect();
        let from_cells = cells_of(extents).ok_or(OverviewError::Shape)?;
        let to_cells = cells_of(&to).ok_or(OverviewError::Shape)?;
        let scalars = to_cells
            .checked_mul(lanes)
            .and_then(|n| n.checked_mul(steps))
            .ok_or(OverviewError::Shape)?;
        let mut out = Self {
            extents: to,
            lanes,
            steps,
            min: Vec::with_capacity(scalars),
            max: Vec::with_capacity(scalars),
            isum: vec![0; if S::INT { scalars } else { 0 }],
            fsum: vec![0.0; if S::INT { 0 } else { scalars }],
            count: vec![0; to_cells],
        };
        // the target cell of each source cell, by its coordinates, once for every step
        let map =
            target_map(extents, &out.extents, mask, from_cells).ok_or(OverviewError::Shape)?;
        for (c, t) in map.iter().enumerate() {
            let n = src.count(c).ok_or(OverviewError::Shape)?;
            if let Some(d) = out.count.get_mut(*t) {
                *d = d.saturating_add(n);
            }
        }
        out.min.resize(scalars, S::HIGHEST);
        out.max.resize(scalars, S::LOWEST);
        let mut seen = vec![false; scalars];
        for step in 0..steps {
            for (c, t) in map.iter().enumerate() {
                let from = step
                    .checked_mul(from_cells)
                    .and_then(|s| s.checked_add(c))
                    .and_then(|i| i.checked_mul(lanes))
                    .ok_or(OverviewError::Shape)?;
                let into = step
                    .checked_mul(to_cells)
                    .and_then(|s| s.checked_add(*t))
                    .and_then(|i| i.checked_mul(lanes))
                    .ok_or(OverviewError::Shape)?;
                for l in 0..lanes {
                    let (lo, hi, isum, fsum) = from
                        .checked_add(l)
                        .and_then(|i| src.at(i))
                        .ok_or(OverviewError::Shape)?;
                    let i = into.checked_add(l).ok_or(OverviewError::Shape)?;
                    out.add(i, &mut seen, (lo, hi, isum, fsum));
                }
            }
        }
        Ok(out)
    }

    /// Take a source's statistics into scalar `i`; the first sets them, so a float NaN of the first source stays.
    fn add(&mut self, i: usize, seen: &mut [bool], (lo, hi, isum, fsum): (S, S, i128, f64)) {
        let first = seen
            .get_mut(i)
            .is_some_and(|s| !core::mem::replace(s, true));
        if let Some(m) = self.min.get_mut(i)
            && (first || lo < *m)
        {
            *m = lo;
        }
        if let Some(m) = self.max.get_mut(i)
            && (first || hi > *m)
        {
            *m = hi;
        }
        // a tile step holds at most GRID_TILE_CELLS_MAX (2^26) cells of at most 64 bits: sums stay under 2^90
        if let Some(s) = self.isum.get_mut(i) {
            *s = s.saturating_add(isum);
        }
        if let Some(s) = self.fsum.get_mut(i) {
            *s += fsum;
        }
    }

    /// Cells of the tile at this level along each fixed axis.
    pub fn extents(&self) -> &[u32] {
        &self.extents
    }

    /// Time steps.
    pub fn steps(&self) -> usize {
        self.steps
    }

    /// The smallest value of each cell's source cells, per lane: steps, then cells, then lanes.
    pub fn min(&self) -> &[S] {
        &self.min
    }

    /// The largest value of each cell's source cells, as [Self::min].
    pub fn max(&self) -> &[S] {
        &self.max
    }

    /// Source cells as written that each cell of a time step covers.
    pub fn counts(&self) -> &[u32] {
        &self.count
    }

    /// The mean of each cell's source cells, as [Self::min]: an integer's rounded to the nearest, ties to even; a
    /// float's divided in f64.
    pub fn means(&self) -> Vec<S> {
        let cells = self.count.len().max(1);
        let lanes = self.lanes.max(1);
        let count = |i: usize| {
            let c = i.checked_div(lanes).and_then(|i| i.checked_rem(cells));
            c.and_then(|c| self.count.get(c))
                .copied()
                .unwrap_or(1)
                .max(1)
        };
        if S::INT {
            self.isum
                .iter()
                .enumerate()
                .map(|(i, s)| S::from_i128(div_even(*s, count(i))))
                .collect()
        } else {
            self.fsum
                .iter()
                .enumerate()
                .map(|(i, s)| S::from_f64(*s / f64::from(count(i)), false))
                .collect()
        }
    }
}

/// `sum / n` rounded to the nearest, ties to even; `n` is 1 and up.
pub fn div_even(sum: i128, n: u32) -> i128 {
    let n = i128::from(n.max(1));
    let q = sum.div_euclid(n);
    let r = sum.rem_euclid(n);
    // r is below n, at most u32::MAX: twice it fits
    let twice = r.saturating_mul(2);
    if twice > n || (twice == n && q.rem_euclid(2) == 1) {
        q.saturating_add(1)
    } else {
        q
    }
}

/// For each of the `cells` cells of a box of `from` extents, row-major, the cell of the box of `to` extents it falls in
/// when the axes of `mask` are halved.
fn target_map(from: &[u32], to: &[u32], mask: u32, cells: usize) -> Option<Vec<usize>> {
    let mut out = Vec::with_capacity(cells);
    let mut at = vec![0u32; from.len()];
    for _ in 0..cells {
        let mut t = 0usize;
        for (a, (c, n)) in at.iter().zip(to).enumerate() {
            let c = if halved(mask, a) { c >> 1 } else { *c };
            t = t
                .checked_mul(usize::try_from(*n).ok()?)?
                .checked_add(usize::try_from(c).ok()?)?;
        }
        out.push(t);
        // the next cell, the last axis fastest
        for (c, e) in at.iter_mut().zip(from).rev() {
            *c = c.saturating_add(1);
            if *c < *e {
                break;
            }
            *c = 0;
        }
    }
    Some(out)
}
