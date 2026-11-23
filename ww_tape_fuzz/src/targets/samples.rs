//! ww_tape sample codecs and the lz4 payload codec on any bytes: reading a column never panics, and values built
//! from the input code and decode back bit for bit, whole and from any sample on. Elements are predefined or
//! described by the input: any width, either bit order, floats of another layout.
use ww_tape::lz4::{lz4_decode, lz4_decoded_len, lz4_encode, lz4_max_len};
use ww_tape::samples::{
    Element, ElementDesc, SampleShape, Samples, Scalar, encode, max_encoded_len,
};
use ww_tape::{Codec, Sample};

const CODECS: [Codec; 11] = [
    Codec::RAW,
    Codec::BIG_ENDIAN,
    Codec::PACKED10_LSB,
    Codec::PACKED10_MSB,
    Codec::PACKED12_LSB,
    Codec::PACKED12_MSB,
    Codec::PACKED24_LSB,
    Codec::PACKED24_MSB,
    Codec::DELTA,
    Codec::DELTA2,
    Codec::ALP,
];

/// Read `bytes` as a column of `count` samples and decode all of it and a tail, if the column checks.
pub(crate) fn read<T: Sample>(
    codec: Codec,
    shape: SampleShape,
    count: usize,
    bytes: &[u8],
    from: usize,
) {
    if let Ok(s) = Samples::new(codec, shape, count, bytes) {
        let lanes = s.lanes();
        if count.saturating_mul(lanes) > 1 << 20 {
            return;
        }
        let mut out = vec![T::default(); count * lanes];
        let _ = s.decode(0, &mut out);
        let from = from % (count + 1);
        let _ = s.decode(from, &mut out[from * lanes..]);
        for g in 0..s.group_count() {
            let (samples, range) = s
                .group_bytes(g)
                .expect("a column that checks holds every group below group_count");
            assert!(samples.end <= count, "samples within the count");
            assert!(range.end <= bytes.len(), "a range within the bytes");
        }
    }
}

/// Values of `T` from the input, masked to fit `codec` and the element's bits, coded and decoded back. A float of
/// another layout takes the values a raw column of the input holds.
fn round<T: Sample>(codec: Codec, shape: SampleShape, data: &[u8], from: usize) {
    let w = (T::SCALAR.bits() / 8).max(1) as usize;
    let lanes = shape.lanes().unwrap() as usize;
    let lane = shape.element.lane();
    if lane.exponent_bits != 0 {
        let per = lane.bits as usize * lanes;
        let count = (data.len() * 8 / per).min(1 << 12);
        let Ok(raw) = Samples::new(Codec::RAW, shape, count, &data[..(count * per).div_ceil(8)])
        else {
            return;
        };
        let mut values = vec![T::default(); count * lanes];
        if raw.decode(0, &mut values).is_ok() {
            round_values(codec, shape, values, from);
        }
        return;
    }
    let bits = match codec.0 {
        17 | 18 => 10,
        19 | 20 => 12,
        21 | 22 => 24,
        _ => 64,
    }
    .min(lane.bits);
    let mut values: Vec<T> = data
        .chunks_exact(w)
        .map(|c| {
            let mut b = [0u8; 8];
            b[..w].copy_from_slice(c);
            let v = u64::from_le_bytes(b);
            let v = if bits < 64 {
                let x = v & ((1 << bits) - 1);
                if T::SCALAR.is_signed() {
                    (((x << (64 - bits)) as i64) >> (64 - bits)) as u64
                } else {
                    x
                }
            } else {
                v
            };
            T::from_bits(v)
        })
        .collect();
    values.truncate(values.len() / lanes * lanes);
    round_values(codec, shape, values, from);
}

