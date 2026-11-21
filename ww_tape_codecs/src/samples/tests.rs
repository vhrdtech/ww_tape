use super::*;

/// xorshift64*, a fixed seed per test.
pub(crate) struct Rng(pub(crate) u64);

impl Rng {
    pub(crate) fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}

/// The lane and the lanes of a column.
#[derive(Copy, Clone, Debug)]
pub(crate) struct Shape(pub(crate) Lane, pub(crate) NonZeroUsize);

/// `lanes` values of `scalar`, each filling it, per sample.
pub(crate) const fn sh(scalar: Scalar, lanes: usize) -> Shape {
    shl(Lane::of(scalar), lanes)
}

/// `lanes` values of `lane` per sample.
pub(crate) const fn shl(lane: Lane, lanes: usize) -> Shape {
    match NonZeroUsize::new(lanes) {
        Some(l) => Shape(lane, l),
        None => panic!("a column has lanes"),
    }
}

/// `bits` of `scalar`, LSB or MSB first.
pub(crate) const fn lane(scalar: Scalar, bits: u32, msb_first: bool) -> Lane {
    Lane {
        scalar,
        bits,
        msb_first,
        exponent_bits: 0,
    }
}

/// A float of `bits` with `exponent_bits`, in `scalar`.
pub(crate) const fn float_lane(
    scalar: Scalar,
    bits: u32,
    exponent_bits: u32,
    msb_first: bool,
) -> Lane {
    Lane {
        scalar,
        bits,
        msb_first,
        exponent_bits,
    }
}

pub(crate) fn read_column(
    codec: Codec,
    shape: Shape,
    count: usize,
    bytes: &[u8],
) -> Result<Samples<'_>, SampleError> {
    Samples::new(codec, shape.0, shape.1, count, bytes)
}

pub(crate) fn max_len(codec: Codec, shape: Shape, count: usize) -> Result<usize, SampleError> {
    max_encoded_len(codec, shape.0, shape.1, count)
}

/// [encode] of `values`, whose scalar must be the shape's.
pub(crate) fn enc<T: Sample>(
    codec: Codec,
    shape: Shape,
    values: &[T],
    out: &mut [u8],
) -> Result<usize, SampleError> {
    assert_eq!(
        T::SCALAR,
        shape.0.scalar,
        "the values are the shape's scalar"
    );
    encode(codec, shape.0, shape.1, values, out)
}

pub(crate) fn encode_vec<T: Sample>(codec: Codec, shape: Shape, values: &[T]) -> Vec<u8> {
    let mut out = vec![0u8; max_len(codec, shape, values.len() / shape.1).unwrap()];
    let n = enc(codec, shape, values, &mut out).unwrap();
    out.truncate(n);
    out
}

/// Encode, check the column, decode the whole and a few ranges back.
pub(crate) fn roundtrip<T: Sample>(codec: Codec, shape: Shape, values: &[T]) -> Vec<u8> {
    let lanes = shape.1.get();
    let count = values.len() / lanes;
    let bytes = encode_vec(codec, shape, values);
    let s = read_column(codec, shape, count, &bytes)
        .unwrap_or_else(|e| panic!("{codec:?} {shape:?}: {e:?}"));
    let mut out = vec![T::default(); values.len()];
    s.decode(0, &mut out)
        .unwrap_or_else(|e| panic!("{codec:?} {shape:?} decode: {e:?}"));
    assert!(
        out.iter()
            .zip(values)
            .all(|(a, b)| a.to_bits() == b.to_bits()),
        "{codec:?} {shape:?}"
    );
    for (first, n) in [
        (0, 1),
        (count / 3, count / 2),
        (count.saturating_sub(5), 5),
        (1023, 3),
        (1020, 1030),
    ] {
        if first + n > count || n == 0 {
            continue;
        }
        let mut part = vec![T::default(); n * lanes];
        s.decode(first, &mut part).unwrap();
        assert!(
            part.iter()
                .zip(&values[first * lanes..])
                .all(|(a, b)| a.to_bits() == b.to_bits()),
            "{codec:?} range {first}+{n}"
        );
    }
    bytes
}

