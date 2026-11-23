use super::*;
use crate::TrackId;
use crate::TrackNum;
use crate::tests::id;
use shrink_wrap::prelude::UNib32;

/// xorshift64*, a fixed seed per test.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}

fn encode_vec<T: Sample>(codec: Codec, shape: SampleShape, values: &[T]) -> Vec<u8> {
    let lanes = shape.lanes().unwrap() as usize;
    let mut out = vec![0u8; max_encoded_len(codec, shape, values.len() / lanes).unwrap()];
    let n = encode(codec, shape, values, &mut out).unwrap();
    out.truncate(n);
    out
}

/// Encode, check the column and decode it back whole.
fn roundtrip<T: Sample>(codec: Codec, shape: SampleShape, values: &[T]) -> Vec<u8> {
    let lanes = shape.lanes().unwrap() as usize;
    let bytes = encode_vec(codec, shape, values);
    let s = Samples::new(codec, shape, values.len() / lanes, &bytes).unwrap();
    assert_eq!((s.codec(), s.shape(), s.lanes()), (codec, shape, lanes));
    let mut out = vec![T::default(); values.len()];
    s.decode(0, &mut out).unwrap();
    assert!(
        out.iter()
            .zip(values)
            .all(|(a, b)| a.to_bits() == b.to_bits()),
        "{codec:?} {shape:?}"
    );
    bytes
}

fn d(e: Element) -> ElementDesc {
    ElementDesc::of(e)
}

/// A shape is mapped once: a shape without lanes is refused typed, a codec's error comes back whole with the shape,
/// and its text says both.
#[test]
fn shapes_map_to_scalars_and_lanes() {
    let none = SampleShape::new(d(Element::U16), 0);
    assert_eq!(
        Samples::new(Codec::RAW, none, 3, &[0; 6]).unwrap_err(),
        SampleError::BadShape
    );
    assert_eq!(
        max_encoded_len(Codec::RAW, none, 1),
        Err(SampleError::BadShape)
    );
    let iq = SampleShape::new(d(Element::ComplexI16), 3);
    let s = Samples::new(Codec::RAW, iq, 2, &[0; 24]).unwrap();
    assert_eq!((s.lanes(), s.count()), (6, 2));
    let wrong = s.decode(0, &mut [0u16; 6]).unwrap_err();
    assert_eq!(
        wrong,
        SampleError::Column {
            shape: iq,
            error: codecs::SampleError::WrongType {
                column: Scalar::I16,
                asked: Scalar::U16
            }
        }
    );
    assert_eq!(
        crate::tests::chain_text(&wrong),
        "a column of 3 × complex i16 doesn't code: a column of i16 read as u16"
    );
    assert_eq!(
        encode(Codec::RAW, iq, &[0u32; 6], &mut [0; 24]),
        Err(SampleError::Column {
            shape: iq,
            error: codecs::SampleError::WrongType {
                column: Scalar::I16,
                asked: Scalar::U32
            }
        })
    );
    let alp = Samples::new(Codec::ALP, iq, 2, &[0; 24]).unwrap_err();
    assert_eq!(
        crate::tests::chain_text(&alp),
        "a column of 3 × complex i16 doesn't code: codec 26 doesn't code i16"
    );
}

