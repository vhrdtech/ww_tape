//! Overview cells against a naive downsample: every source cell of an overview cell gathered by its coordinates.

use crate::overview::{OverviewCells, OverviewError, div_even};

/// A deterministic stream of numbers.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 11
    }
}

/// Row-major coordinates of cell `i` of a box of `extents`.
fn coords(mut i: usize, extents: &[u32]) -> Vec<u32> {
    let mut out = vec![0; extents.len()];
    for (c, e) in out.iter_mut().zip(extents).rev() {
        *c = (i % *e as usize) as u32;
        i /= *e as usize;
    }
    out
}

/// Per overview cell of level `level` and lane, every source value it covers.
fn naive<S: Copy>(
    cells: &[S],
    steps: usize,
    extents: &[u32],
    lanes: usize,
    mask: u32,
    level: u32,
) -> Vec<Vec<S>> {
    let to: Vec<u32> = extents
        .iter()
        .enumerate()
        .map(|(a, e)| {
            if mask >> a & 1 == 1 {
                e.div_ceil(1 << level)
            } else {
                *e
            }
        })
        .collect();
    let from_n: usize = extents.iter().map(|e| *e as usize).product();
    let to_n: usize = to.iter().map(|e| *e as usize).product();
    let mut out = vec![Vec::new(); steps * to_n * lanes];
    for s in 0..steps {
        for c in 0..from_n {
            let at = coords(c, extents);
            let mut t = 0usize;
            for (a, (x, n)) in at.iter().zip(&to).enumerate() {
                let x = if mask >> a & 1 == 1 { x >> level } else { *x };
                t = t * *n as usize + x as usize;
            }
            for l in 0..lanes {
                out[(s * to_n + t) * lanes + l].push(cells[(s * from_n + c) * lanes + l]);
            }
        }
    }
    out
}

/// Every level of random integer tiles, odd and even extents, some axes kept: min, max and the mean of the source
/// cells, rounded ties to even, as a naive downsample has them.
#[test]
fn integer_levels_are_a_naive_downsample() {
    let mut r = Lcg(7);
    for case in 0..60 {
        let axes = 1 + case % 3;
        let extents: Vec<u32> = (0..axes).map(|_| 1 + (r.next() % 9) as u32).collect();
        let mask = (r.next() as u32 | 1) & ((1 << axes) - 1);
        let lanes = 1 + case % 3;
        let steps = 1 + case % 4;
        let n: usize = extents.iter().map(|e| *e as usize).product::<usize>() * steps * lanes;
        let cells: Vec<i16> = (0..n).map(|_| r.next() as i16).collect();
        let mut level = OverviewCells::of_cells(&cells, steps, &extents, lanes, mask).unwrap();
        for k in 1..=4u32 {
            let want = naive(&cells, steps, &extents, lanes, mask, k);
            let means = level.means();
            assert_eq!(level.min().len(), want.len(), "{extents:?} {mask:b} {k}");
            for (i, vals) in want.iter().enumerate() {
                assert_eq!(level.min()[i], *vals.iter().min().unwrap());
                assert_eq!(level.max()[i], *vals.iter().max().unwrap());
                let sum: i128 = vals.iter().map(|v| i128::from(*v)).sum();
                assert_eq!(
                    i128::from(means[i]),
                    div_even(sum, vals.len() as u32),
                    "{extents:?} {mask:b} level {k} cell {i}"
                );
                let cell = i / lanes % level.counts().len();
                assert_eq!(level.counts()[cell] as usize, vals.len());
            }
            level = level.halve(mask).unwrap();
        }
    }
}

/// A float tile's means are its source cells' sums over their count, within the rounding of pairwise sums.
#[test]
fn float_means_are_the_sums_over_the_counts() {
    let mut r = Lcg(11);
    let extents = [7u32, 5];
    let cells: Vec<f32> = (0..7 * 5 * 2 * 3)
        .map(|_| (r.next() % 1000) as f32 / 8.0)
        .collect();
    let mut level = OverviewCells::of_cells(&cells, 3, &extents, 2, 0b11).unwrap();
    for k in 1..=3 {
        let want = naive(&cells, 3, &extents, 2, 0b11, k);
        for (m, vals) in level.means().iter().zip(&want) {
            let mean = vals.iter().map(|v| f64::from(*v)).sum::<f64>() / vals.len() as f64;
            assert!((f64::from(*m) - mean).abs() < 1e-3, "{m} {mean}");
        }
        level = level.halve(0b11).unwrap();
    }
}

