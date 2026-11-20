//! Series summaries of `ww_tape` against the samples they summarise: MB/s of sample bytes on one core, per element
//! type, through the RAW sample codec alone (no summaries), `SeriesSummaries::push` alone with every statistic, and
//! both. Then the single-pass kernel alone against a summariser that keeps one statistic at a time, one pass each:
//! MB/s per element type for each kernel this CPU runs (portable, AVX2, AVX-512). Prints Markdown tables with the
//! load average around each.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::indexing_slicing,
    clippy::print_stdout,
    reason = "a bench reads no file of others: it computes sizes and rates freely, fails by panicking and prints its table"
)]

use clap::Parser;
use std::hint::black_box;
use std::time::Instant;
use vc_ui::Paint;
use ww_tape::samples::{encode, max_encoded_len};
use ww_tape::{Codec, Element, ElementDesc, Magnitude, Ratio64, Sample, SampleShape, SeriesGrid};
use ww_tape_bench::{Cell, Common, Machine, Report, Table};
use ww_tape_codecs::kernel::{IntBatch, Kernel, Narrow};
use ww_tape_summary::{Scalar, SeriesStart, SeriesStats, SeriesSummaries, SeriesSummarySpec};

#[derive(Parser)]
#[command(
    name = "summaries",
    about = "Series summaries of ww_tape: MB/s per element type with and without summaries",
    version = vc_build::version!(),
    after_long_help = "Examples:
  just bench-summaries
  just bench-summaries --frames 1024 --mib 64
  summaries --json docs/bench/summaries.json"
)]
struct Args {
    #[command(flatten)]
    common: Common,
    /// Frames handed over per push, as a driver's buffer.
    #[arg(long, default_value_t = 16 * 1024)]
    frames: usize,
    /// MiB of samples per timed pass.
    #[arg(long, default_value_t = 256)]
    mib: usize,
    /// Timed passes per case; the fastest counts.
    #[arg(long, default_value_t = 5)]
    repeats: usize,
    /// log2 of a level-0 node's samples.
    #[arg(long, default_value_t = 10)]
    base_shift: u8,
}

impl AsRef<Common> for Args {
    fn as_ref(&self) -> &Common {
        &self.common
    }
}

/// A sample type the bench generates: ADC-like noise, small steps around zero.
trait Gen: Scalar + Sample {
    fn from_noise(v: i32) -> Self;
}

macro_rules! gen_int {
    ($($t:ty),+) => {$(
        impl Gen for $t {
            fn from_noise(v: i32) -> Self {
                // the noise is a 16-bit value: truncated for 8 bits, offset for unsigned types
                let v = if <$t>::MIN == 0 { v.wrapping_add(1 << 15) } else { v };
                v as $t
            }
        }
    )+};
}
gen_int!(i8, i16, i32, i64, u8, u16, u32, u64);

impl Gen for f32 {
    fn from_noise(v: i32) -> Self {
        v as Self / 1024.0
    }
}

impl Gen for f64 {
    fn from_noise(v: i32) -> Self {
        Self::from(v) / 1024.0
    }
}

/// `n` values of a seeded random walk in 16 bits.
fn noise<T: Gen>(n: usize) -> Vec<T> {
    let mut x = 0x2545_f491_4f6c_dd1du64;
    let mut acc = 0i32;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            acc = (acc + (x & 0x3ff) as i32 - 512).clamp(-32768, 32767);
            T::from_noise(acc)
        })
        .collect()
}

/// What a case runs per push.
#[derive(Copy, Clone)]
enum Work {
    Raw,
    Summaries,
    Both,
}

