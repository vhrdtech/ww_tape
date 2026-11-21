//! Summary trees of [ww_tape] tracks, built into summary blocks and read from them.
//!
//! A track's items (an event track's items, a series' samples) are grouped into nodes over item numbers: a node at
//! level 0 holds `2^shift` items, a node one level up 16 nodes of the level below. The format of a summary block, its
//! head and the order of its columns is in [ww_tape::summary]; ww_tape checks summary blocks and lists them in its
//! index without reading their nodes. This crate builds the nodes and reads them:
//!
//! - **Building**: [EventTree] builds the item tree of an event track, [SeriesSummaries] the statistics a series
//!   track declares ([SeriesSummarySpec]). Both write a block when a level's batch fills and the partial nodes at
//!   `finish`, for [TapeWriter::defer_block](ww_tape::TapeWriter::defer_block).
//! - **Reading**: [Summary] reads a summary block's nodes in place ([Summary::of_block]): [EventNode]s of an event
//!   tree, [ChannelStats] and [StatColumn]s of a series, [SeriesNode]s with every channel at the block's scale.
//!
//! Every statistic merges: a parent is its 16 children merged ([EventNode::merge], [ChannelStats::merge]), in item
//! order, so a reader building coarser bins (a view cache) gets what the writer would have written. Nodes of two
//! scales merge at their [common_scale] ([SeriesNode::merge]); two scales with no common scale are refused, never
//! mixed.
//!
//! The head types of ww_tape's summary module are re-exported here, so everything about summaries is imported from
//! this crate.
//!
//! # Caps
//!
//! | Cap | Value | Bounds |
//! |---|---|---|
//! | [FACTOR_MAX] | 2^53 | a factor to the common scale |
//!
//! The caps of a summary head ([NODE_SHIFT_MAX], [LEVEL_MAX], [SCALE_LEN_MAX], [SeriesSummarySpec::BASE_SHIFT_MAX])
//! are ww_tape's.

#![cfg_attr(not(feature = "std"), no_std)]
#![deny(unsafe_code)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::unreachable,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::as_conversions,
        clippy::cast_lossless,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss,
        reason = "tests fail by panicking and compute expected values freely"
    )
)]

#[cfg(feature = "alloc")]
extern crate alloc;

use ww_tape::summary::components;
pub use ww_tape::summary::{
    ClipRange, EVENT_BASE_SHIFT, EventFields, FANOUT_SHIFT, LEVEL_MAX, Layout, NODE_SHIFT_MAX,
    NotSummary, SCALE_LEN_MAX, SUMMARY_FIXED_LEN, SeriesStats, SeriesSummarySpec, Stat,
    SummaryError, SummaryHead, SummaryWriter, Tree, summary_len,
};
#[cfg(feature = "std")]
use ww_tape::summary::{SUMMARY_MIN_LEN, is_complex, level_shift, node_span};

mod nodes;
#[cfg(feature = "std")]
use nodes::batch_nodes;
pub use nodes::{EventNode, StatColumn, Summary, write_event_summary, write_series_summary};

mod series;
#[cfg(feature = "alloc")]
pub use series::SeriesNode;
pub use series::{ChannelStats, FACTOR_MAX, ScaleError, Value, common_scale, scale_factor};
#[cfg(feature = "std")]
pub use series::{
    SERIES_BLOCK_BYTES, Scalar, SeriesLeaf, SeriesLeaves, SeriesStart, SeriesSummaries, SeriesTree,
};

#[cfg(feature = "std")]
mod levels;
#[cfg(feature = "std")]
pub use levels::BuildError;

#[cfg(feature = "std")]
mod tree;
#[cfg(feature = "std")]
pub use tree::{EVENT_BLOCK_BYTES, EventRuns, EventTree, ItemRun};

#[cfg(all(test, feature = "std"))]
mod kernel_tests;
#[cfg(all(test, feature = "std"))]
mod split_tests;
#[cfg(test)]
mod tests;

/// A block's byte cap as a `usize`.
#[cfg(feature = "std")]
fn block_room(bytes: exact_units::Bytes<u32>) -> usize {
    // a u32 fits a usize on every target the summaries build for
    usize::try_from(bytes.get()).unwrap_or(usize::MAX)
}
