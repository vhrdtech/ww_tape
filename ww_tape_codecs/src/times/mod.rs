//! Time and value columns: starts in a time mode, durations and flags packed per group of 128 items, and the
//! values of a relation, each read in place over a column's bytes and an item count.
//!
//! A reader checks every group record once and then reads without failing; a salvage read checks the groups of a
//! damaged column one by one and marks the ones that fail. The layouts, what a read checks and what a salvage keeps
//! are documented with the events chunk that holds these columns (`ww_tape::times`).

mod column;
mod groups;
mod packed;
mod starts;
mod values;

pub use column::{ColumnError, TimeMode};
#[cfg(feature = "alloc")]
pub(crate) use groups::encode_groups;
pub use packed::{Durations, Flags};
#[cfg(feature = "alloc")]
pub use packed::{encode_durations, encode_flags};
#[cfg(feature = "alloc")]
pub use starts::encode_starts;
pub use starts::{Iter, SLOPE_MAX, Starts};
#[cfg(feature = "alloc")]
pub use values::encode_values;
pub use values::{Values, ordered, unordered};
