//! What more than one bench binary of ww_tape_benchmarks writes: the series track of i16 samples ([series]).
#![allow(
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    reason = "a bench computes item numbers freely and fails by panicking; the constants it builds from are known good"
)]

pub mod series;
