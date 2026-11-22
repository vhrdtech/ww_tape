//! Series summaries: the statistics a series track declares, per channel and node, and how they merge.

#[cfg(feature = "alloc")]
use alloc::vec::Vec;

use exact_units::Magnitude;

#[cfg(feature = "std")]
use super::{SeriesStats, SeriesSummarySpec, components, is_complex};
use ww_tape::ElementDesc;

/// How a minimum or maximum of `element` is stored.
fn kind(element: ElementDesc) -> ValueKind {
    if element.exponent_bits != 0 {
        ValueKind::Float
    } else if element.signed {
        ValueKind::Signed
    } else {
        ValueKind::Unsigned
    }
}

/// `v` rounded to the nearest f64: sums, means and minimum or maximum read as a float.
#[expect(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    reason = "rounding to the nearest f64 is the result asked for"
)]
fn u64_f64(v: u64) -> f64 {
    v as f64
}

/// `v` rounded to the nearest f64.
#[expect(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    reason = "rounding to the nearest f64 is the result asked for"
)]
fn i64_f64(v: i64) -> f64 {
    v as f64
}

/// `v` rounded to the nearest f64: an exact integer sum stored as the format's f64.
#[cfg(feature = "std")]
#[expect(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    reason = "the format stores sums as f64: rounding to the nearest is its meaning"
)]
fn i128_f64(v: i128) -> f64 {
    v as f64
}

/// `v` rounded to the nearest f32: the format stores powers as f32.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "the format stores powers as f32: rounding to the nearest is its meaning"
)]
fn f64_f32(v: f64) -> f32 {
    v as f32
}

/// How a minimum or maximum is stored.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ValueKind {
    Signed,
    Unsigned,
    Float,
}

/// A statistic's value: minimum and maximum in the element's kind, sums and powers as f64, counts and flags as u64.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Value {
    I64(i64),
    U64(u64),
    F64(f64),
}

impl Value {
    pub(super) fn from_bits(element: ElementDesc, bits: u64) -> Self {
        match kind(element) {
            ValueKind::Signed => Self::I64(bits.cast_signed()),
            ValueKind::Unsigned => Self::U64(bits),
            ValueKind::Float => Self::F64(f64::from_bits(bits)),
        }
    }

    pub(super) fn to_bits(self) -> u64 {
        match self {
            Self::I64(v) => v.cast_unsigned(),
            Self::U64(v) => v,
            Self::F64(v) => v.to_bits(),
        }
    }

    /// The value as f64, rounded to nearest.
    pub fn as_f64(self) -> f64 {
        match self {
            Self::I64(v) => i64_f64(v),
            Self::U64(v) => u64_f64(v),
            Self::F64(v) => v,
        }
    }

    fn pick(self, other: Self, smaller: bool) -> Self {
        let take_other = match (self, other) {
            (Self::I64(a), Self::I64(b)) => (b < a) == smaller && b != a,
            (Self::U64(a), Self::U64(b)) => (b < a) == smaller && b != a,
            (Self::F64(a), Self::F64(b)) => a.is_nan() || (if smaller { b < a } else { b > a }),
            _ => false,
        };
        if take_other { other } else { self }
    }
}

/// Why statistics at two scales can't be brought to one.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScaleError {
    /// No scale both are whole multiples of: their ratio isn't a fraction of integers (a power of π, a half
    /// exponent), or the common scale is out of a [Magnitude]'s range.
    NoCommonScale,
    /// A factor to the common scale past [FACTOR_MAX], or a minimum or maximum past its 64-bit type there.
    Overflow,
}

impl core::fmt::Display for ScaleError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoCommonScale => write!(f, "the two scales have no common scale"),
            Self::Overflow => write!(f, "statistics overflow at the common scale"),
        }
    }
}

impl core::error::Error for ScaleError {}

/// Largest factor from a scale to the common scale: every factor is then exact as an f64.
pub const FACTOR_MAX: u64 = 1 << 53;

/// The largest scale `a` and `b` are both whole multiples of: `10⁻⁶` for `10⁻³` and `10⁻⁶`, `1/1000` for `1/1000` and
/// `1024`.
///
/// The scale statistics of samples at both scales are kept in, so minimum and maximum stay exact integers.
pub fn common_scale(a: Magnitude, b: Magnitude) -> Result<Magnitude, ScaleError> {
    if a == b {
        return Ok(a);
    }
    // a / b = p / q in lowest terms, so a = (b / q) · p and b = (b / q) · q
    let (_p, q) = a
        .checked_div(b)
        .ok()
        .and_then(|r| r.to_ratio())
        .ok_or(ScaleError::NoCommonScale)?;
    let q = u64::try_from(q).ok().ok_or(ScaleError::NoCommonScale)?;
    Magnitude::checked_ratio(1, q)
        .and_then(|r| b.checked_mul(r))
        .ok()
        .ok_or(ScaleError::NoCommonScale)
}

/// `scale / common` as a whole number, at most [FACTOR_MAX].
pub fn scale_factor(scale: Magnitude, common: Magnitude) -> Result<u64, ScaleError> {
    if scale == common {
        return Ok(1);
    }
    let (n, d) = scale
        .checked_div(common)
        .ok()
        .and_then(|r| r.to_ratio())
        .ok_or(ScaleError::NoCommonScale)?;
    if d != 1 {
        return Err(ScaleError::NoCommonScale);
    }
    u64::try_from(n)
        .ok()
        .filter(|n| *n <= FACTOR_MAX)
        .ok_or(ScaleError::Overflow)
}

impl Value {
    /// The value times `k`, exact for integers.
    fn times(self, k: u64) -> Result<Self, ScaleError> {
        Ok(match self {
            Self::I64(v) => Self::I64(
                i128::from(v)
                    .checked_mul(i128::from(k))
                    .and_then(|x| i64::try_from(x).ok())
                    .ok_or(ScaleError::Overflow)?,
            ),
            Self::U64(v) => Self::U64(v.checked_mul(k).ok_or(ScaleError::Overflow)?),
            Self::F64(v) => Self::F64(v * u64_f64(k)),
        })
    }

    /// Whether `self` (a minimum) and `max` are of no samples: the minimum above the maximum, as
    /// [ChannelStats::empty] has them. NaN for floats rescales to NaN, so floats never count as empty here.
    fn no_samples(self, max: Self) -> bool {
        match (self, max) {
            (Self::I64(a), Self::I64(b)) => a > b,
            (Self::U64(a), Self::U64(b)) => a > b,
            _ => false,
        }
    }
}

/// Larger of two floats, NaN left out; the first on a tie, so the result is the same on every platform.
fn fmax(a: f64, b: f64) -> f64 {
    if a.is_nan() || b > a { b } else { a }
}

/// The statistics of one channel over a node, or over consecutive nodes merged.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ChannelStats {
    /// Per component (I, Q); only the first is used for a real element.
    pub min: [Value; 2],
    pub max: [Value; 2],
    pub sum: [f64; 2],
    pub sum_sq: [f64; 2],
    pub clipped: u64,
    pub flags: u32,
    pub power_mean: f32,
    pub power_max: f32,
}

impl ChannelStats {
    /// Statistics of no samples: what merging starts from, and what a block that doesn't hold a statistic reads as.
    pub fn empty(element: ElementDesc) -> Self {
        let (min, max) = match kind(element) {
            ValueKind::Signed => (Value::I64(i64::MAX), Value::I64(i64::MIN)),
            ValueKind::Unsigned => (Value::U64(u64::MAX), Value::U64(0)),
            ValueKind::Float => (Value::F64(f64::NAN), Value::F64(f64::NAN)),
        };
        Self {
            min: [min; 2],
            max: [max; 2],
            sum: [0.0; 2],
            sum_sq: [0.0; 2],
            clipped: 0,
            flags: 0,
            power_mean: 0.0,
            power_max: f32::NAN,
        }
    }