/// A described element's column holds exactly its bits per part: 12-bit values 3 to 2 bytes, an 8-bit IQ pair
/// with a zero as 2 bytes, bfloat16 as 2 bytes read back as f32.
#[test]
fn described_elements_code_at_their_width() {
    let mut twelve = d(Element::U16);
    twelve.bits = 12;
    let shape = SampleShape::scalar(twelve);
    let values: Vec<u16> = (0..64).map(|i| i * 61 % 4096).collect();
    let bytes = roundtrip(Codec::RAW, shape, &values);
    assert_eq!(bytes.len(), 64 * 12 / 8);

    let mut iq = d(Element::U8);
    iq.parts = UNib32(2);
    iq.zero = exact_units::Ratio64::new(255, 2);
    let values: Vec<u8> = (0..64u32).map(|i| (i * 7 % 256) as u8).collect();
    let bytes = roundtrip(Codec::RAW, SampleShape::new(iq, 2), &values);
    assert_eq!(bytes.len(), 64);

    let bf16 = ElementDesc {
        bits: 16,
        exponent_bits: 8,
        signed: true,
        msb_first: false,
        parts: UNib32(1),
        zero: None,
    };
    let values: Vec<f32> = (0..32u32)
        .map(|i| f32::from_bits((i * 2047) << 16))
        .collect();
    let bytes = roundtrip(Codec::RAW, SampleShape::scalar(bf16), &values);
    assert_eq!(bytes.len(), 64);
    let not_bf16 = encode(
        Codec::RAW,
        SampleShape::scalar(bf16),
        &[1.000_1f32],
        &mut [0; 4],
    );
    assert!(not_bf16.is_err(), "{not_bf16:?}");
}

