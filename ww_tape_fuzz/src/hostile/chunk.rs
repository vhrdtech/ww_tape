//! Chunk blocks: the chunk head (payload offset, segment shift, column table, counts and ticks), and the CRCs that
//! cover it.

use super::{
    Draw, Field, HEADER_FIELDS, LEN_FIELD, SESSION, crc32c, get, hit, hit_some, seal, span,
};
use ww_tape::chunk::{
    CHUNK_FIXED_LEN, COLUMN_ENTRY_LEN, MAX_COLUMNS, SEGMENT_SHIFT_MAX, SEGMENT_SHIFT_MIN,
    SERIES_GRID_LEN, chunk_len, write_chunk,
};
use ww_tape::series::SCALE_LEN_MAX;
use ww_tape::{
    BlockId, BlockKind, ChunkHead, Codec, ColumnKind, ColumnSpec, EndMode, MAX_BLOCK_LEN,
    Magnitude, SeriesGrid,
};

/// Where the column table of an events chunk starts: after the fixed head.
pub(crate) const TABLE: usize = super::HEADER_LEN + CHUNK_FIXED_LEN;

/// The grid fields of a series chunk: grid index, grid tick, scale length.
const GRID_FIELDS: [Field<'static>; 3] = [
    Field::new(TABLE, 8, &[]),
    Field::new(TABLE + 8, 8, &[i64::MAX as u64]),
    Field::new(TABLE + 16, 1, &[SCALE_LEN_MAX as u64]),
];

/// Where the column table of the chunk `b` starts, from its own head: after the grid and scale of a series chunk.
pub(crate) fn table(b: &[u8]) -> usize {
    if get(b, 5, 1) == u64::from(BlockKind::Series.to_u8()) {
        TABLE + SERIES_GRID_LEN + get(b, TABLE + 16, 1) as usize
    } else {
        TABLE
    }
}

/// The fixed fields of a chunk head after the block header.
pub(crate) const HEAD_FIELDS: [Field<'static>; 11] = [
    Field::new(32, 4, &[MAX_BLOCK_LEN as u64]),
    Field::new(36, 4, &[]),
    Field::new(40, 8, &[]),
    Field::new(48, 8, &[]),
    Field::new(56, 8, &[]),
    Field::new(64, 8, &[]),
    Field::new(72, 8, &[]),
    Field::new(
        80,
        1,
        &[
            SEGMENT_SHIFT_MIN as u64,
            SEGMENT_SHIFT_MAX as u64,
            ww_tape::chunk::SEGMENT_SHIFT as u64,
        ],
    ),
    Field::new(81, 1, &[MAX_COLUMNS as u64]),
    Field::new(82, 1, &[]),
    Field::new(83, 1, &[]),
];

/// The fields of column `i`'s entry in the table of the chunk `b`: kind, codec, param, offset.
pub(crate) fn column_fields(b: &[u8], i: usize) -> [Field<'static>; 4] {
    let at = table(b) + i * COLUMN_ENTRY_LEN;
    [
        Field::new(at, 1, &[]),
        Field::new(at + 1, 1, &[Codec::OUTER as u64]),
        Field::new(at + 2, 2, &[ww_tape::series::SERIES_CHANNELS_MAX as u64]),
        Field::new(at + 4, 4, &[]),
    ]
}

/// Write a whole chunk of `columns` with `head`.
pub(crate) fn build(head: &ChunkHead, columns: &[(ColumnSpec, &[u8])]) -> Vec<u8> {
    let specs: Vec<ColumnSpec> = columns.iter().map(|c| c.0).collect();
    let mut buf = vec![0u8; chunk_len(head, &specs, 0)];
    let n =
        write_chunk(&mut buf, BlockId::new(SESSION, 1), head, columns, 0).expect("a valid chunk");
    buf.truncate(n);
    buf
}

/// Where column `i`'s bytes start in the block, from the block's own head (None when outside the block).
pub(crate) fn column_start(b: &[u8], i: usize) -> Option<usize> {
    let payload = get(b, 32, 4) as usize;
    let at = get(b, table(b) + i * COLUMN_ENTRY_LEN + 4, 4) as usize;
    let start = payload.checked_add(at)?;
    (start < b.len()).then_some(start)
}

