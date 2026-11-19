//! What summaries a series track declares: the statistics, their order and widths in a block, the base node.

use shrink_wrap::prelude::*;

use crate::Element;

/// Scalars per sample of `element`: 2 for a complex element, else 1.
pub fn components(element: Element) -> u8 {
    if element.parts() == 2 { 2 } else { 1 }
}

/// Whether `element` is a complex pair.
pub fn is_complex(element: Element) -> bool {
    element.parts() == 2
}

/// The statistics a series track keeps per channel and node. Each merges from two adjacent nodes without the samples.
#[derive_shrink_wrap(
    borrowed,
    owned(feature = "std"),
    derive(Debug, PartialEq, Eq, Clone, Copy, Default, Hash)
)]
pub struct SeriesStats {
    /// The smallest sample (per component), NaN left out.
    pub min: bool,
    /// The largest sample (per component), NaN left out.
    pub max: bool,
    /// The sum of the samples (per component), f64; the mean is the sum over the node's samples.
    pub sum: bool,
    /// The sum of the squares (per component), f64: the RMS with the sum.
    pub sum_sq: bool,
    /// Samples clipped (at or past the [ClipRange], any component) or NaN.
    pub clipped: bool,
    /// The OR of the flags words the writer was handed with the node's samples.
    pub flags: bool,
    /// The mean of `I² + Q²` (complex elements), f32.
    pub power_mean: bool,
    /// The largest `I² + Q²` (complex elements), f32: max hold, so short bursts show at every level.
    pub power_max: bool,
}

/// One statistic of [SeriesStats].
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Stat {
    Min,
    Max,
    Sum,
    SumSq,
    Clipped,
    Flags,
    PowerMean,
    PowerMax,
}

impl Stat {
    /// Every statistic, in the order a summary block stores them.
    pub const ALL: [Stat; 8] = [
        Stat::Min,
        Stat::Max,
        Stat::Sum,
        Stat::SumSq,
        Stat::Clipped,
        Stat::Flags,
        Stat::PowerMean,
        Stat::PowerMax,
    ];

    /// Bytes of one value.
    pub fn width(self) -> usize {
        match self {
            Stat::Min | Stat::Max | Stat::Sum | Stat::SumSq | Stat::Clipped => 8,
            Stat::Flags | Stat::PowerMean | Stat::PowerMax => 4,
        }
    }

    /// Values per channel and node: the element's components for minimum, maximum and sums, else 1.
    pub fn components(self, element: Element) -> u8 {
        match self {
            Stat::Min | Stat::Max | Stat::Sum | Stat::SumSq => components(element),
            _ => 1,
        }
    }

    /// The statistic's bit in the fields word: `1 <<` its place in [Self::ALL].
    fn bit(self) -> u16 {
        match self {
            Stat::Min => 1,
            Stat::Max => 1 << 1,
            Stat::Sum => 1 << 2,
            Stat::SumSq => 1 << 3,
            Stat::Clipped => 1 << 4,
            Stat::Flags => 1 << 5,
            Stat::PowerMean => 1 << 6,
            Stat::PowerMax => 1 << 7,
        }
    }

    /// Bytes per channel and node: [Self::width] times [Self::components].
    fn node_len(self, element: Element) -> usize {
        match self {
            Stat::Min | Stat::Max | Stat::Sum | Stat::SumSq if is_complex(element) => 16,
            _ => self.width(),
        }
    }
}

impl SeriesStats {
    /// Every statistic for an element: power only for complex ones.
    pub fn all(element: Element) -> Self {
        SeriesStats {
            min: true,
            max: true,
            sum: true,
            sum_sq: true,
            clipped: true,
            flags: true,
            power_mean: is_complex(element),
            power_max: is_complex(element),
        }
    }

    pub fn has(self, stat: Stat) -> bool {
        match stat {
            Stat::Min => self.min,
            Stat::Max => self.max,
            Stat::Sum => self.sum,
            Stat::SumSq => self.sum_sq,
            Stat::Clipped => self.clipped,
            Stat::Flags => self.flags,
            Stat::PowerMean => self.power_mean,
            Stat::PowerMax => self.power_max,
        }
    }

    /// Whether no statistic is kept.
    pub fn is_empty(self) -> bool {
        self.bits() == 0
    }

    pub(super) fn bits(self) -> u16 {
        Stat::ALL
            .into_iter()
            .filter(|s| self.has(*s))
            .fold(0, |b, s| b | s.bit())
    }

    pub(super) fn from_bits(v: u16) -> Option<Self> {
        if v >> Stat::ALL.len() != 0 {
            return None;
        }
        let on = |s: Stat| v & s.bit() != 0;
        Some(SeriesStats {
            min: on(Stat::Min),
            max: on(Stat::Max),
            sum: on(Stat::Sum),
            sum_sq: on(Stat::SumSq),
            clipped: on(Stat::Clipped),
            flags: on(Stat::Flags),
            power_mean: on(Stat::PowerMean),
            power_max: on(Stat::PowerMax),
        })
    }

    /// Bytes per channel and node: at most 8 statistics of 16 bytes.
    pub fn node_len(self, element: Element) -> usize {
        Stat::ALL
            .into_iter()
            .filter(|s| self.has(*s))
            .map(|s| s.node_len(element))
            .sum()
    }
}

/// Where a sample counts as clipped: at or below `low`, at or above `high`, or NaN, in the samples' own units.
///
/// Raw sample values on purpose: the limits are compared with the stored samples (an ADC's codes), whatever unit the
/// track gives them.
#[derive_shrink_wrap(
    borrowed,
    owned(feature = "std"),
    derive(Debug, PartialEq, Clone, Copy)
)]
pub struct ClipRange {
    pub low: f64,
    pub high: f64,
}

/// What summaries a series track keeps: its declaration, as the track definition carries it.
#[derive_shrink_wrap(
    borrowed,
    owned(feature = "std"),
    derive(Debug, PartialEq, Clone, Copy)
)]
pub struct SeriesSummarySpec {
    /// A level-0 node holds `2^base_shift` samples, [Self::BASE_SHIFT_MIN] to [Self::BASE_SHIFT_MAX]; each level up 16
    /// times more. A raw format field: log2 of a sample count.
    pub base_shift: u8,
    pub stats: SeriesStats,
    /// Where samples clip; None: at the scalar type's ends for integers, infinities for floats.
    pub clip: Option<ClipRange>,
}

impl SeriesSummarySpec {
    /// Smallest [Self::base_shift]: 16 samples.
    pub const BASE_SHIFT_MIN: u8 = 4;
    /// Largest [Self::base_shift]: 16 Mi samples.
    pub const BASE_SHIFT_MAX: u8 = 24;
}