#[test]
fn packed12_bytes() {
    let shape = sh(Scalar::U16, 1);
    let v: [u16; 3] = [0xabc, 0x123, 0xfff];
    assert_eq!(
        encode_vec(Codec::PACKED12_LSB, shape, &v),
        [0xbc, 0x3a, 0x12, 0xff, 0x0f]
    );
    assert_eq!(
        encode_vec(Codec::PACKED12_MSB, shape, &v),
        [0xab, 0xc1, 0x23, 0xff, 0xf0]
    );
    let s: [i16; 2] = [-1, -2048];
    assert_eq!(
        encode_vec(Codec::PACKED12_LSB, sh(Scalar::I16, 1), &s),
        [0xff, 0x0f, 0x80]
    );
    roundtrip(Codec::PACKED12_LSB, sh(Scalar::I16, 1), &s);
    roundtrip(Codec::PACKED12_MSB, sh(Scalar::I16, 1), &s);
}

#[test]
fn packed24_and_big_endian_bytes() {
    let v: [i32; 2] = [-2, 0x123456];
    let shape = sh(Scalar::I32, 1);
    assert_eq!(
        encode_vec(Codec::PACKED24_LSB, shape, &v),
        [0xfe, 0xff, 0xff, 0x56, 0x34, 0x12]
    );
    assert_eq!(
        encode_vec(Codec::PACKED24_MSB, shape, &v),
        [0xff, 0xff, 0xfe, 0x12, 0x34, 0x56]
    );
    assert_eq!(
        encode_vec(Codec::BIG_ENDIAN, shape, &v[1..]),
        [0x00, 0x12, 0x34, 0x56]
    );
    assert_eq!(
        encode_vec(Codec::RAW, shape, &v[1..]),
        [0x56, 0x34, 0x12, 0x00]
    );
    let b = [true, false, true, true, false, false, false, false, true];
    assert_eq!(
        encode_vec(Codec::RAW, sh(Scalar::Bool, 1), &b),
        [0b0000_1101, 0b1]
    );
    roundtrip(Codec::RAW, sh(Scalar::Bool, 1), &b);
}

#[test]
fn packed_refuses_wide_values() {
    let shape = sh(Scalar::I16, 1);
    let mut out = [0u8; 16];
    assert_eq!(
        enc(Codec::PACKED12_LSB, shape, &[0i16, 2048], &mut out),
        Err(SampleError::ValueTooWide { index: 1 })
    );
    assert_eq!(
        enc(
            Codec::PACKED10_MSB,
            sh(Scalar::U16, 1),
            &[1024u16],
            &mut out
        ),
        Err(SampleError::ValueTooWide { index: 0 })
    );
}