/// Compute a chunk block's CRCs again from what its head says: the segment CRCs (unless `stale_segments`, which
/// leaves the payload damaged as far as a reader can tell), the head CRC and the block CRC. A head whose payload
/// offset, shift or table doesn't fit keeps its old CRCs there.
pub(crate) fn reseal(b: &mut [u8], stale_segments: bool) {
    let len = b.len();
    let payload = get(b, 32, 4) as usize;
    let shift = get(b, 80, 1) as u32;
    let columns = get(b, 81, 1) as usize;
    let fits = payload >= table(b) && payload.checked_add(4).is_some_and(|e| e <= len);
    if fits && (u32::from(SEGMENT_SHIFT_MIN)..=u32::from(SEGMENT_SHIFT_MAX)).contains(&shift) {
        let payload_len = len - 4 - payload;
        let segments = payload_len.div_ceil(1 << shift);
        let table = table(b) + columns * COLUMN_ENTRY_LEN;
        if !stale_segments && table + 4 * segments <= payload {
            for s in 0..segments {
                let from = payload + (s << shift);
                let to = (from + (1 << shift)).min(len - 4);
                let crc = crc32c(&[&b[from..to]]);
                b[table + 4 * s..table + 4 * s + 4].copy_from_slice(&crc.to_le_bytes());
            }
        }
    }
    if fits && let (Some(a), Some(c)) = (span(b, 0, 28), span(b, 32, payload)) {
        let crc = crc32c(&[a, c]);
        b[28..32].copy_from_slice(&crc.to_le_bytes());
    }
    seal(b);
}

/// Write hostile values over the head of the chunk `b` (block header, fixed head, column table) and reseal it; a
/// draw also leaves its segment CRCs stale.
pub(crate) fn hit_head(d: &mut Draw<'_>, b: &mut [u8]) {
    let columns = get(b, 81, 1) as usize;
    match d.u8() % 16 {
        15 => hit(d, b, LEN_FIELD),
        1..4 => {
            let f = HEADER_FIELDS[d.below(HEADER_FIELDS.len())];
            hit(d, b, f);
        }
        4..8 => hit_some(d, b, &HEAD_FIELDS),
        8..10 if get(b, 5, 1) == u64::from(BlockKind::Series.to_u8()) => {
            hit_some(d, b, &GRID_FIELDS);
        }
        8..10 => hit_some(d, b, &HEAD_FIELDS),
        _ => {
            let i = d.below(columns + 1);
            let fields = column_fields(b, i);
            hit_some(d, b, &fields);
        }
    }
}

/// A chunk block whose head takes hostile values: an events or series chunk of raw columns sized by the input.
pub fn chunk(data: &[u8]) -> Vec<u8> {
    let mut d = Draw::new(data);
    let series = d.bool();
    let n = 1 + d.below(4);
    let cols: Vec<Vec<u8>> = (0..n)
        .map(|_| {
            let len = d.below(200);
            let x = d.u8();
            (0..len).map(|i| x.wrapping_add(i as u8)).collect()
        })
        .collect();
    let columns: Vec<(ColumnSpec, &[u8])> = cols
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let kind = if series {
                ColumnKind::Samples
            } else {
                ColumnKind::from_u8(1 + i as u8)
            };
            let spec = ColumnSpec {
                kind,
                codec: Codec::RAW,
                param: i as u16,
                len: c.len(),
            };
            (spec, c.as_slice())
        })
        .collect();
    let count = 1 + u32::from(d.u8());
    let first_item = u64::from(d.u16());
    let first_tick = i64::from(d.u32() as i32);
    let last_tick = first_tick + i64::from(d.u16());
    let head = ChunkHead {
        kind: if series {
            BlockKind::Series
        } else {
            BlockKind::Events
        },
        track_id: 7,
        track_number: 1,
        first_item,
        count,
        first_tick,
        last_tick,
        latest_end: last_tick,
        time_mode: 0,
        end_mode: EndMode::Absent,
        grid: series.then(|| {
            let grid_index = first_item.saturating_sub(u64::from(d.u8()));
            SeriesGrid::start(
                grid_index,
                first_tick,
                Magnitude::pow10(-((d.u8() % 19) as i8)),
            )
        }),
    };
    let mut b = build(&head, &columns);
    hit_head(&mut d, &mut b);
    reseal(&mut b, d.u8() % 4 == 3);
    b
}