/// MB/s of sample bytes for `work` on `mib` MiB of `batch` pushed again and again; the fastest of `repeats` passes.
fn rate<T: Gen>(args: &Args, shape: SampleShape, batch: &[T], work: Work) -> anyhow::Result<f64> {
    let frames = batch.len() / (shape.element.parts.0 as usize * shape.channels as usize);
    let mut out = vec![
        0u8;
        max_encoded_len(Codec::RAW, shape, frames)
            .map_err(|e| anyhow::anyhow!("{e:?}"))?
    ];
    let bytes = args.mib << 20;
    let pushes = bytes.div_ceil(size_of_val(batch));
    let spec = SeriesSummarySpec {
        base_shift: args.base_shift,
        stats: SeriesStats::all(shape.element),
        clip: None,
    };
    let start = SeriesStart {
        track_id: 1,
        track_number: 1,
        shape,
        first_item: 0,
        grid: SeriesGrid::start(0, 0, Magnitude::ONE),
        period: Ratio64::ONE,
    };
    let mut best = f64::MAX;
    for _ in 0..args.repeats {
        let mut s = SeriesSummaries::<T>::new(start, spec).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let t = Instant::now();
        for _ in 0..pushes {
            if matches!(work, Work::Raw | Work::Both) {
                black_box(
                    encode(Codec::RAW, shape, black_box(batch), &mut out)
                        .map_err(|e| anyhow::anyhow!("{e:?}"))?,
                );
            }
            if matches!(work, Work::Summaries | Work::Both) {
                s.push(black_box(batch), 0)
                    .map_err(|e| anyhow::anyhow!("{e:?}"))?;
                black_box(s.take_blocks());
            }
        }
        best = best.min(t.elapsed().as_secs_f64());
    }
    Ok((pushes * size_of_val(batch)) as f64 / best / 1e6)
}

fn case<T: Gen>(
    args: &Args,
    table: &mut Table,
    element: Element,
    channels: u32,
) -> anyhow::Result<()> {
    let shape = SampleShape::new(ElementDesc::of(element), channels);
    let batch: Vec<T> = noise(args.frames * element.parts() as usize * channels as usize);
    let raw = rate(args, shape, &batch, Work::Raw)?;
    let sum = rate(args, shape, &batch, Work::Summaries)?;
    let both = rate(args, shape, &batch, Work::Both)?;
    table.row(vec![
        Cell::text(format!("{element:?}")),
        Cell::count(u64::from(channels)),
        Cell::num(raw, 0),
        Cell::num(sum, 0),
        Cell::num(both, 0),
    ]);
    Ok(())
}

/// Every statistic of `frames` (`C` components each) one at a time, one pass each, as a summariser without the
/// single-pass kernel does: minimum, maximum, sum and sum of squares per component, the clipped count, and for complex
/// frames the sum and maximum of `I² + Q²`. Returns a value of all of them, so none is optimised away.
fn one_at_a_time<T: Narrow + Into<i64>, const C: usize>(frames: &[[T; C]], lo: T, hi: T) -> i128 {
    let mut acc = 0i128;
    for c in 0..C {
        let x = |f: &[T; C]| f[c].into();
        acc += i128::from(frames.iter().map(x).min().unwrap_or(0));
        acc += i128::from(frames.iter().map(x).max().unwrap_or(0));
        acc += frames.iter().map(|f| i128::from(x(f))).sum::<i128>();
        acc += frames.iter().map(|f| i128::from(x(f) * x(f))).sum::<i128>();
    }
    acc += frames
        .iter()
        .filter(|f| f.iter().any(|&v| v <= lo || v >= hi))
        .count() as i128;
    if C == 2 {
        let pow = |f: &[T; C]| i128::from(f[0].into()).pow(2) + i128::from(f[1].into()).pow(2);
        acc += frames.iter().map(pow).sum::<i128>();
        acc += frames.iter().map(pow).max().unwrap_or(0);
    }
    acc
}

/// MB/s of sample bytes of `f` over nodes of `2^base_shift` frames of `batch`, `mib` MiB per pass, the fastest of
/// `repeats` passes.
fn node_rate<T, const C: usize>(
    args: &Args,
    batch: &[[T; C]],
    mut f: impl FnMut(&[[T; C]]),
) -> f64 {
    let node = 1usize << args.base_shift;
    let bytes = args.mib << 20;
    let passes = bytes.div_ceil(size_of_val(batch));
    let mut best = f64::MAX;
    for _ in 0..args.repeats {
        let t = Instant::now();
        for _ in 0..passes {
            for n in batch.chunks(node) {
                f(black_box(n));
            }
        }
        best = best.min(t.elapsed().as_secs_f64());
    }
    (passes * size_of_val(batch)) as f64 / best / 1e6
}