/// A lane of fewer bits than its scalar packs at its width under [Codec::RAW], the same bytes the packed codecs give
/// for that width; a whole-byte width `msb_first` is big endian; a 1-bit unsigned lane is a `Bool` column.
#[test]
fn described_lanes_pack_at_their_width() {
    let v: [u16; 3] = [0xabc, 0x123, 0xfff];
    let lsb12 = shl(lane(Scalar::U16, 12, false), 1);
    let msb12 = shl(lane(Scalar::U16, 12, true), 1);
    assert_eq!(
        encode_vec(Codec::RAW, lsb12, &v),
        encode_vec(Codec::PACKED12_LSB, sh(Scalar::U16, 1), &v)
    );
    assert_eq!(
        encode_vec(Codec::RAW, msb12, &v),
        encode_vec(Codec::PACKED12_MSB, sh(Scalar::U16, 1), &v)
    );
    let i: [i32; 2] = [-2, 0x123456];
    assert_eq!(
        encode_vec(Codec::RAW, shl(lane(Scalar::I32, 24, true), 1), &i),
        [0xff, 0xff, 0xfe, 0x12, 0x34, 0x56]
    );
    assert_eq!(
        encode_vec(Codec::RAW, shl(lane(Scalar::I32, 32, true), 1), &i[1..]),
        encode_vec(Codec::BIG_ENDIAN, sh(Scalar::I32, 1), &i[1..])
    );
    let bits: [u8; 9] = [1, 0, 1, 1, 0, 0, 0, 0, 1];
    assert_eq!(
        encode_vec(Codec::RAW, shl(lane(Scalar::U8, 1, false), 1), &bits),
        [0b0000_1101, 0b1]
    );
    let mut out = [0u8; 8];
    assert_eq!(
        enc(Codec::RAW, lsb12, &[0u16, 4096], &mut out),
        Err(SampleError::ValueTooWide { index: 1 })
    );
    assert_eq!(
        enc(
            Codec::RAW,
            shl(lane(Scalar::I16, 12, true), 1),
            &[2048i16],
            &mut out
        ),
        Err(SampleError::ValueTooWide { index: 0 })
    );
    let mut r = Rng(5);
    for n in [1usize, 2, 7, 128, 129, 1000] {
        for (bits, msb) in [
            (3, false),
            (3, true),
            (12, true),
            (17, false),
            (17, true),
            (33, false),
            (48, false),
            (48, true),
            (56, true),
            (63, false),
        ] {
            let signed: Vec<i64> = (0..n).map(|_| (r.next() as i64) >> (64 - bits)).collect();
            let unsigned: Vec<u64> = (0..2 * n).map(|_| r.next() >> (64 - bits)).collect();
            roundtrip(Codec::RAW, shl(lane(Scalar::I64, bits, msb), 1), &signed);
            roundtrip(Codec::RAW, shl(lane(Scalar::U64, bits, msb), 2), &unsigned);
        }
        let iq8: Vec<u8> = (0..2 * n).map(|_| r.next() as u8).collect();
        roundtrip(Codec::RAW, shl(Lane::of(Scalar::U8), 2), &iq8);
        let s12: Vec<i16> = (0..n).map(|_| ((r.next() as i64) >> 52) as i16).collect();
        roundtrip(Codec::DELTA, shl(lane(Scalar::I16, 12, true), 1), &s12);
        roundtrip(Codec::DELTA2, shl(lane(Scalar::I16, 12, false), 1), &s12);
    }
}

/// A float of another layout is stored as its own bits and read as its scalar; a value the layout can't hold exactly
/// is refused on write.
#[test]
fn float_lanes_store_their_own_bits() {
    let bf16 = shl(float_lane(Scalar::F32, 16, 8, false), 1);
    let bf16_be = shl(float_lane(Scalar::F32, 16, 8, true), 1);
    let f16 = shl(float_lane(Scalar::F32, 16, 5, false), 2);
    let e4m3 = shl(float_lane(Scalar::F32, 8, 4, false), 1);
    let v = [1.0f32, -2.5, 0.0, f32::INFINITY, 3.0];
    assert_eq!(
        encode_vec(Codec::RAW, bf16, &v),
        [0x80, 0x3f, 0x20, 0xc0, 0x00, 0x00, 0x80, 0x7f, 0x40, 0x40]
    );
    assert_eq!(
        encode_vec(Codec::RAW, bf16_be, &v),
        [0x3f, 0x80, 0xc0, 0x20, 0x00, 0x00, 0x7f, 0x80, 0x40, 0x40]
    );
    assert_eq!(encode_vec(Codec::RAW, e4m3, &v[..2]), [0x38, 0xc2]);
    let mut out = [0u8; 16];
    assert_eq!(
        enc(Codec::RAW, bf16, &[1.0f32, core::f32::consts::PI], &mut out),
        Err(SampleError::ValueTooWide { index: 1 })
    );
    let mut r = Rng(17);
    for n in [1usize, 3, 128, 300] {
        let halves: Vec<f32> = (0..2 * n)
            .map(|_| {
                let raw = (r.next() & 0xffff) as u16;
                // every f16 bit pattern is an f32 value: widen by hand
                let l = FloatLayout {
                    bits: 16,
                    exponent_bits: 5,
                };
                f32::from_bits(l.widen(u64::from(raw), Scalar::F32).unwrap() as u32)
            })
            .collect();
        roundtrip(Codec::RAW, f16, &halves);
        let bf: Vec<f32> = (0..n)
            .map(|_| f32::from_bits((r.next() as u32) & 0xffff_0000))
            .collect();
        roundtrip(Codec::RAW, bf16, &bf);
        roundtrip(Codec::RAW, bf16_be, &bf);
    }
    // a float lane takes only RAW
    for c in [Codec::ALP, Codec::DELTA, Codec::BIG_ENDIAN] {
        assert!(matches!(
            max_len(c, bf16, 4),
            Err(SampleError::Unsupported { .. })
        ));
    }
}