    /// Merge `next` (`next_samples` samples, right after this one's `samples`) into this one. Merging in item order,
    /// one node at a time, gives the same bits as the writer's parent node.
    pub fn merge(&mut self, next: &Self, samples: u64, next_samples: u64) {
        for (a, b) in self.min.iter_mut().zip(next.min) {
            *a = a.pick(b, true);
        }
        for (a, b) in self.max.iter_mut().zip(next.max) {
            *a = a.pick(b, false);
        }
        for (a, b) in self.sum.iter_mut().zip(next.sum) {
            *a += b;
        }
        for (a, b) in self.sum_sq.iter_mut().zip(next.sum_sq) {
            *a += b;
        }
        // a count of samples: a damaged block's count stops at u64::MAX instead of wrapping
        self.clipped = self.clipped.saturating_add(next.clipped);
        self.flags |= next.flags;
        // the counts as f64: exact up to 2^53 samples, so the mean is the same as with an integer total
        let (n, next_n) = (u64_f64(samples), u64_f64(next_samples));
        let total = n + next_n;
        if total > 0.0 {
            let a = f64::from(self.power_mean) * n;
            let b = f64::from(next.power_mean) * next_n;
            self.power_mean = f64_f32((a + b) / total);
        }
        self.power_max = f64_f32(fmax(f64::from(self.power_max), f64::from(next.power_max)));
    }

    /// The statistics with every stored number times `k`: from a scale to a scale `k` times finer. Minimum and
    /// maximum stay exact integers (an error past their 64-bit type), sums are multiplied by `k`, sums of squares
    /// and powers by `k²`; clipped counts and flags don't depend on the scale.
    pub fn rescaled(&self, k: u64) -> Result<Self, ScaleError> {
        if k == 1 {
            return Ok(*self);
        }
        if k > FACTOR_MAX {
            return Err(ScaleError::Overflow);
        }
        let mut s = *self;
        // k <= 2^53: exact as an f64
        let kf = u64_f64(k);
        for (min, max) in s.min.iter_mut().zip(&mut s.max) {
            if !min.no_samples(*max) {
                *min = min.times(k)?;
                *max = max.times(k)?;
            }
        }
        for v in &mut s.sum {
            *v *= kf;
        }
        for v in &mut s.sum_sq {
            *v *= kf * kf;
        }
        s.power_mean = f64_f32(f64::from(self.power_mean) * (kf * kf));
        s.power_max = f64_f32(f64::from(self.power_max) * (kf * kf));
        Ok(s)
    }

    /// Merge `next` (at `next_scale`, `next_samples` samples, right after this one's `samples` at `scale`) into this
    /// one at the common scale of the two ([common_scale]), which it returns. On an error this one is left as it was.
    pub fn merge_scaled(
        &mut self,
        scale: Magnitude,
        next: &Self,
        next_scale: Magnitude,
        samples: u64,
        next_samples: u64,
    ) -> Result<Magnitude, ScaleError> {
        let common = common_scale(scale, next_scale)?;
        let mut a = self.rescaled(scale_factor(scale, common)?)?;
        let b = next.rescaled(scale_factor(next_scale, common)?)?;
        a.merge(&b, samples, next_samples);
        *self = a;
        Ok(common)
    }
}

/// A node of a series: per channel its statistics, all at one scale.
#[cfg(feature = "alloc")]
#[derive(Clone, Debug, PartialEq)]
pub struct SeriesNode {
    /// What a stored number (minimum, maximum, sum) is worth in the sample type's unit; squares and powers are at
    /// its square.
    pub scale: Magnitude,
    /// One per channel.
    pub channels: Vec<ChannelStats>,
}

#[cfg(feature = "alloc")]
impl SeriesNode {
    /// The node at `scale`, which must be a whole fraction of its own ([scale_factor]). On an error it is left as it
    /// was.
    pub fn rescale(&mut self, scale: Magnitude) -> Result<(), ScaleError> {
        let k = scale_factor(self.scale, scale)?;
        if k != 1 {
            self.channels = self
                .channels
                .iter()
                .map(|c| c.rescaled(k))
                .collect::<Result<_, _>>()?;
        }
        self.scale = scale;
        Ok(())
    }

    /// Merge `next` (`next_samples` samples, right after this one's `samples`, as many channels) into this one, both
    /// at their [common_scale] first. On an error this one is left as it was.
    pub fn merge(
        &mut self,
        next: &Self,
        samples: u64,
        next_samples: u64,
    ) -> Result<(), ScaleError> {
        let common = common_scale(self.scale, next.scale)?;
        let mut b = next.clone();
        b.rescale(common)?;
        let mut a = self.clone();
        a.rescale(common)?;
        for (x, y) in a.channels.iter_mut().zip(&b.channels) {
            x.merge(y, samples, next_samples);
        }
        *self = a;
        Ok(())
    }
}

#[cfg(feature = "std")]
#[expect(
    private_interfaces,
    reason = "the sealed trait's add_channel takes crate types: no other crate can name or call it"
)]
mod scalar {
    use super::{
        ChannelStats, ElementDesc, SeriesStats, Value, components, f64_f32, fmax, i128_f64,
        is_complex, u64_f64,
    };
    use ww_tape::samples::Scalar as ScalarType;
    use ww_tape_codecs::kernel::{IntBatch, Kernel, Narrow, int_channel};