/// Code whole samples `values` and decode them back, whole and from `from` on.
fn round_values<T: Sample>(codec: Codec, shape: SampleShape, values: Vec<T>, from: usize) {
    let lanes = shape.lanes().unwrap() as usize;
    let count = values.len() / lanes;
    let Ok(max) = max_encoded_len(codec, shape, count) else {
        return;
    };
    let mut out = vec![0u8; max];
    let Ok(n) = encode(codec, shape, &values, &mut out) else {
        return;
    };
    let s = Samples::new(codec, shape, count, &out[..n]).expect("written columns check");
    let mut back = vec![T::default(); values.len()];
    s.decode(0, &mut back).expect("written columns decode");
    assert!(
        back.iter()
            .zip(&values)
            .all(|(a, b)| a.to_bits() == b.to_bits()),
        "the columns decode to the values written"
    );
    let from = from % (count + 1);
    let mut tail = vec![T::default(); (count - from) * lanes];
    s.decode(from, &mut tail).expect("a tail decodes");
    assert!(
        tail.iter()
            .zip(&values[from * lanes..])
            .all(|(a, b)| a.to_bits() == b.to_bits()),
        "a tail decodes to the values written"
    );
}

fn both(codec: Codec, element: ElementDesc, channels: u32, count: usize, data: &[u8], from: usize) {
    let shape = SampleShape::new(element, channels);
    macro_rules! go {
        ($t:ty) => {{
            read::<$t>(codec, shape, count, data, from);
            round::<$t>(codec, shape, data, from);
        }};
    }
    match element.scalar() {
        Scalar::I8 => go!(i8),
        Scalar::I16 => go!(i16),
        Scalar::I32 => go!(i32),
        Scalar::I64 => go!(i64),
        Scalar::U8 => go!(u8),
        Scalar::U16 => go!(u16),
        Scalar::U32 => go!(u32),
        Scalar::U64 => go!(u64),
        Scalar::F32 => go!(f32),
        Scalar::F64 => go!(f64),
        Scalar::Bool => go!(bool),
    }
}

/// The element of header bytes `b1` and `b2`: a predefined one while bit 7 of `b1` is clear, else one of
/// `1 + b1 % 64` bits, signed (bit 2 of `b2`), MSB first (bit 3), a pair (bit 4), with the exponent bits bits 5 to 7
/// of `b2` pick; None when the raw codec doesn't code it.
fn element_of(b1: u8, b2: u8) -> Option<ElementDesc> {
    if b1 & 0x80 == 0 {
        return Some(ElementDesc::of(
            Element::ALL[b1 as usize % Element::ALL.len()],
        ));
    }
    let exponent_bits = [0, 0, 0, 4, 5, 8, 11, 0][usize::from(b2 >> 5)];
    let mut bits = 1 + (b1 & 0x3f);
    if exponent_bits == 11 {
        bits = 64;
    } else if exponent_bits != 0 {
        bits = bits.clamp(exponent_bits + 2, 32);
    }
    let e = ElementDesc {
        bits,
        exponent_bits,
        signed: exponent_bits != 0 || b2 & 4 != 0,
        msb_first: b2 & 8 != 0,
        parts: shrink_wrap::prelude::UNib32(if b2 & 16 != 0 { 2 } else { 1 }),
        zero: None,
    };
    e.is_coded().then_some(e)
}

pub fn samples(data: &[u8]) {
    // [codec][element][channels][count u16][from u16][column bytes...]
    if data.len() < 7 {
        return;
    }
    let codec = CODECS[data[0] as usize % CODECS.len()];
    let Some(element) = element_of(data[1], data[2]) else {
        return;
    };
    let channels = 1 + u32::from(data[2] % 4);
    let count = usize::from(u16::from_le_bytes([data[3], data[4]]));
    let from = usize::from(u16::from_le_bytes([data[5], data[6]]));
    let body = &data[7..];
    both(codec, element, channels, count, body, from);
    if let Ok(len) = lz4_decoded_len(body)
        && len <= 1 << 20
    {
        let mut out = vec![0u8; len];
        let _ = lz4_decode(body, &mut out);
    }
    let mut out = vec![0u8; lz4_max_len(body.len())];
    let n = lz4_encode(body, &mut out).expect("output sized by lz4_max_len");
    let mut back = vec![0u8; body.len()];
    assert_eq!(lz4_decode(&out[..n], &mut back), Ok(body.len()));
    assert_eq!(back, body);
}
