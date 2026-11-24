//! An 8-bit IQ radio's samples as a series track (docs/tracks.md, "What a track's items are"): the definition
//! describes the element, and the samples are stored as the radio sends them, with no new element or codec.

use ww_tape::ItemNum;

use shrink_wrap::prelude::*;
use ww_tape::chunk::ChunkHead;
use ww_tape::samples::Samples;
use ww_tape::{
    BlockKind, ClockDef, ClockOrigin, Codec, ColumnKind, ColumnSpec, Definitions, Describe,
    ElementDesc, EndMode, GridDef, Items, Magnitude, MemSink, Ratio64, SeriesGrid, SyncPolicy,
    TapeReader, TapeWriter, TrackDef, TrackKind, TrackRepeat, clock_key, track_key,
};
use ww_tape::{ClockNum, TrackNum};

/// The radio's clock: ns since the capture starts.
const CLOCK: ClockNum = ClockNum::new(1);
/// Samples in the chunk the test writes.
const COUNT: u32 = 1024;

/// The track of the radio's samples.
fn iq8_track() -> TrackDef<'static> {
    // --8<-- [start:def]
    // Two unsigned bytes per sample, I then Q, with zero at 127.5.
    let element = ElementDesc {
        bits: 8,
        exponent_bits: 0,
        signed: false,
        msb_first: false,
        parts: UNib32(2),
        zero: Ratio64::new(255, 2),
    };
    let series = GridDef {
        channels: UNib32(1),
        period: Ratio64::new(1250, 3).expect("2.4 MS/s on a 1 ns clock"),
        scale: Magnitude::ONE,
        summaries: None,
        media: None,
        axes: RefVec::new(),
    };
    TrackDef {
        number: TrackNum::new(1),
        key: track_key(0x5d8, &[]),
        clock: CLOCK,
        dimensions: RefVec::Slice { slice: &[] },
        items: Items::Numbers {
            element,
            unit: None,
        },
        label: "iq8",
        repeat: TrackRepeat::Stream,
        kind: TrackKind::Series(series),
        relations: RefVec::Slice { slice: &[] },
        owner: [0; 32],
        defined_twice: false,
    }
    // --8<-- [end:def]
}

/// The radio's bytes for `count` samples: I and Q interleaved, a slow turn around 127.5.
fn radio_bytes(count: u32) -> Vec<u8> {
    (0..count)
        .flat_map(|i| [(i % 256) as u8, (255 - i % 256) as u8])
        .collect()
}

/// The 8-bit IQ track needs no element or codec of its own: its definition checks, its chunk stores the radio's
/// bytes as sent, two a sample, and the reader decodes them back to the radio's numbers.
#[test]
fn an_8_bit_iq_track_is_stored_as_the_radio_sends_it() {
    let track = iq8_track();
    track.check_kind().unwrap();
    let shape = track.series_shape().expect("a series of numbers");
    assert_eq!(
        shape.element.element(),
        None,
        "no predefined element is 8-bit IQ"
    );
    let TrackKind::Series(series) = &track.kind else {
        unreachable!("iq8_track is a series")
    };
    let mut defs = Definitions::new();
    crate::definitions::hold(
        &mut defs,
        &[
            Describe::Clock(ClockDef {
                number: CLOCK,
                key: clock_key(0x5d8, &[], 1),
                rate: Magnitude::pow10(9).into(),
                resolution: Magnitude::pow10(-9).into(),
                origin: ClockOrigin::Boot,
                label: "radio ns",
            }),
            Describe::Track(track.clone()),
        ],
    )
    .unwrap();

    let bytes = radio_bytes(COUNT);
    let grid: SeriesGrid = series.first_grid(0);
    let (first_tick, last_tick) = grid
        .chunk_ticks(series.period, ItemNum::new(0), COUNT)
        .unwrap();
    let head = ChunkHead {
        kind: BlockKind::Series,
        track_id: track.id(),
        track_number: TrackNum::new(1),
        first_item: ItemNum::new(0),
        count: COUNT,
        first_tick,
        last_tick,
        latest_end: last_tick,
        time_mode: 0,
        end_mode: EndMode::Absent,
        grid: Some(grid),
        tile: ww_tape::TileCoords::NONE,
        level: 0,
    };
    let spec = ColumnSpec {
        kind: ColumnKind::Samples,
        codec: Codec::RAW,
        param: 0,
        len: bytes.len(),
    };
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    let tape = w.sink().tape();
    w.write_chunk(&defs, &head, &[(spec, &bytes)]).unwrap();
    w.close().unwrap();
    let file = tape.to_vec().unwrap();

    let r = TapeReader::open(&file[..]).unwrap();
    let mut buf = Vec::new();
    let mut read = Vec::new();
    for e in r.track_entries(0, track.id()).unwrap() {
        let b = r.read_block(0, &e, &mut buf).unwrap();
        let Some(chunk) = b.chunk() else { continue };
        let column = chunk.find(ColumnKind::Samples, 0).unwrap();
        assert_eq!(column.bytes, &bytes[..], "the radio's bytes, as sent");
        let s = Samples::of(&column, shape, COUNT as usize).unwrap();
        read.resize(COUNT as usize * 2, 0u8);
        s.decode(0, &mut read).unwrap();
    }
    assert_eq!(read, bytes);
    assert_eq!(bytes.len(), 2 * COUNT as usize, "two bytes a sample");
}