    /// A sample scalar the series writer takes: the Rust type a part of an element is read into
    /// ([ElementDesc::scalar]).
    pub trait Scalar: Copy + PartialOrd + 'static + private::Sealed {
        /// The scalar this type is.
        const SCALAR: ScalarType;
        /// The lowest and highest value: the default clip limits.
        const LOWEST: Self;
        const HIGHEST: Self;
        /// Whether squares sum exactly in i128 (integers up to 32 bits).
        const SQ_EXACT: bool;
        /// Whether the type is an integer: sums are exact in i128.
        const INT: bool;
        fn to_f64(self) -> f64;
        /// The value as i128; 0 for floats.
        fn to_i128(self) -> i128;
        fn value(self) -> Value;
        /// `v` rounded down (`up` false) or up, saturating; floats as they are.
        fn from_f64(v: f64, up: bool) -> Self;
        /// `v`, saturating for an integer, rounded to the nearest for a float.
        fn from_i128(v: i128) -> Self;
    }

    mod private {
        use super::{Acc, Channel, Kernel};

        pub trait Sealed: Sized {
            /// Add one channel's samples of `xs` to `acc` in one pass.
            fn add_channel(acc: &mut Acc<Self>, xs: &[Self], ch: Channel<Self>, kernel: Kernel);
        }
    }

    macro_rules! int_scalar {
        ($t:ty, $ty:ident, $kind:ident, $wide:ty, $exact:expr, $to_f64:path, $add:ident, $($lint:path),+) => {
            impl private::Sealed for $t {
                #[inline]
                fn add_channel(acc: &mut Acc<Self>, xs: &[Self], ch: Channel<Self>, kernel: Kernel) {
                    $add(acc, xs, ch, kernel);
                }
            }
            impl Scalar for $t {
                const SCALAR: ScalarType = ScalarType::$ty;
                const LOWEST: Self = <$t>::MIN;
                const HIGHEST: Self = <$t>::MAX;
                const SQ_EXACT: bool = $exact;
                const INT: bool = true;
                #[inline(always)]
                fn to_f64(self) -> f64 {
                    $to_f64(self)
                }
                #[inline(always)]
                fn to_i128(self) -> i128 {
                    i128::from(self)
                }
                fn value(self) -> Value {
                    Value::$kind(<$wide>::from(self))
                }
                #[expect($($lint),+, reason = "saturating, NaN to 0, is the meaning: what `as` does")]
                fn from_f64(v: f64, up: bool) -> Self {
                    if up { v.ceil() as $t } else { v.floor() as $t }
                }
                fn from_i128(v: i128) -> Self {
                    <$t>::try_from(v).unwrap_or(if v < 0 { <$t>::MIN } else { <$t>::MAX })
                }
            }
        };
    }

    int_scalar!(
        i8,
        I8,
        I64,
        i64,
        true,
        f64::from,
        narrow_add,
        clippy::cast_possible_truncation
    );
    int_scalar!(
        i16,
        I16,
        I64,
        i64,
        true,
        f64::from,
        narrow_add,
        clippy::cast_possible_truncation
    );
    int_scalar!(
        i32,
        I32,
        I64,
        i64,
        true,
        f64::from,
        narrow_add,
        clippy::cast_possible_truncation
    );
    int_scalar!(
        i64,
        I64,
        I64,
        i64,
        false,
        super::i64_f64,
        ordered_add,
        clippy::cast_possible_truncation
    );
    int_scalar!(
        u8,
        U8,
        U64,
        u64,
        true,
        f64::from,
        narrow_add,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    );
    int_scalar!(
        u16,
        U16,
        U64,
        u64,
        true,
        f64::from,
        narrow_add,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    );
    int_scalar!(
        u32,
        U32,
        U64,
        u64,
        true,
        f64::from,
        narrow_add,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    );
    int_scalar!(
        u64,
        U64,
        U64,
        u64,
        false,
        super::u64_f64,
        ordered_add,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    );

    macro_rules! float_scalar {
        ($t:ty, $ty:ident, $from_f64:path) => {
            impl private::Sealed for $t {
                #[inline]
                fn add_channel(
                    acc: &mut Acc<Self>,
                    xs: &[Self],
                    ch: Channel<Self>,
                    kernel: Kernel,
                ) {
                    ordered_add(acc, xs, ch, kernel);
                }
            }
            impl Scalar for $t {
                const SCALAR: ScalarType = ScalarType::$ty;
                const LOWEST: Self = <$t>::NEG_INFINITY;
                const HIGHEST: Self = <$t>::INFINITY;
                const SQ_EXACT: bool = false;
                const INT: bool = false;
                #[inline(always)]
                fn to_f64(self) -> f64 {
                    f64::from(self)
                }
                #[inline(always)]
                fn to_i128(self) -> i128 {
                    0
                }
                fn value(self) -> Value {
                    Value::F64(f64::from(self))
                }
                fn from_f64(v: f64, _up: bool) -> Self {
                    $from_f64(v)
                }
                fn from_i128(v: i128) -> Self {
                    $from_f64(i128_f64(v))
                }
            }
        };
    }

    float_scalar!(f32, F32, super::f64_f32);
    float_scalar!(f64, F64, core::convert::identity);

    /// Running statistics of one component of one channel over the open level-0 node.
    ///
    /// Integer sums are exact: a node holds at most `2^BASE_SHIFT_MAX` (2^24) samples, so a sum of 64-bit values stays
    /// under 2^88 and a sum of squares of values up to 32 bits under 2^88, far inside an i128; the wrapping ops never
    /// wrap.
    #[derive(Copy, Clone, Debug)]
    struct Part<T> {
        min: Option<T>,
        max: Option<T>,
        isum: i128,
        fsum: f64,
        isq: i128,
        fsq: f64,
    }

    impl<T: Scalar> Part<T> {
        const NEW: Self = Self {
            min: None,
            max: None,
            isum: 0,
            fsum: 0.0,
            isq: 0,
            fsq: 0.0,
        };
    }

    /// Where one channel's samples sit in a batch of interleaved frames, and what to keep of them.
    #[derive(Copy, Clone, Debug)]
    pub(crate) struct Channel<T> {
        /// Scalars per frame.
        pub(crate) stride: usize,
        /// The channel's first scalar in a frame.
        pub(crate) at: usize,
        /// Whether a sample is an I/Q pair.
        pub(crate) complex: bool,
        /// Clip limits: a component at or past one counts.
        pub(crate) lo: T,
        pub(crate) hi: T,
        pub(crate) stats: SeriesStats,
    }

    /// Add one channel's samples of `xs` to `acc` in one pass, through `kernel` for integers up to 32 bits.
    #[cfg(test)]
    pub(crate) fn add_channel<T: Scalar>(
        acc: &mut Acc<T>,
        xs: &[T],
        ch: Channel<T>,
        kernel: Kernel,
    ) {
        T::add_channel(acc, xs, ch, kernel);
    }

    /// The samples of `ch` in `xs`, `C` components each: frames too short for them are skipped.
    fn samples<T: Copy, const C: usize>(xs: &[T], ch: Channel<T>) -> impl Iterator<Item = &[T; C]> {
        xs.chunks_exact(ch.stride)
            .filter_map(move |f| f.get(ch.at..).and_then(|f| f.first_chunk::<C>()))
    }

    /// Add the samples of `ch` (integers up to 32 bits) through `kernel`: every statistic is exact, so the batch's
    /// totals are the same in any order of addition.
    fn narrow_add<T: Scalar + Narrow>(acc: &mut Acc<T>, xs: &[T], ch: Channel<T>, kernel: Kernel) {
        debug_assert!(T::INT && T::SQ_EXACT, "a narrow type's squares sum exactly");
        let mut b = IntBatch::NEW;
        if ch.complex {
            int_channel::<T, 2>(kernel, xs, ch.stride, ch.at, ch.lo, ch.hi, &mut b);
        } else {
            int_channel::<T, 1>(kernel, xs, ch.stride, ch.at, ch.lo, ch.hi, &mut b);
        }
        if b.frames == 0 {
            return;
        }
        let stats = ch.stats;
        let comps = if ch.complex { 2 } else { 1 };
        let batch = b.min.iter().zip(&b.max).zip(b.sum.iter().zip(&b.sq));
        for (part, ((&min, &max), (&sum, &sq))) in acc.parts.iter_mut().take(comps).zip(batch) {
            if stats.min || stats.max {
                part.min = Some(part.min.map_or(min, |m| m.min(min)));
                part.max = Some(part.max.map_or(max, |m| m.max(max)));
            }
            if stats.sum || stats.sum_sq {
                // under 2^88 and 2^89 over a node of 2^24 samples (see Part)
                part.isum = part.isum.wrapping_add(sum);
                part.isq = part.isq.wrapping_add(sq);
            }
        }
        if ch.complex && (stats.power_mean || stats.power_max) {
            acc.ipow = acc.ipow.wrapping_add(b.pow);
            // rounding is monotonic: the largest power rounded is the largest of the powers rounded
            acc.pmax = fmax(acc.pmax, i128_f64(b.pmax));
        }
        if stats.clipped {
            acc.clipped = acc.clipped.saturating_add(b.clipped);
        }
    }

    /// Add the samples of `ch` (64-bit integers and floats) one frame at a time, in their order: float sums depend on
    /// it. Per component, float sums and sums of squares add each sample to the node's running sum in sample order;
    /// the squares of a 64-bit integer add to a sum of the batch first, which then adds to the node's.
    fn ordered_add<T: Scalar>(acc: &mut Acc<T>, xs: &[T], ch: Channel<T>, _kernel: Kernel) {
        if ch.complex {
            ordered::<T, 2>(acc, xs, ch);
        } else {
            ordered::<T, 1>(acc, xs, ch);
        }
    }

    #[inline(always)]
    fn ordered<T: Scalar, const C: usize>(acc: &mut Acc<T>, xs: &[T], ch: Channel<T>) {
        let stats = ch.stats;
        let (minmax, sums) = (stats.min || stats.max, stats.sum || stats.sum_sq);
        let power = C == 2 && (stats.power_mean || stats.power_max);
        // squares of a 64-bit integer, summed over the batch
        let mut fq = [0.0f64; C];
        for x in samples::<T, C>(xs, ch) {
            for ((&x, part), fq) in x.iter().zip(&mut acc.parts).zip(&mut fq) {
                // NaN compares with nothing, itself included
                if minmax && x.partial_cmp(&x).is_some() {
                    if part.min.is_none_or(|m| x < m) {
                        part.min = Some(x);
                    }
                    if part.max.is_none_or(|m| x > m) {
                        part.max = Some(x);
                    }
                }
                if sums {
                    if T::INT {
                        // under 2^88 over a node (see Part)
                        part.isum = part.isum.wrapping_add(x.to_i128());
                        if T::SQ_EXACT {
                            let v = x.to_i128();
                            part.isq = part.isq.wrapping_add(v.wrapping_mul(v));
                        } else {
                            let f = x.to_f64();
                            *fq += f * f;
                        }
                    } else {
                        let f = x.to_f64();
                        part.fsum += f;
                        part.fsq += f * f;
                    }
                }
            }
            if power && let Some(&[i, q]) = x.first_chunk::<2>() {
                let p = if T::SQ_EXACT {
                    // values up to 32 bits: a power under 2^65, a node's sum under 2^89
                    let (i, q) = (i.to_i128(), q.to_i128());
                    let p = i.wrapping_mul(i).wrapping_add(q.wrapping_mul(q));
                    acc.ipow = acc.ipow.wrapping_add(p);
                    i128_f64(p)
                } else {
                    let (i, q) = (i.to_f64(), q.to_f64());
                    let p = i * i + q * q;
                    acc.fpow += p;
                    p
                };
                acc.pmax = fmax(acc.pmax, p);
            }
            if stats.clipped && x.iter().any(|&v| !(ch.lo < v && v < ch.hi)) {
                // at most a node's 2^24 samples
                acc.clipped = acc.clipped.saturating_add(1);
            }
        }
        if sums && T::INT {
            for (part, fq) in acc.parts.iter_mut().zip(fq) {
                part.fsq += fq;
            }
        }
    }

    /// Running statistics of one channel over the open level-0 node.
    #[derive(Copy, Clone, Debug)]
    pub(crate) struct Acc<T> {
        parts: [Part<T>; 2],
        /// Sum of the powers of an integer element: exact, as the sums of [Part]
        ipow: i128,
        fpow: f64,
        pmax: f64,
        clipped: u64,
        pub(super) flags: u32,
    }

    impl<T: Scalar> Acc<T> {
        pub(crate) const NEW: Self = Self {
            parts: [Part::NEW; 2],
            ipow: 0,
            fpow: 0.0,
            pmax: f64::NAN,
            clipped: 0,
            flags: 0,
        };

        pub(crate) fn stats(&self, element: ElementDesc, samples: u64) -> ChannelStats {
            let mut s = ChannelStats::empty(element);
            let slots = s
                .min
                .iter_mut()
                .zip(&mut s.max)
                .zip(s.sum.iter_mut().zip(&mut s.sum_sq));
            let parts = self.parts.iter().take(usize::from(components(element)));
            for (part, ((min, max), (sum, sum_sq))) in parts.zip(slots) {
                if let Some(v) = part.min {
                    *min = v.value();
                }
                if let Some(v) = part.max {
                    *max = v.value();
                }
                if T::INT {
                    *sum = i128_f64(part.isum);
                    *sum_sq = if T::SQ_EXACT {
                        i128_f64(part.isq)
                    } else {
                        part.fsq
                    };
                } else {
                    *sum = part.fsum;
                    *sum_sq = part.fsq;
                }
            }
            s.clipped = self.clipped;
            s.flags = self.flags;
            if is_complex(element) && samples > 0 {
                let p = if T::SQ_EXACT {
                    i128_f64(self.ipow)
                } else {
                    self.fpow
                };
                s.power_mean = f64_f32(p / u64_f64(samples));
                s.power_max = f64_f32(self.pmax);
            }
            s
        }
    }
}
#[cfg(feature = "std")]
pub use scalar::Scalar;
#[cfg(all(test, feature = "std"))]
pub(crate) use scalar::{Acc, Channel, add_channel};