/// Lanes no codec codes are refused: no bits, more bits than the scalar, a float layout in an integer, an MSB-first
/// width past [raw::MSB_MAX], a float of a layout f32 doesn't hold in an f32.
#[test]
fn lanes_that_do_not_fit_are_refused() {
    for l in [
        lane(Scalar::U16, 0, false),
        lane(Scalar::U16, 17, false),
        lane(Scalar::I64, 57, true),
        lane(Scalar::I64, 63, true),
        float_lane(Scalar::I32, 16, 8, false),
        float_lane(Scalar::F32, 32, 10, false),
        float_lane(Scalar::F32, 16, 1, false),
        float_lane(Scalar::F32, 16, 15, false),
        float_lane(Scalar::F64, 48, 8, false),
    ] {
        assert!(
            matches!(
                read_column(Codec::RAW, shl(l, 1), 1, &[0; 8]),
                Err(SampleError::Unsupported { .. })
            ),
            "{l:?}"
        );
        assert!(matches!(
            max_len(Codec::RAW, shl(l, 1), 1),
            Err(SampleError::Unsupported { .. })
        ));
    }
    assert!(lane(Scalar::I64, 64, true).is_valid());
    assert!(lane(Scalar::I64, 56, true).is_valid());
    assert!(float_lane(Scalar::F64, 32, 10, false).is_valid());
    assert!(float_lane(Scalar::F64, 16, 5, true).is_valid());
    assert_eq!(
        max_len(Codec::RAW, shl(lane(Scalar::I64, 63, true), 1), 1).unwrap_err(),
        SampleError::Unsupported {
            codec: Codec::RAW,
            lane: lane(Scalar::I64, 63, true)
        }
    );
}

#[test]
fn every_raw_format_round_trips() {
    let mut r = Rng(1);
    for n in [1usize, 2, 3, 7, 8, 9, 100, 4099] {
        let u16s: Vec<u16> = (0..n).map(|_| r.next() as u16).collect();
        let i64s: Vec<i64> = (0..n).map(|_| r.next() as i64).collect();
        let f32s: Vec<f32> = (0..n).map(|_| f32::from_bits(r.next() as u32)).collect();
        for c in [Codec::RAW, Codec::BIG_ENDIAN] {
            roundtrip(c, sh(Scalar::U16, 1), &u16s);
            roundtrip(c, sh(Scalar::I64, 1), &i64s);
            roundtrip(c, sh(Scalar::F32, 1), &f32s);
        }
        for (c, w) in [
            (Codec::PACKED10_LSB, 10),
            (Codec::PACKED10_MSB, 10),
            (Codec::PACKED12_LSB, 12),
            (Codec::PACKED12_MSB, 12),
            (Codec::PACKED24_LSB, 24),
            (Codec::PACKED24_MSB, 24),
        ] {
            let signed: Vec<i32> = (0..n)
                .map(|_| ((r.next() as i64) >> (64 - w)) as i32)
                .collect();
            let unsigned: Vec<u64> = (0..n).map(|_| r.next() >> (64 - w)).collect();
            roundtrip(c, sh(Scalar::I32, 1), &signed);
            roundtrip(c, sh(Scalar::U64, 1), &unsigned);
            if w < 16 {
                let iq: Vec<i16> = (0..2 * n)
                    .map(|_| ((r.next() as i64) >> (64 - w)) as i16)
                    .collect();
                roundtrip(c, sh(Scalar::I16, 2), &iq);
            }
        }
    }
}

/// A slow sine with a little noise, as a sensor gives, in `f64`.
pub(super) fn sensor(n: usize, seed: u64) -> Vec<f64> {
    let mut r = Rng(seed);
    (0..n)
        .map(|i| {
            let noise = (r.next() >> 54) as f64 / 1024.0;
            ((i as f64 / 200.0).sin() * 100.0 + noise * 0.01 * 100.0).round() / 100.0
        })
        .collect()
}

