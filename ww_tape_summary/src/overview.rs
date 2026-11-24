//! Space overviews of a grid tile: its cells halved along the downsampled axes, a level at a time.
//!
//! [OverviewCells::of_cells] takes a tile's time steps as written and gives level 1: per cell of the halved tile the
//! smallest, the largest and the sum of its source cells per lane, and how many source cells it covers.
//! [OverviewCells::halve] takes level k to level k + 1 from those alone, so every level's min and max are exact and its
//! sums are those of the cells as written, summed pairwise level by level. [OverviewCells::means] divides: an integer
//! mean is rounded to the nearest, ties to even; a float's is summed in f64 and stored in the element's type.
//!
//! A level holds its statistics and nothing per source cell. An integer's sums are i64 where a whole time step's sum
//! fits one (elements up to 32 bits), else i128.
//!
//! A float NaN is left out of all three, as the series summaries leave it out of their min and max: a cell's statistics
//! are those of its other source cells, whatever their order, and NaN only where every source cell is NaN. An infinity
//! is a value: it is the min or max it is, and the mean of a cell that holds it. A cell holding both infinities has a
//! NaN mean. Of two zeros, -0 is the smaller.
//!
//! A cell stores no count of its NaN source cells, unlike a series summary node. A reader therefore can't tell a cell
//! of some NaN source cells from one of none; it knows only that a NaN cell had no other values.
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
    sums: Sums,
    /// Source cells as written per cell, the same for every lane and time step.
    count: Vec<u32>,
    /// NaN source cells per scalar of a float element; empty until the first NaN.
    nan: Vec<u32>,
}

/// The sums of a level's scalars, NaN left out.
#[derive(Clone, Debug, PartialEq)]
enum Sums {
    /// An integer element whose sum over a time step of the tile fits an i64: exact.
    Narrow(Vec<i64>),
    /// Any other integer element: exact, a tile step's sum under 2^90 (a u64 over 2^26 cells).
    Wide(Vec<i128>),
    /// A float element, summed in f64.
    Float(Vec<f64>),
}

impl Sums {
    /// The sums of `scalars` scalars, all 0: integer ones narrow where `narrow`.
    fn zeros<S: Scalar>(scalars: usize, narrow: bool) -> Self {
        match (S::INT, narrow) {
            (true, true) => Self::Narrow(vec![0; scalars]),
            (true, false) => Self::Wide(vec![0; scalars]),
            (false, _) => Self::Float(vec![0.0; scalars]),
        }
    }

    /// The sum of scalar `i`, as an integer and a float: the other 0.
    fn get(&self, i: usize) -> Option<(i128, f64)> {
        match self {
            Self::Narrow(s) => Some((i128::from(*s.get(i)?), 0.0)),
            Self::Wide(s) => Some((*s.get(i)?, 0.0)),
            Self::Float(s) => Some((0, *s.get(i)?)),
        }
    }

    /// Adds a source's sum to scalar `i`; None past `i` or past the narrow sum, which [Source::narrow] rules out.
    fn add(&mut self, i: usize, isum: i128, fsum: f64) -> Option<()> {
        match self {
            Self::Narrow(s) => {
                let s = s.get_mut(i)?;
                *s = s.checked_add(i64::try_from(isum).ok()?)?;
            }
            Self::Wide(s) => {
                let s = s.get_mut(i)?;
                *s = s.checked_add(isum)?;
            }
            Self::Float(s) => *s.get_mut(i)? += fsum,
        }
        Some(())
    }
}

/// What a level is halved from: the cells as written, or the level below.
trait Source<S> {
    fn lanes(&self) -> usize;
    /// Whether an integer sum over a whole time step of the tile fits an i64.
    fn narrow(&self) -> bool;
    /// The statistics of scalar `i`; the source cells cell `c` covers.
    fn at(&self, i: usize) -> Option<Stats<S>>;
    fn count(&self, c: usize) -> Option<u32>;
}

/// The smallest, largest and sum (integer, float) of a scalar's source cells, NaN left out, and its NaN source cells.
type Stats<S> = (S, S, i128, f64, u32);

/// The cells of a tile as written.
struct Written<'a, S> {
    cells: &'a [S],
    lanes: usize,
    steps: usize,
}

