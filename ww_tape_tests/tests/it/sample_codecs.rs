//! The code of the book page "Samples and their codecs" (`docs/sample_codecs.md`), which includes the marked
//! sections of this file: the counts of a converter as fixed point and as floats under ALP.

use ww_tape::samples::{encode, max_encoded_len};
use ww_tape::{Codec, Sample, SampleShape, SeriesValue};

// --8<-- [start:coded]
/// The bytes `values` take in `codec`.
fn coded<T: Sample + SeriesValue>(codec: Codec, values: &[T]) -> usize {
    let shape = SampleShape::scalar(T::ELEMENT);
    let mut out = vec![0; max_encoded_len(codec, shape, values.len()).expect("a valid shape")];
    encode(codec, shape, values, &mut out).expect("the values code")
}
// --8<-- [end:coded]

/// Fixed-point counts code smaller than the same values as floats under ALP, and ALP undoes only a short decimal scale.
#[test]
fn fixed_point_against_alp() {
    // --8<-- [start:fixed_against_alp]
    // A slow triangle wave with a little noise, as a 12-bit converter counts it.
    let mut noise = 1u32;
    let counts: Vec<i16> = (0..4096)
        .map(|i: i16| {
            noise = noise.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            500 + (i % 1024 - 512).abs() * 3 + (noise >> 29) as i16
        })
        .collect();
    let tenths: Vec<f64> = counts.iter().map(|&c| f64::from(c) * 0.1).collect();
    let volts: Vec<f64> = counts
        .iter()
        .map(|&c| f64::from(c) * 3.3 / 4096.0)
        .collect();

    assert_eq!(coded(Codec::DELTA, &counts), 2336); // fixed point
    assert_eq!(coded(Codec::ALP, &tenths), 5152); // a short decimal scale
    assert_eq!(coded(Codec::ALP, &volts), 26848); // a scale ALP cannot undo
    assert_eq!(coded(Codec::RAW, &volts), 32768); // 8 bytes a sample
    // --8<-- [end:fixed_against_alp]
}
