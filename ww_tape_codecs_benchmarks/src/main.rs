//! `ww_tape_codecs_benchmarks`: the float sample codec of `ww_tape_codecs` (ALP) and the bench-only float codecs (Gorilla, Chimp,
//! delta on ordered float bits) on simulated, decoded and sensor data, against the same quantities stored as
//! fixed-point integers with `DELTA`. Prints a Markdown table: ratio to the raw floats, and encode and decode time per
//! value on one core.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::print_stderr,
    clippy::print_stdout,
    reason = "a bench reads no file of others: it computes sizes and rates freely, fails by panicking and prints its table"
)]

mod data;
mod float_delta;
mod xor;

use anyhow::Context;
use clap::Parser;
use data::{Columns, DataSet};
use std::hint::black_box;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::time::Instant;
use ww_tape_codecs::Codec;
use ww_tape_codecs::samples::{Sample, Samples, Scalar, encode, max_encoded_len};

/// What a column holds: values of a scalar, `lanes` of them per sample, interleaved.
#[derive(Copy, Clone, Debug)]
struct Shape {
    scalar: Scalar,
    lanes: NonZeroUsize,
}

#[derive(Parser)]
#[command(about = "Float sample codecs of ww_tape_codecs on motor_sim, speech and ADC data")]
struct Args {
    /// The speech recording (Ogg Vorbis); `just bench-sample-codecs` fetches it. Without it the set is skipped.
    #[arg(long)]
    speech: Option<PathBuf>,
    /// Samples per chunk: each chunk is one column, coded alone.
    #[arg(long, default_value_t = 4096)]
    chunk: usize,
    /// Timed passes over each set; the fastest counts.
    #[arg(long, default_value_t = 5)]
    repeats: usize,
}

/// A codec the bench runs: one of ww_tape's, or a bench-only float codec.
#[derive(Copy, Clone, Debug)]
enum Coder {
    Tape(Codec),
    Xor(xor::Flavor),
    /// ww_tape's `DELTA` or `DELTA2` on the floats' bits as ordered integers.
    FloatDelta(Codec),
}

/// The float codecs, in table order after the fixed-point row.
const FLOAT_CODECS: [(Coder, &str); 5] = [
    (Coder::Xor(xor::Flavor::Gorilla), "GORILLA"),
    (Coder::Xor(xor::Flavor::Chimp), "CHIMP"),
    (Coder::FloatDelta(Codec::DELTA), "FLOAT_DELTA"),
    (Coder::FloatDelta(Codec::DELTA2), "FLOAT_DELTA2"),
    (Coder::Tape(Codec::ALP), "ALP"),
];

/// `values` as unsigned integers of the floats' width: their ordered bits.
fn ordered<T: Sample, U: Sample>(values: &[T], out: &mut Vec<U>) {
    let w = T::SCALAR.bits();
    out.clear();
    out.extend(
        values
            .iter()
            .map(|v| U::from_bits(float_delta::to_ordered(v.to_bits(), w))),
    );
}

/// Scratch buffers of one coder, sized for a chunk.
struct Scratch {
    u32s: Vec<u32>,
    u64s: Vec<u64>,
}

impl Coder {
    fn max_len(self, shape: Shape, count: usize) -> anyhow::Result<usize> {
        let w = shape.scalar.bits();
        let lanes = shape.lanes;
        let n = match self {
            Self::Tape(c) => max_encoded_len(c, shape.scalar, lanes, count)
                .map_err(|e| anyhow::anyhow!("{e:?}"))?,
            Self::Xor(_) => xor::max_len(count, count * lanes.get(), w).context("too long")?,
            Self::FloatDelta(c) => max_encoded_len(c, int_scalar(shape.scalar)?, lanes, count)
                .map_err(|e| anyhow::anyhow!("{e:?}"))?,
        };
        Ok(n)
    }