#[test]
fn groups_decode_alone() {
    let s = sensor(2500, 5);
    let shape = sh(Scalar::F64, 1);
    let mut bytes = encode_vec(Codec::ALP, shape, &s);
    let col = read_column(Codec::ALP, shape, s.len(), &bytes).unwrap();
    assert_eq!(col.group_count(), 20);
    let (samples, range) = col.group_bytes(1).unwrap();
    assert_eq!(col.group_bytes(20), None);
    assert_eq!(samples, 128..256);
    // damage the middle group: the others still decode
    for b in &mut bytes[range.clone()] {
        *b = 0xff;
    }
    let col = read_column(Codec::ALP, shape, s.len(), &bytes).unwrap();
    let mut out = vec![0f64; 128];
    col.decode(0, &mut out).unwrap();
    assert_eq!(out, s[..128]);
    let mut out = vec![0f64; 2500 - 256];
    col.decode(256, &mut out).unwrap();
    assert_eq!(out, s[256..]);
    let mut out = vec![0f64; 128];
    assert!(col.decode(128, &mut out).is_err() || out != s[128..256]);
}

#[test]
fn columns_are_checked() {
    let shape = sh(Scalar::U16, 1);
    assert_eq!(
        read_column(Codec::PACKED12_LSB, shape, 3, &[0; 4]).unwrap_err(),
        SampleError::BadLength { len: 4 }
    );
    assert!(matches!(
        read_column(Codec::ALP, shape, 3, &[]).unwrap_err(),
        SampleError::Unsupported { .. }
    ));
    assert!(matches!(
        read_column(Codec::PACKED24_LSB, shape, 3, &[0; 9]).unwrap_err(),
        SampleError::Unsupported { .. }
    ));
    assert!(matches!(
        read_column(Codec(47), shape, 3, &[0; 6]).unwrap_err(),
        SampleError::Unsupported { .. }
    ));
    // an outer-coded column: no outer codec is known, whatever the inner one
    for inner in [Codec::RAW, Codec::BIG_ENDIAN] {
        assert!(matches!(
            read_column(Codec(inner.0 | Codec::OUTER), shape, 3, &[0; 6]).unwrap_err(),
            SampleError::Unsupported { .. }
        ));
    }
    let s = read_column(Codec::RAW, shape, 3, &[0; 6]).unwrap();
    assert!(matches!(
        s.decode(0, &mut [0i16; 3]),
        Err(SampleError::WrongType { .. })
    ));
    assert_eq!(s.decode(2, &mut [0u16; 2]), Err(SampleError::OutOfRange));
    assert_eq!(s.sample_bytes(1..3), Some(2..6));
    let f = sh(Scalar::F32, 1);
    // a table whose last end isn't the stream's length
    assert!(read_column(Codec::ALP, f, 1, &[9, 0, 0, 0, 1]).is_err());
    // a truncated stream reads as corrupt, not a panic
    let bytes = encode_vec(Codec::ALP, f, &[1.0f32, 2.0, 3.0]);
    let mut cut = bytes[..bytes.len() - 1].to_vec();
    cut[..4].copy_from_slice(&((bytes.len() - 5) as u32).to_le_bytes());
    let s = read_column(Codec::ALP, f, 3, &cut).unwrap();
    assert_eq!(
        s.decode(0, &mut [0f32; 3]),
        Err(SampleError::Corrupt { group: 0 })
    );
    assert_eq!(s.sample_bytes(0..usize::MAX), None);
}

#[test]
fn count_is_bounded_by_the_bytes() {
    // one group with an end past the bytes: a bound error, not a later read past them
    let f = sh(Scalar::F32, 1);
    assert_eq!(
        read_column(Codec::ALP, f, 1, &[0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0]).unwrap_err(),
        SampleError::Bound(crate::BoundError {
            value: u64::from(u32::MAX),
            bound: 4
        })
    );
    // a count read from a file: every group needs a table entry and a byte per lane
    let u = sh(Scalar::U16, 3);
    let bytes = encode_vec(Codec::DELTA, u, &[1u16, 2, 3]);
    assert!(read_column(Codec::DELTA, u, 1, &bytes).is_ok());
    for count in [1000, usize::MAX / 4] {
        assert_eq!(
            read_column(Codec::DELTA, u, count, &bytes).unwrap_err(),
            SampleError::BadLength { len: bytes.len() },
            "{count}"
        );
    }
    let wide = sh(Scalar::F64, 1000);
    assert_eq!(
        read_column(Codec::ALP, wide, 1, &[4, 0, 0, 0, 1, 0, 0, 0]).unwrap_err(),
        SampleError::BadLength { len: 8 }
    );
}