#[cfg(feature = "std")]
pub use builder::{
    SERIES_BLOCK_BYTES, SeriesLeaf, SeriesLeaves, SeriesStart, SeriesSummaries, SeriesTree,
};

#[cfg(feature = "std")]
mod builder {
    use exact_units::{Bytes, Magnitude};
    use ww_tape::ItemNum;

    use super::i128_f64;
    use super::scalar::{Acc, Channel};
    use super::{
        ChannelStats, Scalar, SeriesNode, SeriesSummarySpec, common_scale, components, is_complex,
    };
    use crate::levels::{BuildError, LevelNode, Levels, Output};
    use crate::{Layout, SCALE_LEN_MAX, SummaryHead, Tree, summary_len, write_series_summary};
    use ww_tape::samples::Sample;
    use ww_tape::{
        ElementDesc, FlagRuns, Ratio64, SampleShape, SeriesError, SeriesGrid, SeriesTimeline,
        TrackId, TrackNum,
    };
    use ww_tape_codecs::kernel::Kernel;

    impl LevelNode for SeriesNode {
        fn merge(&mut self, next: &Self, items: u64, next_items: u64) -> Result<(), BuildError> {
            Self::merge(self, next, items, next_items).map_err(BuildError::Scale)
        }
    }

    /// Builds the summary tree of one series track as samples come, and encodes its summary blocks.
    ///
    /// Samples go in as interleaved frames ([Self::push]): per sample every channel, per channel every component.
    /// A grid ([Self::set_grid]) may change the scale of the numbers from the next sample on; a node holding
    /// samples of two scales is kept at their [common_scale](super::common_scale), so every node's minimum and maximum
    /// are exact in the sample type's unit. Finished blocks wait in [Self::take_blocks] for
    /// [TapeWriter::defer_block](ww_tape::TapeWriter::defer_block); [Self::finish] writes the partial nodes at the end.
    /// After an error the summaries are no longer whole: drop them.
    ///
    /// Every statistic is kept in one pass over a batch. Integers up to 32 bits sum exactly, so their results don't
    /// depend on how samples are split into pushes; they run through vectorised kernels chosen for the CPU at
    /// [Self::new]. Float sums and sums of squares add each sample in sample order, the squares of 64-bit integers
    /// sum per push first: the same bits for the same pushes, NaN payloads aside.
    ///
    /// An element with a [zero](ElementDesc::zero) `p/q` is summarised as `q · sample - p` in its
    /// [summary element](ElementDesc::summary_element), at the grid's scale over `q`: 8-bit IQ offset to 127.5 keeps
    /// exact minimum, maximum and sums in an i16. Its samples go in by [Self::push_from].
    ///
    /// It is the two halves a writer may also run on two threads: [SeriesLeaves], the pass over the samples on the
    /// producer's thread, and [SeriesTree], the levels above and the blocks on the writing thread.
    pub struct SeriesSummaries<T: Scalar> {
        leaves: SeriesLeaves<T>,
        tree: SeriesTree,
    }