    fn encode<T: Sample>(
        self,
        shape: Shape,
        values: &[T],
        out: &mut [u8],
        s: &mut Scratch,
    ) -> anyhow::Result<usize> {
        let e = |e| anyhow::anyhow!("{e:?}");
        let lanes = shape.lanes;
        match self {
            Self::Tape(c) => encode(c, lanes, values, out).map_err(e),
            Self::Xor(f) => Ok(xor::encode(f, values, lanes.get(), out)),
            Self::FloatDelta(c) => {
                if int_scalar(shape.scalar)? == Scalar::U32 {
                    ordered(values, &mut s.u32s);
                    encode(c, lanes, &s.u32s, out).map_err(e)
                } else {
                    ordered(values, &mut s.u64s);
                    encode(c, lanes, &s.u64s, out).map_err(e)
                }
            }
        }
    }

    fn decode<T: Sample>(
        self,
        shape: Shape,
        bytes: &[u8],
        out: &mut [T],
        s: &mut Scratch,
    ) -> anyhow::Result<()> {
        let e = |e| anyhow::anyhow!("{e:?}");
        let lanes = shape.lanes;
        let count = out.len() / lanes;
        match self {
            Self::Tape(c) => Samples::new(c, shape.scalar, lanes, count, bytes)
                .and_then(|col| col.decode(0, out))
                .map_err(e),
            Self::Xor(f) => {
                xor::decode(f, bytes, count, lanes.get(), 0, out).context("corrupt column")
            }
            Self::FloatDelta(c) => {
                let int = int_scalar(shape.scalar)?;
                let w = shape.scalar.bits();
                let col = Samples::new(c, int, lanes, count, bytes).map_err(e)?;
                if int == Scalar::U32 {
                    s.u32s.resize(out.len(), 0);
                    col.decode(0, &mut s.u32s).map_err(e)?;
                    for (o, v) in out.iter_mut().zip(&s.u32s) {
                        *o = T::from_bits(float_delta::from_ordered(u64::from(*v), w));
                    }
                } else {
                    s.u64s.resize(out.len(), 0);
                    col.decode(0, &mut s.u64s).map_err(e)?;
                    for (o, v) in out.iter_mut().zip(&s.u64s) {
                        *o = T::from_bits(float_delta::from_ordered(*v, w));
                    }
                }
                Ok(())
            }
        }
    }
}

/// The unsigned integer scalar of a float scalar's bits.
fn int_scalar(scalar: Scalar) -> anyhow::Result<Scalar> {
    Ok(match scalar {
        Scalar::F32 => Scalar::U32,
        Scalar::F64 => Scalar::U64,
        s => anyhow::bail!("delta on ordered float bits takes f32 or f64, not {s}"),
    })
}

/// One codec on one set.
struct Run {
    bytes: usize,
    encode_ns: f64,
    decode_ns: f64,
}

/// Code every chunk of `cols` and decode it back, checking every bit; the fastest of `repeats` passes.
fn run<T: Sample>(
    codec: Coder,
    shape: Shape,
    cols: &[Vec<T>],
    chunk: usize,
    repeats: usize,
) -> anyhow::Result<Run> {
    let lanes = shape.lanes.get();
    let pieces: Vec<&[T]> = cols.iter().flat_map(|c| c.chunks(chunk * lanes)).collect();
    let values: usize = pieces.iter().map(|p| p.len()).sum();
    let mut coded: Vec<Vec<u8>> = pieces
        .iter()
        .map(|p| Ok(vec![0u8; codec.max_len(shape, p.len() / lanes)?]))
        .collect::<anyhow::Result<_>>()?;
    let mut scratch = Scratch {
        u32s: Vec::with_capacity(chunk * lanes),
        u64s: Vec::with_capacity(chunk * lanes),
    };
    let mut lens = vec![0usize; pieces.len()];
    let mut encode_ns = f64::MAX;
    for _ in 0..repeats {
        let start = Instant::now();
        for ((p, out), len) in pieces.iter().zip(&mut coded).zip(&mut lens) {
            *len = codec.encode(shape, p, out, &mut scratch)?;
        }
        encode_ns = encode_ns.min(start.elapsed().as_nanos() as f64 / values as f64);
    }
    let mut back = vec![T::default(); chunk * lanes];
    let mut decode_ns = f64::MAX;
    for _ in 0..repeats {
        let start = Instant::now();
        for ((p, out), len) in pieces.iter().zip(&coded).zip(&lens) {
            codec.decode(shape, &out[..*len], &mut back[..p.len()], &mut scratch)?;
            black_box(&back);
        }
        decode_ns = decode_ns.min(start.elapsed().as_nanos() as f64 / values as f64);
    }
    for ((p, out), len) in pieces.iter().zip(&coded).zip(&lens) {
        codec.decode(shape, &out[..*len], &mut back[..p.len()], &mut scratch)?;
        anyhow::ensure!(
            back.iter()
                .zip(p.iter())
                .all(|(a, b)| a.to_bits() == b.to_bits()),
            "{codec:?} didn't give the values back"
        );
    }
    Ok(Run {
        bytes: lens.iter().sum(),
        encode_ns,
        decode_ns,
    })
}