#[test]
fn deterministic() {
    let s = sensor(5000, 9);
    for c in [Codec::ALP, Codec::BIG_ENDIAN] {
        let shape = sh(Scalar::F64, 1);
        assert_eq!(encode_vec(c, shape, &s), encode_vec(c, shape, &s));
    }
}

#[test]
fn delta_codecs_round_trip_every_pattern() {
    let mut r = Rng(13);
    for n in [1usize, 2, 3, 127, 128, 129, 300, 1000] {
        let any_i64: Vec<i64> = (0..n).map(|_| r.next() as i64).collect();
        let any_u8: Vec<u8> = (0..n).map(|_| r.next() as u8).collect();
        let ramp: Vec<i32> = (0..n as i32).map(|i| i * 7 - 100).collect();
        let iq: Vec<i16> = (0..2 * n)
            .map(|i| ((i as f64 / 9.0).sin() * 2000.0) as i16)
            .collect();
        let edges: Vec<i16> = (0..n).map(|i| [i16::MIN, i16::MAX, 0, -1][i % 4]).collect();
        for c in [Codec::DELTA, Codec::DELTA2] {
            roundtrip(c, sh(Scalar::I64, 1), &any_i64);
            roundtrip(c, sh(Scalar::U8, 1), &any_u8);
            roundtrip(c, sh(Scalar::I32, 1), &ramp);
            roundtrip(c, sh(Scalar::I16, 2), &iq);
            roundtrip(c, sh(Scalar::I16, 4), &edges[..n / 4 * 4]);
        }
    }
}

#[test]
fn delta_bytes() {
    // a ramp packs at width 0: first value, min, width 0, no packed bytes
    let ramp: Vec<u16> = (0..5).map(|i| 10 + 3 * i).collect();
    assert_eq!(
        encode_vec(Codec::DELTA, sh(Scalar::U16, 1), &ramp),
        [5, 0, 0, 0, 10, 0, 3, 0, 0]
    );
    // order 2 of a parabola: first, first difference, min of second differences
    let p: Vec<i16> = (0..4).map(|i| i * i).collect();
    assert_eq!(
        encode_vec(Codec::DELTA2, sh(Scalar::I16, 1), &p),
        [7, 0, 0, 0, 0, 0, 1, 0, 2, 0, 0]
    );
    // differences 1, -1, 2: min -1, residuals 2, 0, 3 at width 2
    let v: Vec<i8> = vec![0, 1, 0, 2];
    assert_eq!(
        encode_vec(Codec::DELTA, sh(Scalar::I8, 1), &v),
        [4, 0, 0, 0, 0, 0xff, 2, 0b11_00_10]
    );
}

#[test]
fn delta_rejects_malformed_groups() {
    let shape = sh(Scalar::I16, 1);
    let v: Vec<i16> = (0..200).map(|i| (i * i % 300) as i16).collect();
    let bytes = encode_vec(Codec::DELTA, shape, &v);
    // a width over the scalar's bits
    let mut bad = bytes.clone();
    bad[8 + 4] = 17;
    let s = read_column(Codec::DELTA, shape, 200, &bad).unwrap();
    assert_eq!(
        s.decode(0, &mut [0i16; 200]),
        Err(SampleError::Corrupt { group: 0 })
    );
    // the second group still decodes
    let mut tail = [0i16; 72];
    s.decode(128, &mut tail).unwrap();
    assert_eq!(tail, v[128..]);
    assert!(read_column(Codec::DELTA, sh(Scalar::F32, 1), 1, &[]).is_err());
}

/// The ids of the float codecs that left the format stay reserved: refused for every element, on read and on write.
#[test]
fn reserved_float_codec_ids_are_refused() {
    for id in [24, 25, 34, 35] {
        for shape in [
            sh(Scalar::F32, 1),
            sh(Scalar::F64, 1),
            sh(Scalar::F32, 2),
            sh(Scalar::I32, 1),
        ] {
            assert!(matches!(
                read_column(Codec(id), shape, 1, &[0; 16]),
                Err(SampleError::Unsupported { .. })
            ));
            assert!(matches!(
                max_len(Codec(id), shape, 1),
                Err(SampleError::Unsupported { .. })
            ));
        }
    }
}