/// The kernels and the one-at-a-time summariser on frames of `C` components of `T`.
fn kernels<T: Gen + Narrow + Into<i64>, const C: usize>(
    args: &Args,
    table: &mut Table,
    name: &str,
) {
    let flat: Vec<T> = noise(args.frames * C);
    let (batch, _) = flat.as_chunks::<C>();
    let (lo, hi) = (T::LOW, T::HIGH);
    let mut row = vec![
        Cell::text(name),
        Cell::num(
            node_rate(args, batch, |n| {
                black_box(one_at_a_time(n, lo, hi));
            }),
            0,
        ),
    ];
    for kernel in [KernelName::Portable, KernelName::Avx2, KernelName::Avx512] {
        row.push(match kernel.get() {
            Some(k) => Cell::num(
                node_rate(args, batch, |n| {
                    let mut out = IntBatch::<T>::NEW;
                    k.int_batch(n, lo, hi, &mut out);
                    black_box(out);
                }),
                0,
            ),
            None => Cell::text("not on this CPU"),
        });
    }
    table.row(row);
}

/// A kernel by the name the table gives it.
#[derive(Copy, Clone)]
enum KernelName {
    Portable,
    Avx2,
    Avx512,
}

impl KernelName {
    /// The kernel, None when this CPU doesn't run it.
    fn get(self) -> Option<Kernel> {
        Kernel::all().into_iter().find(|k| match (self, k) {
            (Self::Portable, Kernel::Portable) => true,
            #[cfg(target_arch = "x86_64")]
            (Self::Avx2, Kernel::Avx2(_)) | (Self::Avx512, Kernel::Avx512(_)) => true,
            _ => false,
        })
    }
}

fn main() -> anyhow::Result<()> {
    let args: Args = ww_tape_bench::args();
    anyhow::ensure!(
        args.frames > 0 && args.repeats > 0,
        "frames and repeats must be positive"
    );
    let paint = Paint::stdout();
    let mut report = Report::new(
        "summaries",
        vc_build::version!(),
        Machine::this(),
        vec![
            ("frames a push", args.frames.to_string()),
            ("MiB a pass", args.mib.to_string()),
            ("best of", args.repeats.to_string()),
            ("node", format!("2^{} samples", args.base_shift)),
        ],
    );
    println!("{}", report.header(paint));
    let mut t = report.table(
        "summaries",
        &format!(
            "Series summaries, every statistic, nodes of 2^{} samples, {} frames a push, {} MiB a pass, best of {}.",
            args.base_shift, args.frames, args.mib, args.repeats
        ),
        &[
            "Element",
            "Channels",
            "RAW MB/s",
            "Summaries MB/s",
            "RAW and summaries MB/s",
        ],
    );
    case::<i8>(&args, &mut t, Element::I8, 1)?;
    case::<u8>(&args, &mut t, Element::U8, 1)?;
    case::<i16>(&args, &mut t, Element::I16, 1)?;
    case::<i16>(&args, &mut t, Element::I16, 4)?;
    case::<u16>(&args, &mut t, Element::U16, 1)?;
    case::<i32>(&args, &mut t, Element::I32, 1)?;
    case::<u32>(&args, &mut t, Element::U32, 1)?;
    case::<i64>(&args, &mut t, Element::I64, 1)?;
    case::<u64>(&args, &mut t, Element::U64, 1)?;
    case::<f32>(&args, &mut t, Element::F32, 1)?;
    case::<f64>(&args, &mut t, Element::F64, 1)?;
    case::<i16>(&args, &mut t, Element::ComplexI16, 1)?;
    case::<i16>(&args, &mut t, Element::ComplexI16, 4)?;
    case::<f32>(&args, &mut t, Element::ComplexF32, 1)?;
    println!("{}", report.push(t, paint));

    let mut k = report.table(
        "kernels",
        &format!(
            "Every statistic of a node of 2^{} frames: one at a time, one pass each, against the single-pass kernel, \
             {} MiB a pass, best of {}.",
            args.base_shift, args.mib, args.repeats
        ),
        &[
            "Element",
            "One at a time MB/s",
            "Portable kernel MB/s",
            "AVX2 kernel MB/s",
            "AVX-512 kernel MB/s",
        ],
    );
    kernels::<i16, 1>(&args, &mut k, "I16");
    kernels::<i16, 2>(&args, &mut k, "ComplexI16");
    kernels::<i32, 1>(&args, &mut k, "I32");
    println!("{}", report.push(k, paint));
    report.finish(args.common.json.as_deref())
}