impl<S: Scalar> Source<S> for Written<'_, S> {
    fn lanes(&self) -> usize {
        self.lanes
    }

    fn narrow(&self) -> bool {
        // the largest magnitude a cell holds, times the cells of a time step
        let cells = self
            .cells
            .len()
            .checked_div(self.lanes.checked_mul(self.steps).unwrap_or(0));
        let most = S::HIGHEST
            .to_i128()
            .max(S::LOWEST.to_i128().saturating_neg());
        cells
            .and_then(|c| i128::try_from(c).ok())
            .and_then(|c| c.checked_mul(most))
            .is_some_and(|m| m <= i128::from(i64::MAX))
    }

    fn at(&self, i: usize) -> Option<Stats<S>> {
        let v = *self.cells.get(i)?;
        if is_nan(v) {
            return Some((v, v, 0, 0.0, 1));
        }
        Some((v, v, v.to_i128(), v.to_f64(), 0))
    }

    fn count(&self, _c: usize) -> Option<u32> {
        Some(1)
    }
}

impl<S: Scalar> Source<S> for OverviewCells<S> {
    fn lanes(&self) -> usize {
        self.lanes
    }

    fn narrow(&self) -> bool {
        // a level's cells cover the same source cells as the level below
        matches!(self.sums, Sums::Narrow(_))
    }