/// Columns of every codec, and a series chunk holding some, are the golden file's (`WW_TAPE_GOLDEN=update` rewrites
/// it): one line per column, `codec element channels count: hex`, then the chunk block. Inputs come from integer
/// arithmetic only, so they are the same on every platform.
#[test]
fn samples_golden() {
    let mut r = Rng(2026);
    let n = 300;
    let wave: Vec<i64> = (0..n as i64)
        .map(|i| (i * i * 37 % 4001) - 2000 + i % 7)
        .collect();
    let i16s: Vec<i16> = wave.iter().map(|v| *v as i16).collect();
    let u16s: Vec<u16> = wave.iter().map(|v| (*v + 2000) as u16).collect();
    let i32s: Vec<i32> = wave.iter().map(|v| (*v * 1000) as i32).collect();
    let f64s: Vec<f64> = wave.iter().map(|v| *v as f64 / 16.0).collect();
    let f32s: Vec<f32> = wave.iter().map(|v| *v as f32 / 8.0).collect();
    let any: Vec<f64> = (0..n).map(|_| f64::from_bits(r.next())).collect();
    let bools: Vec<bool> = wave.iter().map(|v| v % 3 == 0).collect();
    let mut lines = vec![];
    let mut add = |line: String| lines.push(line);
    fn col<T: Sample>(c: Codec, shape: SampleShape, v: &[T]) -> String {
        let bytes = roundtrip(c, shape, v);
        let lanes = shape.lanes().unwrap() as usize;
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        // a predefined element by its name, one word as the fuzz seeds parse it
        let element = shape.element.element().expect("predefined elements");
        format!(
            "{} {:?} {} {}: {hex}",
            c.0,
            element,
            shape.channels,
            v.len() / lanes
        )
    }
    let s = |e: Element| SampleShape::scalar(d(e));
    add(col(Codec::RAW, s(Element::I16), &i16s));
    add(col(Codec::RAW, s(Element::Bool), &bools));
    add(col(Codec::BIG_ENDIAN, s(Element::I32), &i32s));
    add(col(
        Codec::BIG_ENDIAN,
        SampleShape::new(d(Element::ComplexI16), 1),
        &i16s,
    ));
    for c in [Codec::PACKED12_LSB, Codec::PACKED12_MSB] {
        add(col(c, s(Element::I16), &i16s));
        add(col(c, s(Element::U16), &u16s));
    }
    let tens: Vec<i16> = i16s.iter().map(|v| v / 4).collect();
    add(col(Codec::PACKED10_LSB, s(Element::I16), &tens));
    add(col(Codec::PACKED10_MSB, s(Element::I16), &tens));
    add(col(Codec::PACKED24_LSB, s(Element::I32), &i32s));
    add(col(Codec::PACKED24_MSB, s(Element::I32), &i32s));
    let c = Codec::ALP;
    add(col(c, s(Element::F64), &f64s));
    add(col(c, s(Element::F32), &f32s));
    add(col(c, s(Element::F64), &any[..40]));
    add(col(c, SampleShape::new(d(Element::ComplexF32), 1), &f32s));
    // decimals, the data ALP codes as integers: IEEE division gives the same values everywhere
    let cents: Vec<f64> = wave.iter().map(|v| *v as f64 / 100.0).collect();
    let cents32: Vec<f32> = wave.iter().map(|v| *v as f32 / 100.0).collect();
    add(col(Codec::ALP, s(Element::F64), &cents));
    add(col(Codec::ALP, s(Element::F32), &cents32));
    for c in [Codec::DELTA, Codec::DELTA2] {
        add(col(c, s(Element::I16), &i16s));
        add(col(c, SampleShape::new(d(Element::I32), 3), &i32s));
        add(col(c, s(Element::I64), &wave));
    }
    let lz4 = {
        let raw: Vec<u8> = i16s.iter().flat_map(|v| v.to_le_bytes()).collect();
        let mut out = vec![0u8; crate::lz4::lz4_max_len(raw.len())];
        let n = crate::lz4::lz4_encode(&raw, &mut out).unwrap();
        out.truncate(n);
        out
    };
    lines.push(format!(
        "48 payload: {}",
        lz4.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ));
    // a series chunk: channel 0 packed 12-bit, channel 1 delta
    let shape = s(Element::I16);
    let c0 = encode_vec(Codec::PACKED12_LSB, shape, &i16s);
    let c1 = encode_vec(Codec::DELTA, shape, &i16s);
    let head = crate::ChunkHead {
        kind: crate::BlockKind::Series,
        track_id: TrackId::new(0x5a5a),
        track_number: TrackNum::new(3),
        first_item: crate::ItemNum::new(1000),
        count: n as u32,
        first_tick: 0,
        last_tick: (n as i64 - 1) * 20,
        latest_end: (n as i64 - 1) * 20,
        time_mode: 0,
        end_mode: crate::EndMode::Absent,
        grid: Some(crate::SeriesGrid::start(1000, 0, crate::Magnitude::ONE)),
        tile: crate::TileCoords::NONE,
    };
    let spec = |codec, param, len| crate::ColumnSpec {
        kind: crate::ColumnKind::Samples,
        codec,
        param,
        len,
    };
    let cols = [
        (spec(Codec::PACKED12_LSB, 0, c0.len()), &c0[..]),
        (spec(Codec::DELTA, 1, c1.len()), &c1[..]),
    ];
    let specs: Vec<_> = cols.iter().map(|c| c.0).collect();
    let mut block = vec![0u8; crate::chunk::chunk_len(&head, &specs, 0)];
    let len = crate::chunk::write_chunk(&mut block, id(1), &head, &cols, 0).unwrap();
    block.truncate(len);
    let read = crate::Block::read(&block).unwrap();
    let chunk = read.chunk().unwrap();
    for (param, codec) in [(0, Codec::PACKED12_LSB), (1, Codec::DELTA)] {
        let column = chunk.find(crate::ColumnKind::Samples, param).unwrap();
        assert_eq!(column.codec, codec);
        let samples = Samples::of(&column, shape, n).unwrap();
        let mut out = vec![0i16; n];
        samples.decode(0, &mut out).unwrap();
        assert_eq!(out, i16s);
    }
    lines.push(format!(
        "chunk: {}",
        block.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ));
    let text = lines.join("\n") + "\n";
    let golden = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("golden/v1-samples.hex");
    if std::env::var("WW_TAPE_GOLDEN").as_deref() == Ok("update") {
        std::fs::write(&golden, &text).unwrap();
    }
    let want = std::fs::read_to_string(&golden)
        .expect("golden/v1-samples.hex: run with WW_TAPE_GOLDEN=update");
    assert!(
        text == want,
        "the format changed: bytes differ from golden/v1-samples.hex"
    );
}
