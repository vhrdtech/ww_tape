//! Series chunks with flag runs, behind the block of definitions of their track: each run's first sample, count and
//! flags word, the column's codec and length, and the chunk's sample count take hostile values.

use super::chunk::{build, column_fields, column_start, hit_head, reseal};
use super::series::track_item;
use super::{Draw, Field, SESSION, clock_item, describe_block, hit};
use ww_tape::flag_runs::{FLAG_RUN_LEN, FLAG_RUNS_MAX, FlagRunsBuilder};
use ww_tape::{
    BlockId, BlockKind, ChunkHead, Codec, ColumnKind, ColumnSpec, EndMode, Magnitude, Ratio64,
    SampleShape, TrackId, TrackNum,
};

/// A block of definitions of an i16 series track and a series chunk with hostile flag runs.
///
/// The chunk's samples are zeros. Its runs are built from the input in the writer's form, then take hostile values.
/// The head CRC holds, so a reader takes the head.
pub fn flags(data: &[u8]) -> Vec<u8> {
    let mut d = Draw::new(data);
    let channels = 1 + d.below(3) as u32;
    let shape = SampleShape::new(ww_tape::ElementDesc::of(ww_tape::Element::I16), channels);
    let count = 1 + d.below(256) as u32;
    let mut runs = FlagRunsBuilder::new();
    let mut at = 0;
    while at < count {
        let n = (1 + d.below(32) as u32).min(count - at);
        let flags = if d.bool() { 0 } else { d.u32() };
        runs.push(n, flags).expect("a few runs");
        at += n;
    }
    if runs.is_empty() {
        // a chunk without a flagged sample has no runs column: flag its first sample
        runs.clear();
        runs.push(1, 1).expect("one run");
        runs.push(count - 1, 0).expect("the rest unflagged");
    }
    let mut column = vec![];
    runs.write(&mut column);
    let samples = vec![0u8; 2 * (channels * count) as usize];
    let first_item = ww_tape::ItemNum::new(u64::from(d.u16()));
    let head = ChunkHead {
        kind: BlockKind::Series,
        track_id: TrackId::new(1),
        track_number: TrackNum::new(super::series::TRACK),
        first_item,
        count,
        first_tick: 0,
        last_tick: 0,
        latest_end: 0,
        time_mode: 0,
        end_mode: EndMode::Absent,
        grid: Some(ww_tape::SeriesGrid::start(
            first_item.get(),
            0,
            Magnitude::ONE,
        )),
        tile: ww_tape::TileCoords::NONE,
        level: 0,
    };
    let spec = |kind, codec, len| ColumnSpec {
        kind,
        codec,
        param: 0,
        len,
    };
    let mut b = build(
        &head,
        &[
            (
                spec(ColumnKind::Samples, Codec::RAW, samples.len()),
                &samples,
            ),
            (
                spec(ColumnKind::Flags, Codec::FLAG_RUNS, column.len()),
                &column,
            ),
        ],
    );
    let at = column_start(&b, 1).expect("the runs are in the block");
    let n = runs.runs().len();
    let caps = &[
        u64::from(count),
        FLAG_RUNS_MAX as u64,
        u64::from(u32::MAX),
        (FLAG_RUN_LEN * n) as u64,
    ];
    for _ in 0..1 + d.below(3) {
        let run = at + FLAG_RUN_LEN * d.below(n);
        match d.u8() % 10 {
            0..3 => hit(&mut d, &mut b, Field::new(run, 4, caps)),
            3..6 => hit(&mut d, &mut b, Field::new(run + 4, 4, caps)),
            6 => hit(&mut d, &mut b, Field::new(run + 8, 4, caps)),
            // the runs column's entry: kind, codec, param, offset
            7 => {
                let fields = column_fields(&b, 1);
                let f = fields[d.below(fields.len())];
                hit(&mut d, &mut b, f);
            }
            8 => hit_head(&mut d, &mut b),
            _ => {
                // a run's bytes copied over the next one's: a repeat, an overlap
                if n > 1 {
                    let i = d.below(n - 1);
                    let from = at + FLAG_RUN_LEN * i;
                    b.copy_within(from..from + FLAG_RUN_LEN, from + FLAG_RUN_LEN);
                }
            }
        }
    }
    reseal(&mut b, d.u8() % 4 == 3);
    let item = track_item(shape, Ratio64::new(1, 1).expect("a ratio"), Magnitude::ONE);
    let mut out = describe_block(BlockId::new(SESSION, 0), &[&clock_item(), &item]);
    out.extend_from_slice(&b);
    out
}
