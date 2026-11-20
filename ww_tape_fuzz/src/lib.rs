//! Fuzzing of ww_tape's decoders: the bodies of the `fuzz/` targets as plain functions ([targets]), and the
//! structure-aware builders of their hostile inputs ([hostile]).
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unreachable,
    clippy::unwrap_used,
    reason = "fuzz bodies and hostile builders: a panic is the finding a fuzz target reports, and builders compute offsets freely"
)]

pub mod case;
pub mod hostile;
pub mod seeds;
pub mod targets;