    /// Where a series starts: its track, its samples and the grid of its first sample.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct SeriesStart {
        /// [TrackDef::id](ww_tape::TrackDef::id).
        pub track_id: TrackId,
        /// [TrackDef::number](ww_tape::TrackDef::number).
        pub track_number: TrackNum,
        /// The samples' element ([TrackDef::series_shape](ww_tape::TrackDef::series_shape)) and channels, at most
        /// 65535.
        pub shape: SampleShape,
        /// Index of the first sample in the track (a later tape piece starts past 0).
        pub first_item: ItemNum,
        /// The grid and scale of the first sample, as its chunk's head carries them.
        pub grid: SeriesGrid,
        /// Ticks from one sample to the next, exact.
        pub period: Ratio64,
    }

    /// Default most bytes of a series summary block: small enough to fit the gap before an aligned chunk payload.
    pub const SERIES_BLOCK_BYTES: Bytes<u32> = Bytes::new(2048);

    /// `v` shifted by a zero `(p, q)`: `q · v - p`, saturating (a valid element's values never saturate).
    /// Push each stretch of `samples` (frames of `stride` scalars) that one flags word of `runs` covers, with that
    /// word: how a chunk's samples go in with its flag runs ([Chunk::flag_runs](ww_tape::Chunk::flag_runs)), as the
    /// writer pushed them. Refused when the samples are not whole frames or the runs reach past them.
    fn by_runs<X>(
        samples: &[X],
        stride: usize,
        runs: &FlagRuns<'_>,
        mut push: impl FnMut(&[X], u32) -> Result<(), BuildError>,
    ) -> Result<(), BuildError> {
        let partial = BuildError::PartialFrame { stride };
        if stride == 0 || !samples.len().is_multiple_of(stride) {
            return Err(partial);
        }
        let past = BuildError::Invalid("flag runs past the samples");
        let frames = samples.len().checked_div(stride).ok_or(partial)?;
        let count = u32::try_from(frames).map_err(|_e| past)?;
        if runs
            .get(runs.len().saturating_sub(1))
            .is_some_and(|r| r.end().is_none_or(|end| end > count))
        {
            return Err(past);
        }
        for (span, flags) in runs.spans(count) {
            let at = |i: u32| {
                usize::try_from(i)
                    .ok()
                    .and_then(|i| i.checked_mul(stride))
                    .ok_or(past)
            };
            let part = samples.get(at(span.start)?..at(span.end)?).ok_or(past)?;
            push(part, flags)?;
        }
        Ok(())
    }

    fn shift_by(shift: Option<(i128, i128)>, v: i128) -> i128 {
        match shift {
            Some((p, q)) => q.saturating_mul(v).saturating_sub(p),
            None => v,
        }
    }

    /// The low `bits` of `raw` as an integer, sign-extended when `signed`.
    fn as_i128(raw: u64, bits: u32, signed: bool) -> i128 {
        let unused = 64u32.saturating_sub(bits);
        if signed {
            i128::from((raw << unused).cast_signed() >> unused)
        } else {
            i128::from(raw)
        }
    }

    /// The scale of statistics of samples at `scale`: over `q` for a zero `(p, q)`.
    fn stat_scale(shift: Option<(i128, i128)>, scale: Magnitude) -> Result<Magnitude, BuildError> {
        let Some((_, q)) = shift else {
            return Ok(scale);
        };
        // a magnitude error says only that the result is out of range, which this error says
        u64::try_from(q)
            .ok()
            .and_then(|q| Magnitude::checked_ratio(1, q).ok())
            .and_then(|r| scale.checked_mul(r).ok())
            .ok_or(BuildError::Invalid(
                "scale over the zero's denominator out of a magnitude's range",
            ))
    }

    /// What a series and its summaries declare, checked once.
    #[derive(Copy, Clone)]
    struct Declared {
        /// The element the statistics are kept in
        element: ElementDesc,
        /// The stored element's zero `(p, q)`: the statistics hold `q · v - p`
        shift: Option<(i128, i128)>,
        /// The stored element
        stored: ElementDesc,
        channels: u16,
        /// Scalars per sample of one channel: 1, or 2 for a complex element
        components: usize,
        /// Scalars per frame: channels times components
        stride: usize,
        /// Bytes of a node in a block
        node_len: usize,
        /// `2^base_shift - 1`: an item's place in its level-0 node
        node_mask: u64,
        spec: SeriesSummarySpec,
    }

    impl Declared {
        /// The declaration of `shape` and `spec`, for samples of scalar `scalar` when given.
        fn of(
            shape: SampleShape,
            spec: SeriesSummarySpec,
            scalar: Option<ww_tape::samples::Scalar>,
        ) -> Result<Self, BuildError> {
            let stored = shape.element;
            if !stored.is_valid() {
                return Err(BuildError::Invalid("an element no writer takes"));
            }
            if stored.parts.0 > 2 {
                return Err(BuildError::Invalid("an element of more than 2 parts"));
            }
            let element = stored.summary_element();
            if scalar.is_some_and(|s| s != element.scalar()) {
                return Err(BuildError::Invalid(
                    "element is not the samples' scalar type",
                ));
            }
            let channels = u16::try_from(shape.channels)
                .ok()
                .ok_or(BuildError::Invalid("more than 65535 channels"))?;
            if channels == 0 {
                return Err(BuildError::Invalid("a series has no channels"));
            }
            if !(SeriesSummarySpec::BASE_SHIFT_MIN..=SeriesSummarySpec::BASE_SHIFT_MAX)
                .contains(&spec.base_shift)
            {
                return Err(BuildError::Invalid("base shift out of range"));
            }
            if spec.stats.is_empty() {
                return Err(BuildError::Invalid("no statistics"));
            }
            if (spec.stats.power_mean || spec.stats.power_max) && !is_complex(element) {
                return Err(BuildError::Invalid("power of a real element"));
            }
            let too_wide = BuildError::Invalid("a frame or node past usize");
            let components = usize::from(components(element));
            let stride = usize::from(channels)
                .checked_mul(components)
                .ok_or(too_wide)?;
            let node_len = spec
                .stats
                .node_len(element)
                .checked_mul(usize::from(channels))
                .ok_or(too_wide)?;
            Ok(Self {
                element,
                shift: stored.zero_shift(),
                stored,
                channels,
                components,
                stride,
                node_len,
                // base_shift is at most BASE_SHIFT_MAX, checked above
                node_mask: !(u64::MAX << spec.base_shift),
                spec,
            })
        }

        /// The level-0 node of sample `item`.
        fn node_of(&self, item: u64) -> u64 {
            item >> self.spec.base_shift
        }
    }

    /// A level-0 node of a series, whole or a part of it, as [SeriesLeaves] computes it from the samples.
    #[derive(Clone, Debug, PartialEq)]
    pub struct SeriesLeaf {
        /// Its first sample.
        pub first_item: ItemNum,
        /// Its samples: 1 up to the node's.
        pub samples: u64,
        /// Its statistics, at the scale of its samples.
        pub node: SeriesNode,
    }

    /// The pass over a series' samples: the statistics of each level-0 node, on the thread that has the samples.
    ///
    /// Samples go in as [SeriesSummaries::push] takes them. A node that fills is a whole [SeriesLeaf]; [Self::seal]
    /// ends the samples so far as a part, at the end of a chunk. The leaves go, in order, to a [SeriesTree], which
    /// merges the parts of a node and builds the levels above.
    pub struct SeriesLeaves<T: Scalar> {
        d: Declared,
        lo: T,
        hi: T,
        /// Scale of the samples from [Self::acc_from] on
        scale: Magnitude,
        /// Common scale of every sample so far
        common: Magnitude,
        /// The open part's samples since [Self::acc_from]
        acc: Vec<Acc<T>>,
        acc_from: u64,
        /// The open part's samples before [Self::acc_from], at an earlier scale, and their count
        part: Option<(SeriesNode, u64)>,
        next_item: u64,
        leaves: Vec<SeriesLeaf>,
        /// The code the integer statistics run through
        kernel: Kernel,
    }

    impl<T: Scalar> SeriesLeaves<T> {
        /// The pass over samples of `shape` from `first_item` on, at a grid's `scale`, kept as `spec` declares.
        pub fn new(
            shape: SampleShape,
            scale: Magnitude,
            first_item: ItemNum,
            spec: SeriesSummarySpec,
        ) -> Result<Self, BuildError> {
            let d = Declared::of(shape, spec, Some(T::SCALAR))?;
            let shift = d.shift;
            let shifted = |v: f64| match shift {
                Some((p, q)) => i128_f64(q) * v - i128_f64(p),
                None => v,
            };
            let (lo, hi) = match (spec.clip, d.stored.range()) {
                (Some(c), _) => (
                    T::from_f64(shifted(c.low), false),
                    T::from_f64(shifted(c.high), true),
                ),
                // an integer narrower than its scalar clips at its own ends
                (None, Some((low, high))) => (
                    T::from_i128(shift_by(shift, low)),
                    T::from_i128(shift_by(shift, high)),
                ),
                (None, None) => (T::LOWEST, T::HIGHEST),
            };
            let scale = stat_scale(shift, scale)?;
            Ok(Self {
                d,
                lo,
                hi,
                scale,
                common: scale,
                acc: vec![Acc::NEW; usize::from(d.channels)],
                acc_from: first_item.get(),
                part: None,
                next_item: first_item.get(),
                leaves: Vec::new(),
                kernel: Kernel::best(),
            })
        }

        /// The next sample's index.
        pub fn next_item(&self) -> ItemNum {
            ItemNum::new(self.next_item)
        }

        /// The scale of the statistics: the grid's scale, over `q` for an element with a zero `p/q`.
        pub fn scale(&self) -> Magnitude {
            self.scale
        }

        /// Take the samples from the next one on at a grid's `scale`. Refused when it has no common scale with the
        /// scales before.
        pub fn set_scale(&mut self, scale: Magnitude) -> Result<(), BuildError> {
            let scale = stat_scale(self.d.shift, scale)?;
            let common = common_scale(self.common, scale).map_err(BuildError::Scale)?;
            if scale != self.scale {
                self.flush_acc()?;
                self.scale = scale;
                self.common = common;
            }
            Ok(())
        }

        /// Add samples, as [SeriesSummaries::push] takes them.
        pub fn push(&mut self, samples: &[T], flags: u32) -> Result<(), BuildError> {
            let stride = self.d.stride;
            let partial = BuildError::PartialFrame { stride };
            if stride == 0 || !samples.len().is_multiple_of(stride) {
                return Err(partial);
            }
            let past = BuildError::Invalid("sample index past u64");
            let mut rest = samples;
            while !rest.is_empty() {
                // the samples left in the open node: 1 to 2^BASE_SHIFT_MAX
                let to_boundary = (!self.next_item & self.d.node_mask)
                    .checked_add(1)
                    .ok_or(past)?;
                // a boundary past usize is farther than any slice
                let to_boundary = usize::try_from(to_boundary).unwrap_or(usize::MAX);
                let frames = rest
                    .len()
                    .checked_div(stride)
                    .ok_or(partial)?
                    .min(to_boundary);
                let (now, later) = rest
                    .split_at_checked(frames.checked_mul(stride).ok_or(partial)?)
                    .ok_or(partial)?;
                self.add(now, flags);
                self.next_item = u64::try_from(frames)
                    .ok()
                    .and_then(|n| self.next_item.checked_add(n))
                    .ok_or(past)?;
                rest = later;
                if self.next_item & self.d.node_mask == 0 {
                    self.seal()?;
                }
            }
            Ok(())
        }

        /// Add samples flagged by `runs`, as [SeriesSummaries::push_runs] takes them.
        pub fn push_runs(&mut self, samples: &[T], runs: &FlagRuns<'_>) -> Result<(), BuildError> {
            by_runs(samples, self.d.stride, runs, |x, f| self.push(x, f))
        }

        /// Add stored samples flagged by `runs`, as [SeriesSummaries::push_from_runs] takes them.
        pub fn push_from_runs<S: Sample>(
            &mut self,
            samples: &[S],
            runs: &FlagRuns<'_>,
        ) -> Result<(), BuildError> {
            by_runs(samples, self.d.stride, runs, |x, f| self.push_from(x, f))
        }

        /// Add stored samples of the element's own scalar, as [SeriesSummaries::push_from] takes them.
        pub fn push_from<S: Sample>(
            &mut self,
            samples: &[S],
            flags: u32,
        ) -> Result<(), BuildError> {
            let stored = self.d.stored.scalar();
            if S::SCALAR != stored {
                return Err(BuildError::Invalid(
                    "samples of another scalar than the element's",
                ));
            }
            if S::SCALAR.is_float() {
                return Err(BuildError::Invalid("float samples go in by push"));
            }
            let bits = S::SCALAR.bits();
            let signed = S::SCALAR.is_signed();
            let shift = self.d.shift;
            let values: Vec<T> = samples
                .iter()
                .map(|s| T::from_i128(shift_by(shift, as_i128(s.to_bits(), bits, signed))))
                .collect();
            self.push(&values, flags)
        }

        fn add(&mut self, xs: &[T], flags: u32) {
            let comps = self.d.components;
            let stride = self.d.stride;
            // channel `ch` starts at scalar ch · comps of every frame: the channels' starts step by comps
            let starts = (0..stride).step_by(comps);
            for (acc, at) in self.acc.iter_mut().zip(starts) {
                let ch = Channel {
                    stride,
                    at,
                    complex: is_complex(self.d.element),
                    lo: self.lo,
                    hi: self.hi,
                    stats: self.d.spec.stats,
                };
                T::add_channel(acc, xs, ch, self.kernel);
                acc.flags |= flags;
            }
        }

        /// Run the integer kernels as `kernel` from now on.
        #[cfg(test)]
        pub(crate) fn set_kernel(&mut self, kernel: Kernel) {
            self.kernel = kernel;
        }

        /// The open part's samples since the last change of scale, at their scale, and their count.
        fn acc_node(&self) -> Result<(SeriesNode, u64), BuildError> {
            let samples = self
                .next_item
                .checked_sub(self.acc_from)
                .ok_or(BuildError::Invalid("open node starts past the next sample"))?;
            let channels = self
                .acc
                .iter()
                .map(|a| a.stats(self.d.element, samples))
                .collect();
            Ok((
                SeriesNode {
                    scale: self.scale,
                    channels,
                },
                samples,
            ))
        }

        /// The open part so far: the part before the last change of scale merged with the samples since; None
        /// when it holds no sample.
        pub fn open_node(&self) -> Result<Option<SeriesNode>, BuildError> {
            let (node, samples) = self.acc_node()?;
            Ok(match (&self.part, samples) {
                (None, 0) => None,
                (None, _) => Some(node),
                (Some((p, _)), 0) => Some(p.clone()),
                (Some((p, n)), _) => {
                    let mut p = p.clone();
                    p.merge(&node, *n, samples).map_err(BuildError::Scale)?;
                    Some(p)
                }
            })
        }

        /// Move the samples since the last change of scale into the open part's earlier part.
        fn flush_acc(&mut self) -> Result<(), BuildError> {
            let (node, samples) = self.acc_node()?;
            if samples > 0 {
                self.part = Some(match self.part.take() {
                    None => (node, samples),
                    Some((mut p, n)) => {
                        p.merge(&node, n, samples).map_err(BuildError::Scale)?;
                        let n = n
                            .checked_add(samples)
                            .ok_or(BuildError::Invalid("open node past u64 samples"))?;
                        (p, n)
                    }
                });
                self.acc.fill(Acc::NEW);
            }
            self.acc_from = self.next_item;
            Ok(())
        }

        /// End the open part as a leaf, whole when its node filled: at the end of a chunk, and at the end.
        pub fn seal(&mut self) -> Result<(), BuildError> {
            self.flush_acc()?;
            if let Some((node, samples)) = self.part.take() {
                let first_item = self
                    .next_item
                    .checked_sub(samples)
                    .ok_or(BuildError::Invalid("open node starts past the next sample"))?;
                self.leaves.push(SeriesLeaf {
                    first_item: ItemNum::new(first_item),
                    samples,
                    node,
                });
            }
            Ok(())
        }

        /// The leaves so far, in order, for [SeriesTree::push_leaf].
        pub fn take_leaves(&mut self) -> Vec<SeriesLeaf> {
            core::mem::take(&mut self.leaves)
        }
    }

    /// The levels of a series' summary tree above its leaves, and its blocks: on the writing thread.
    ///
    /// [Self::samples] adds the samples a chunk holds, on the grid set last ([Self::set_grid]); [Self::push_leaf]
    /// takes the [SeriesLeaf]s of those samples in order and merges the parts of a node. Finished blocks wait in
    /// [Self::take_blocks]; [Self::finish] writes the partial nodes at the end.
    pub struct SeriesTree {
        track_id: TrackId,
        track_number: TrackNum,
        d: Declared,
        origin: u64,
        timeline: SeriesTimeline,
        /// Grid of the next samples
        grid: SeriesGrid,
        /// One past the last sample added
        next_item: u64,
        /// One past the last sample of the leaves taken
        leaves_end: u64,
        /// The node whose parts came so far: its number, merged parts, samples
        open: Option<(u64, SeriesNode, u64)>,
        levels: Levels<SeriesNode>,
        blocks: Vec<Vec<u8>>,
    }

    impl SeriesTree {
        /// The tree of the series `start` describes, kept as `spec` declares.
        pub fn new(start: SeriesStart, spec: SeriesSummarySpec) -> Result<Self, BuildError> {
            let d = Declared::of(start.shape, spec, None)?;
            if start.grid.grid_index > start.first_item.get() {
                return Err(BuildError::Series(SeriesError::GridAfterFirst {
                    first_item: start.first_item,
                    grid_index: start.grid.grid_index,
                }));
            }
            let first = start.first_item.get();
            Ok(Self {
                track_id: start.track_id,
                track_number: start.track_number,
                d,
                origin: first,
                timeline: SeriesTimeline::new(start.period),
                grid: start.grid,
                next_item: first,
                leaves_end: first,
                open: None,
                levels: Levels::new(
                    first,
                    spec.base_shift,
                    d.node_len,
                    crate::block_room(SERIES_BLOCK_BYTES).saturating_sub(SCALE_LEN_MAX),
                ),
                blocks: Vec::new(),
            })
        }

        /// Keep blocks within `bytes` (default [SERIES_BLOCK_BYTES]); call before the first sample.
        pub fn set_max_block_bytes(&mut self, bytes: Bytes<u32>) {
            self.levels = Levels::new(
                self.origin,
                self.d.spec.base_shift,
                self.d.node_len,
                crate::block_room(bytes).saturating_sub(SCALE_LEN_MAX),
            );
        }

        /// The next sample's index.
        pub fn next_item(&self) -> ItemNum {
            ItemNum::new(self.next_item)
        }

        /// The grids of the samples so far.
        pub fn timeline(&self) -> &SeriesTimeline {
            &self.timeline
        }

        /// Put the samples from the next one on grid `g`, as [SeriesSummaries::set_grid] says; the scale is the
        /// leaves' to check.
        pub fn set_grid(&mut self, g: SeriesGrid) -> Result<(), BuildError> {
            self.check_grid(g)?;
            self.grid = g;
            Ok(())
        }

        fn check_grid(&self, g: SeriesGrid) -> Result<(), BuildError> {
            let next = ItemNum::new(self.next_item);
            let first = g
                .chunk_ticks(self.timeline.period(), next, 1)
                .map_err(BuildError::Series)?
                .0;
            if !g.same_grid(&self.grid)
                && let Some(last) = self
                    .next_item
                    .checked_sub(1)
                    .and_then(|i| self.timeline.tick(ItemNum::new(i)))
                && !ww_tape::series::is_gap(self.timeline.period(), last, first)
            {
                return Err(BuildError::Series(SeriesError::GridTooClose {
                    first_item: next,
                }));
            }
            Ok(())
        }

        /// Add the next `count` samples, on the grid set last.
        pub fn samples(&mut self, count: u64) -> Result<(), BuildError> {
            let mut rest = count;
            while rest > 0 {
                let n = u32::try_from(rest).unwrap_or(u32::MAX);
                self.timeline
                    .push(ItemNum::new(self.next_item), n, self.grid)
                    .map_err(BuildError::Series)?;
                self.next_item = self
                    .next_item
                    .checked_add(u64::from(n))
                    .ok_or(BuildError::Invalid("sample index past u64"))?;
                rest = rest.saturating_sub(u64::from(n));
            }
            Ok(())
        }

        /// Take the next leaf: it starts where the last one ended and lies in the samples added.
        pub fn push_leaf(&mut self, leaf: SeriesLeaf) -> Result<(), BuildError> {
            let SeriesLeaf {
                first_item,
                samples,
                node,
            } = leaf;
            let first = first_item.get();
            if first != self.leaves_end || samples == 0 {
                return Err(BuildError::Invalid("a leaf out of order"));
            }
            let end = first
                .checked_add(samples)
                .filter(|e| *e <= self.next_item)
                .ok_or(BuildError::Invalid("a leaf past the samples added"))?;
            let idx = self.d.node_of(first);
            let last = end.checked_sub(1).ok_or(BuildError::Invalid("no sample"))?;
            if self.d.node_of(last) != idx {
                return Err(BuildError::Invalid("a leaf across a level-0 node"));
            }
            let open = match self.open.take() {
                Some((open_idx, mut merged, n)) if open_idx == idx => {
                    merged.merge(&node, n, samples).map_err(BuildError::Scale)?;
                    let n = n
                        .checked_add(samples)
                        .ok_or(BuildError::Invalid("open node past u64 samples"))?;
                    (idx, merged, n)
                }
                // a leaf that starts its node: the leaves before ended the node before at its boundary
                _ => (idx, node, samples),
            };
            self.leaves_end = end;
            if end & self.d.node_mask == 0 {
                self.levels.push(0, idx, open.1, end)?;
                self.encode()?;
            } else {
                self.open = Some(open);
            }
            Ok(())
        }

        /// Write the open nodes of every level, partial ones included, as blocks. Call it once, after the last
        /// leaf.
        pub fn finish(&mut self) -> Result<(), BuildError> {
            if let Some((idx, node, _)) = self.open.take() {
                self.levels.push(0, idx, node, self.leaves_end)?;
            }
            self.levels.finish(self.leaves_end)?;
            self.encode()
        }

        fn tick(&self, item: u64) -> Result<i64, BuildError> {
            self.timeline
                .tick(ItemNum::new(item))
                .ok_or(BuildError::Invalid("sample tick past i64"))
        }

        fn encode(&mut self) -> Result<(), BuildError> {
            for out in self.levels.take_output() {
                let Output {
                    level,
                    first_item,
                    items,
                    mut nodes,
                } = out;
                // one scale per block: the common scale of its nodes
                let mut scale = nodes
                    .first()
                    .map(|n| n.scale)
                    .ok_or(BuildError::Invalid("a batch holds nodes"))?;
                for n in &nodes {
                    scale = common_scale(scale, n.scale).map_err(BuildError::Scale)?;
                }
                for n in &mut nodes {
                    n.rescale(scale).map_err(BuildError::Scale)?;
                }
                let last = items
                    .checked_sub(1)
                    .and_then(|n| first_item.checked_add(n))
                    .ok_or(BuildError::Invalid("a batch of no items"))?;
                let last_tick = self.tick(last)?;
                let head = SummaryHead {
                    track_id: self.track_id,
                    track_number: self.track_number,
                    tree: Tree::Series,
                    level,
                    node_shift: self.levels.shift(level)?,
                    layout: Layout::Series {
                        element: self.d.element,
                        channels: self.d.channels,
                        stats: self.d.spec.stats,
                        scale,
                    },
                    first_item: ItemNum::new(first_item),
                    items,
                    first_tick: self.tick(first_item)?,
                    last_tick,
                    latest_end: last_tick,
                };
                let refs: Vec<&[ChannelStats]> =
                    nodes.iter().map(|n| n.channels.as_slice()).collect();
                let mut buf = vec![0u8; summary_len(&head)?];
                let len = write_series_summary(
                    &mut buf,
                    ww_tape::BlockId::new(ww_tape::SessionTag::NONE, 0),
                    &head,
                    &refs,
                )?;
                buf.truncate(len);
                self.blocks.push(buf);
            }
            Ok(())
        }

        /// Finished summary blocks, oldest first, for
        /// [TapeWriter::defer_block](ww_tape::TapeWriter::defer_block).
        pub fn take_blocks(&mut self) -> Vec<Vec<u8>> {
            core::mem::take(&mut self.blocks)
        }

        /// What a live reader takes from memory for `level`: the number of the first node not in a block yet, and the
        /// nodes from it on, each at its own scale; at level 0 the last may be the parts of a node taken so far.
        pub fn pending(&self, level: u8) -> (u64, Vec<SeriesNode>) {
            let (mut first, mut nodes) = self.levels.pending(level);
            if level == 0
                && let Some((idx, node, _)) = &self.open
            {
                if nodes.is_empty() {
                    first = *idx;
                }
                nodes.push(node.clone());
            }
            (first, nodes)
        }
    }

    impl<T: Scalar> SeriesSummaries<T> {
        /// Summaries of the series `start` describes, kept as `spec` declares.
        pub fn new(start: SeriesStart, spec: SeriesSummarySpec) -> Result<Self, BuildError> {
            let leaves = SeriesLeaves::new(start.shape, start.grid.scale, start.first_item, spec)?;
            let tree = SeriesTree::new(start, spec)?;
            Ok(Self { leaves, tree })
        }

        /// Keep blocks within `bytes` (default [SERIES_BLOCK_BYTES]); call before the first sample.
        pub fn set_max_block_bytes(&mut self, bytes: Bytes<u32>) {
            self.tree.set_max_block_bytes(bytes);
        }

        /// The next sample's index.
        pub fn next_item(&self) -> ItemNum {
            self.leaves.next_item()
        }

        /// The grids of the samples so far.
        pub fn timeline(&self) -> &SeriesTimeline {
            self.tree.timeline()
        }

        /// Put the samples from the next one ([Self::next_item]) on grid `g`: a new grid (a gap, a restart) or the
        /// same grid at a new scale (a prefix change), as the next chunk's head carries it. Refused when the grid
        /// starts after the next sample, a new grid's first sample isn't more than a period after the last sample's
        /// tick ([is_gap](ww_tape::series::is_gap)), or the scale has no common scale with the ones before.
        pub fn set_grid(&mut self, g: SeriesGrid) -> Result<(), BuildError> {
            self.tree.check_grid(g)?;
            self.leaves.set_scale(g.scale)?;
            self.tree.grid = g;
            Ok(())
        }

        /// Add samples: whole frames of `channels × components` scalars, interleaved; `flags` ORs into the nodes they
        /// fall in. Samples that are not whole frames are refused.
        pub fn push(&mut self, samples: &[T], flags: u32) -> Result<(), BuildError> {
            let frames = self.frames(samples.len())?;
            self.tree.samples(frames)?;
            self.leaves.push(samples, flags)?;
            self.take_leaves()
        }

        /// Add stored samples of the element's own scalar `S` (whole frames, as [Self::push] takes them), each
        /// `v` as `q · v - p` for the element's [zero](ElementDesc::zero) `p/q`: how the samples of an element
        /// with a zero, or of a scalar narrower than its summary element, go in. Floats go in by [Self::push].
        pub fn push_from<S: Sample>(
            &mut self,
            samples: &[S],
            flags: u32,
        ) -> Result<(), BuildError> {
            let frames = self.frames(samples.len())?;
            self.tree.samples(frames)?;
            self.leaves.push_from(samples, flags)?;
            self.take_leaves()
        }

        /// Add the samples of a chunk (whole frames, as [Self::push] takes them) flagged as its flag runs say
        /// ([Chunk::flag_runs](ww_tape::Chunk::flag_runs)): the summaries a writer built of them, rebuilt from the
        /// chunk alone. Refused when the runs reach past the samples.
        pub fn push_runs(&mut self, samples: &[T], runs: &FlagRuns<'_>) -> Result<(), BuildError> {
            by_runs(samples, self.leaves.d.stride, runs, |x, f| self.push(x, f))
        }

        /// [Self::push_runs] of stored samples of the element's own scalar, as [Self::push_from] takes them.
        pub fn push_from_runs<S: Sample>(
            &mut self,
            samples: &[S],
            runs: &FlagRuns<'_>,
        ) -> Result<(), BuildError> {
            by_runs(samples, self.leaves.d.stride, runs, |x, f| {
                self.push_from(x, f)
            })
        }

        /// The frames in `scalars` scalars; refused when they are not whole frames.
        fn frames(&self, scalars: usize) -> Result<u64, BuildError> {
            let stride = self.leaves.d.stride;
            let partial = BuildError::PartialFrame { stride };
            if stride == 0 || !scalars.is_multiple_of(stride) {
                return Err(partial);
            }
            scalars
                .checked_div(stride)
                .and_then(|n| u64::try_from(n).ok())
                .ok_or(partial)
        }

        fn take_leaves(&mut self) -> Result<(), BuildError> {
            for leaf in self.leaves.take_leaves() {
                self.tree.push_leaf(leaf)?;
            }
            Ok(())
        }

        /// Run the integer kernels as `kernel` from now on.
        #[cfg(test)]
        pub(crate) fn set_kernel(&mut self, kernel: Kernel) {
            self.leaves.set_kernel(kernel);
        }

        /// Write the open nodes of every level, partial ones included, as blocks. Call it once, after the last
        /// sample.
        pub fn finish(&mut self) -> Result<(), BuildError> {
            self.leaves.seal()?;
            self.take_leaves()?;
            self.tree.finish()
        }

        /// The scale of the statistics: the grid's scale, over `q` for an element with a zero `p/q`.
        pub fn scale(&self) -> Magnitude {
            self.leaves.scale()
        }

        /// Finished summary blocks, oldest first, for
        /// [TapeWriter::defer_block](ww_tape::TapeWriter::defer_block).
        pub fn take_blocks(&mut self) -> Vec<Vec<u8>> {
            self.tree.take_blocks()
        }

        /// What a live reader takes from memory for `level`: the number of the first node not in a block yet, and the
        /// nodes from it on, each at its own scale; the last may be partial (the open node).
        pub fn pending(&self, level: u8) -> Result<(u64, Vec<SeriesNode>), BuildError> {
            let (mut first, mut nodes) = self.tree.pending(level);
            if level == 0
                && let Some(open) = self.leaves.open_node()?
            {
                if nodes.is_empty() {
                    let last = self
                        .leaves
                        .next_item
                        .checked_sub(1)
                        .ok_or(BuildError::Invalid("an open node of no samples"))?;
                    first = self.leaves.d.node_of(last);
                }
                nodes.push(open);
            }
            Ok((first, nodes))
        }
    }
}