/// Random bytes as columns of every codec and lane: a check or a decode may fail, never panic (the fuzz target
/// `tape-samples` goes further).
#[test]
fn random_columns_never_panic() {
    let mut r = Rng(99);
    let codecs = [
        Codec::RAW,
        Codec::BIG_ENDIAN,
        Codec::PACKED10_LSB,
        Codec::PACKED12_MSB,
        Codec::PACKED24_LSB,
        Codec::DELTA,
        Codec::DELTA2,
        Codec::ALP,
    ];
    fn try_all<T: Sample>(codec: Codec, shape: Shape, count: usize, bytes: &[u8]) {
        if let Ok(s) = read_column(codec, shape, count, bytes) {
            let mut out = vec![T::default(); count * s.lanes()];
            // any bytes decode or fail, never panic: the result is not the point
            s.decode(0, &mut out).ok();
        }
    }
    for round in 0..2000 {
        let len = (r.next() % 600) as usize;
        let mut bytes: Vec<u8> = (0..len).map(|_| r.next() as u8).collect();
        let count = (r.next() % 300) as usize;
        let groups = if round % 2 == 0 {
            count.div_ceil(DELTA_GROUP)
        } else {
            count.div_ceil(ALP_GROUP)
        };
        // a plausible table half the time, so the group decoders run
        if round % 4 < 2 && bytes.len() >= groups * 4 && groups > 0 {
            let data = (bytes.len() - groups * 4) as u32;
            for g in 0..groups {
                let end = data * (g as u32 + 1) / groups as u32;
                bytes[g * 4..g * 4 + 4].copy_from_slice(&end.to_le_bytes());
            }
        }
        let codec = codecs[round % codecs.len()];
        let channels = 1 + (round / codecs.len()) % 3;
        try_all::<i16>(codec, sh(Scalar::I16, channels), count, &bytes);
        try_all::<u32>(codec, sh(Scalar::U32, channels), count, &bytes);
        try_all::<i64>(codec, sh(Scalar::I64, channels), count, &bytes);
        try_all::<f32>(codec, sh(Scalar::F32, 2 * channels), count, &bytes);
        try_all::<f64>(codec, sh(Scalar::F64, channels), count, &bytes);
        let bits = 1 + (round % 63) as u32;
        try_all::<i64>(
            codec,
            shl(lane(Scalar::I64, bits, round % 3 == 0), channels),
            count,
            &bytes,
        );
        try_all::<f32>(
            codec,
            shl(
                float_lane(Scalar::F32, 16, 5 + (round % 4) as u32, round % 2 == 0),
                channels,
            ),
            count,
            &bytes,
        );
    }
}

#[test]
fn grouped_columns_stay_under_4_gib() {
    // 2^29 u64 values are 4 GiB of data, past what a table end addresses: refused, not a panic in encode
    let s = sh(Scalar::U64, 1);
    for c in [Codec::DELTA, Codec::DELTA2] {
        assert_eq!(max_len(c, s, 1 << 29), Err(SampleError::BadShape));
        assert!(max_len(c, s, 1 << 20).is_ok());
    }
}

#[test]
fn raw_ranges_past_the_bytes_are_errors() {
    let short = [0u8; 3];
    assert_eq!(
        raw::decode_le(&short, 1, &mut [0u16; 1]),
        Err(SampleError::OutOfRange)
    );
    assert_eq!(
        raw::decode_be(&short, usize::MAX, &mut [0u16; 1]),
        Err(SampleError::OutOfRange)
    );
    let mut out = [0u16; 3];
    for packing in [raw::Packing::Lsb(12), raw::Packing::Msb(12)] {
        assert_eq!(
            raw::decode_packed(&short, packing, false, usize::MAX - 1, &mut out),
            Err(SampleError::OutOfRange),
            "{packing:?}"
        );
    }
    assert_eq!(
        raw::decode_packed(&short, raw::Packing::Msb(12), false, 2, &mut out[..1]),
        Err(SampleError::OutOfRange)
    );
}