/// Values and raw bytes of the floats of a set.
fn raw(cols: &Columns) -> (usize, usize) {
    let (values, bytes) = match cols {
        Columns::F32(c) => (c.iter().map(Vec::len).sum::<usize>(), 4),
        Columns::F64(c) => (c.iter().map(Vec::len).sum::<usize>(), 8),
        Columns::I16(c) => (c.iter().map(Vec::len).sum::<usize>(), 2),
        Columns::I32(c) => (c.iter().map(Vec::len).sum::<usize>(), 4),
    };
    (values, values * bytes)
}

fn run_cols(codec: Coder, channels: u32, cols: &Columns, a: &Args) -> anyhow::Result<Run> {
    let lanes = usize::try_from(channels)
        .ok()
        .and_then(NonZeroUsize::new)
        .context("a set has channels")?;
    let shape = |scalar| Shape { scalar, lanes };
    match cols {
        Columns::F32(c) => run(codec, shape(Scalar::F32), c, a.chunk, a.repeats),
        Columns::F64(c) => run(codec, shape(Scalar::F64), c, a.chunk, a.repeats),
        Columns::I16(c) => run(codec, shape(Scalar::I16), c, a.chunk, a.repeats),
        Columns::I32(c) => run(codec, shape(Scalar::I32), c, a.chunk, a.repeats),
    }
}

fn row(set: &str, codec: &str, raw_bytes: usize, values: usize, r: &Run) {
    println!(
        "| {set} | {codec} | {:.2} | {:.2} | {:.1} | {:.2} |",
        raw_bytes as f64 / r.bytes as f64,
        r.bytes as f64 * 8.0 / values as f64,
        r.encode_ns,
        r.decode_ns
    );
}

fn bench(set: &DataSet, a: &Args) -> anyhow::Result<()> {
    let (values, raw_bytes) = raw(&set.floats);
    let fixed = run_cols(Coder::Tape(Codec::DELTA), set.channels, &set.fixed, a)?;
    row(
        &set.name,
        &format!("DELTA of {}", set.fixed_note),
        raw_bytes,
        values,
        &fixed,
    );
    for (codec, name) in FLOAT_CODECS {
        let r = run_cols(codec, set.channels, &set.floats, a)?;
        row(&set.name, name, raw_bytes, values, &r);
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let a = Args::parse();
    println!(
        "sample codecs, {} samples per chunk, best of {} passes, {} kernel {:?}",
        a.chunk,
        a.repeats,
        std::env::consts::ARCH,
        ww_tape_codecs::bitpack::Kernel::best()
    );
    println!();
    println!("| Data | Codec | Ratio | Bits/value | Encode ns/value | Decode ns/value |");
    println!("|---|---|---:|---:|---:|---:|");
    let mut sets: Vec<DataSet> = data::motor().into();
    match &a.speech {
        Some(p) => sets.push(data::speech(p)?),
        None => eprintln!("speech: skipped, no --speech file"),
    }
    sets.extend(data::adc());
    for s in &sets {
        bench(s, &a)?;
    }
    Ok(())
}