/// An edge cell of one source cell holds it; a u64 tile sums past i64 exactly.
#[test]
fn edges_and_wide_sums() {
    let cells: Vec<u64> = vec![u64::MAX, u64::MAX - 2, 5];
    let l = OverviewCells::of_cells(&cells, 1, &[3], 1, 1).unwrap();
    assert_eq!(l.extents(), [2]);
    assert_eq!(l.counts(), [2, 1]);
    assert_eq!(l.min(), [u64::MAX - 2, 5]);
    assert_eq!(l.means(), [u64::MAX - 1, 5]);
    let kept = OverviewCells::of_cells(&cells, 1, &[3], 1, 0).unwrap();
    assert_eq!(kept.extents(), [3]);
    assert_eq!(kept.means(), cells);
}

#[test]
fn means_round_ties_to_even() {
    for (sum, n, want) in [
        (5, 2, 2),
        (7, 2, 4),
        (-5, 2, -2),
        (-7, 2, -4),
        (4, 3, 1),
        (5, 3, 2),
        (-4, 3, -1),
        (-5, 3, -2),
        (0, 1, 0),
    ] {
        assert_eq!(div_even(sum, n), want, "{sum} / {n}");
    }
}

#[test]
fn cells_not_the_tiles_are_refused() {
    assert_eq!(
        OverviewCells::of_cells(&[1i32; 5], 1, &[2, 2], 1, 3),
        Err(OverviewError::Len { len: 5, want: 4 })
    );
    assert_eq!(
        OverviewCells::<i32>::of_cells(&[], 1, &[2], 0, 1),
        Err(OverviewError::Shape)
    );
}

/// NaN is left out of the min, max and mean in any order of the source cells, and stays only where every one is NaN.
#[test]
fn nan_is_left_out_in_any_order() {
    let nan = f32::NAN;
    for cells in [
        [nan, 1.0, 3.0, nan],
        [1.0, nan, nan, 3.0],
        [3.0, nan, 1.0, nan],
    ] {
        // two cells of two along one axis, then the level above: one cell of four
        let l1 = OverviewCells::of_cells(&cells, 1, &[4], 1, 1).unwrap();
        let l2 = l1.halve(1).unwrap();
        assert_eq!((l2.min()[0], l2.max()[0]), (1.0, 3.0), "{cells:?}");
        assert_eq!(l2.means(), [2.0], "{cells:?}");
        for (m, pair) in l1.means().iter().zip(cells.chunks(2)) {
            let vals: Vec<f32> = pair.iter().copied().filter(|v| !v.is_nan()).collect();
            let want = vals.iter().sum::<f32>() / vals.len() as f32;
            assert_eq!(m.to_bits(), want.to_bits(), "{cells:?} {pair:?}");
        }
    }
    let all = OverviewCells::of_cells(&[nan, nan, 2.0, nan], 1, &[4], 1, 1).unwrap();
    assert!(all.min()[0].is_nan(), "an all-NaN cell's min is NaN");
    assert!(all.max()[0].is_nan(), "an all-NaN cell's max is NaN");
    assert!(all.means()[0].is_nan(), "an all-NaN cell's mean is NaN");
    assert_eq!(
        (all.min()[1], all.max()[1], all.means()[1]),
        (2.0, 2.0, 2.0)
    );
    let up = all.halve(1).unwrap();
    assert_eq!((up.min()[0], up.max()[0], up.means()[0]), (2.0, 2.0, 2.0));
}

/// An infinity is a value: the min, max or mean it is; both infinities make a NaN mean. -0 is below +0 in any order.
#[test]
fn infinities_and_zeros_are_values() {
    let (inf, ninf) = (f64::INFINITY, f64::NEG_INFINITY);
    let l = OverviewCells::of_cells(&[inf, 1.0, ninf, 1.0, inf, ninf], 1, &[6], 1, 1).unwrap();
    assert_eq!(l.min(), [1.0, ninf, ninf]);
    assert_eq!(l.max(), [inf, 1.0, inf]);
    let means = l.means();
    assert_eq!(means[..2], [inf, ninf]);
    assert!(means[2].is_nan(), "both infinities: NaN mean");
    for cells in [[-0.0f32, 0.0], [0.0, -0.0]] {
        let l = OverviewCells::of_cells(&cells, 1, &[2], 1, 1).unwrap();
        assert!(l.min()[0].is_sign_negative(), "{cells:?}: min -0");
        assert!(l.max()[0].is_sign_positive(), "{cells:?}: max +0");
    }
}