    fn at(&self, i: usize) -> Option<Stats<S>> {
        let (isum, fsum) = self.sums.get(i)?;
        // no NaN count yet: none of the scalars has a NaN source cell
        let nan = self.nan.get(i).copied().unwrap_or(0);
        Some((*self.min.get(i)?, *self.max.get(i)?, isum, fsum, nan))
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

/// Whether `v` is a float NaN: NaN compares with nothing, itself included.
fn is_nan<S: Scalar>(v: S) -> bool {
    v.partial_cmp(&v).is_none()
}

/// Whether `v` takes the place of the running smallest (`smaller`) or largest `m`: a NaN `v` never does, and anything
/// takes the place of a NaN `m`; of two zeros, -0 is the smaller. So the result is the same in any order.
fn takes<S: Scalar>(v: S, m: S, smaller: bool) -> bool {
    use core::cmp::Ordering;
    if is_nan(v) {
        return false;
    }
    if is_nan(m) {
        return true;
    }
    match v.partial_cmp(&m) {
        Some(Ordering::Less) => smaller,
        Some(Ordering::Greater) => !smaller,
        Some(Ordering::Equal) => {
            let (v_neg, m_neg) = (v.to_f64().is_sign_negative(), m.to_f64().is_sign_negative());
            if smaller {
                v_neg && !m_neg
            } else {
                m_neg && !v_neg
            }
        }
        None => false,
    }
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
        Self::halved(
            &Written {
                cells,
                lanes,
                steps,
            },
            steps,
            extents,
            halve,
        )
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
        let shape = || OverviewError::Shape;
        let mut out = Self {
            extents: to.clone(),
            lanes,
            steps,
            min: Vec::with_capacity(scalars),
            max: Vec::with_capacity(scalars),
            sums: Sums::zeros::<S>(scalars, src.narrow()),
            count: vec![0; to_cells],
            nan: Vec::new(),
        };
        each_cell(extents, &to, mask, |c, t| {
            let n = src.count(c).ok_or_else(shape)?;
            let d = out.count.get_mut(t).ok_or_else(shape)?;
            // at most a tile step's cells, under GRID_TILE_CELLS_MAX
            *d = d.checked_add(n).ok_or_else(shape)?;
            Ok(())
        })?;
        // a float's min and max start NaN, which any value takes the place of: an all-NaN cell keeps it
        let (lowest, highest) = if S::INT {
            (S::LOWEST, S::HIGHEST)
        } else {
            let nan = S::from_f64(f64::NAN, false);
            (nan, nan)
        };
        out.min.resize(scalars, highest);
        out.max.resize(scalars, lowest);
        for step in 0..steps {
            let from_step = step.checked_mul(from_cells).ok_or_else(shape)?;
            let into_step = step.checked_mul(to_cells).ok_or_else(shape)?;
            each_cell(extents, &to, mask, |c, t| {
                let from = from_step
                    .checked_add(c)
                    .and_then(|i| i.checked_mul(lanes))
                    .ok_or_else(shape)?;
                let into = into_step
                    .checked_add(t)
                    .and_then(|i| i.checked_mul(lanes))
                    .ok_or_else(shape)?;
                for l in 0..lanes {
                    let stats = from
                        .checked_add(l)
                        .and_then(|i| src.at(i))
                        .ok_or_else(shape)?;
                    let i = into.checked_add(l).ok_or_else(shape)?;
                    out.add(i, stats).ok_or_else(shape)?;
                }
                Ok(())
            })?;
        }
        Ok(out)
    }

    /// Take a source's statistics into scalar `i`, NaN left out of the min, max and sum; None past the sums.
    fn add(&mut self, i: usize, (lo, hi, isum, fsum, nan): Stats<S>) -> Option<()> {
        if let Some(m) = self.min.get_mut(i)
            && takes(lo, *m, true)
        {
            *m = lo;
        }
        if let Some(m) = self.max.get_mut(i)
            && takes(hi, *m, false)
        {
            *m = hi;
        }
        if nan > 0 {
            if self.nan.is_empty() {
                self.nan = vec![0; self.min.len()];
            }
            // at most the source cells the cell covers, a u32
            if let Some(n) = self.nan.get_mut(i) {
                *n = n.saturating_add(nan);
            }
        }
        self.sums.add(i, isum, fsum)
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
    /// float's divided in f64, NaN left out, and NaN where every source cell is NaN.
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
        match &self.sums {
            Sums::Narrow(sums) => sums
                .iter()
                .enumerate()
                .map(|(i, s)| S::from_i128(div_even(i128::from(*s), count(i))))
                .collect(),
            Sums::Wide(sums) => sums
                .iter()
                .enumerate()
                .map(|(i, s)| S::from_i128(div_even(*s, count(i))))
                .collect(),
            Sums::Float(sums) => sums
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    // no NaN count yet: no NaN source cell; at most the cell's count, all of them for 0 / 0, NaN
                    let nan = self.nan.get(i).copied().unwrap_or(0);
                    let n = count(i).saturating_sub(nan);
                    S::from_f64(*s / f64::from(n), false)
                })
                .collect(),
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

/// Calls `f` with each cell of a box of `from` extents, row-major, and the cell of the box of `to` extents it falls in
/// when the axes of `mask` are halved: the target computed a row at a time, with no table of the cells.
fn each_cell(
    from: &[u32],
    to: &[u32],
    mask: u32,
    mut f: impl FnMut(usize, usize) -> Result<(), OverviewError>,
) -> Result<(), OverviewError> {
    let shape = || OverviewError::Shape;
    let Some((&last, outer)) = from.split_last() else {
        // no fixed axes: one cell
        return f(0, 0);
    };
    let to_last = to
        .get(outer.len())
        .and_then(|n| usize::try_from(*n).ok())
        .ok_or_else(shape)?;
    let last_halved = halved(mask, outer.len());
    let rows = cells_of(outer).ok_or_else(shape)?;
    let mut at = vec![0u32; outer.len()];
    let mut c = 0usize;
    for _ in 0..rows {
        // the target row: the outer coordinates, halved along the axes of the mask
        let mut row = 0usize;
        for (a, (x, n)) in at.iter().zip(to).enumerate() {
            let x = if halved(mask, a) { x >> 1 } else { *x };
            row = usize::try_from(*n)
                .ok()
                .and_then(|n| row.checked_mul(n))
                .and_then(|r| r.checked_add(usize::try_from(x).ok()?))
                .ok_or_else(shape)?;
        }
        let row = row.checked_mul(to_last).ok_or_else(shape)?;
        for x in 0..last {
            let x = if last_halved { x >> 1 } else { x };
            let t = usize::try_from(x)
                .ok()
                .and_then(|x| row.checked_add(x))
                .ok_or_else(shape)?;
            f(c, t)?;
            c = c.checked_add(1).ok_or_else(shape)?;
        }
        // the next row, the last outer axis fastest
        for (x, e) in at.iter_mut().zip(outer).rev() {
            *x = x.saturating_add(1);
            if *x < *e {
                break;
            }
            *x = 0;
        }
    }
    Ok(())
}
