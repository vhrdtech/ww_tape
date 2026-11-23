//! Index, close, piece head and tail blocks: an index's entry offset, directory, self and previous offsets, entry
//! length and entries (flags, range pair and unit hash among them; entries longer than [ENTRY_LEN], well formed or
//! not); a close's offsets and track ranges; a piece head's tracks and integrity level; a tail's piece length and
//! close offset.

use ww_tape::ItemNum;

use super::{
    Draw, Field, HEADER_FIELDS, LEN_FIELD, SESSION, crc32c, get, hit, hit_some, seal, span,
};
use ww_tape::TrackId;
use ww_tape::TrackNum;
use ww_tape::index::{
    CHECKPOINT_ENTRIES_MAX, ENTRY_LEN, INDEX_LEN_MAX, IndexEntry, IndexHead, IndexKind, TRACK_LEN,
    index_len, track_count, write_index,
};
use ww_tape::piece::{
    Close, CloseReason, ContentHash, OwnerId, PieceHead, PieceId, PieceIdent, REMOVED_PIECES_MAX,
    RemovedPiece, TAIL_LEN, Tail, TrackHash, TrackRange, TrackStart, close_len, piece_head_len,
    write_close, write_piece_head,
};
use ww_tape::{BlockId, BlockKind, MAX_BLOCK_LEN, SessionTag};

/// Where an index block's directory starts.
const DIRECTORY: usize = 60;

/// The fixed fields of an index head after the block header.
const INDEX_FIELDS: [Field<'static>; 6] = [
    Field::new(32, 4, &[INDEX_LEN_MAX as u64]),
    Field::new(36, 4, &[CHECKPOINT_ENTRIES_MAX as u64]),
    Field::new(40, 8, &[MAX_BLOCK_LEN as u64]),
    Field::new(48, 8, &[u64::MAX]),
    Field::new(56, 2, &[1]),
    Field::new(58, 2, &[ENTRY_LEN as u64]),
];

/// Entry flag values a draw favours: repeat, range, unhashed ([ww_tape::index::ENTRY_UNHASHED]), the first unknown
/// bit, every bit.
const ENTRY_FLAGS: &[u64] = &[1, 2, ww_tape::index::ENTRY_UNHASHED as u64, 8, 0xFF];

/// The fields of an index entry starting at `at`.
pub(crate) fn entry_fields(at: usize) -> [Field<'static>; 16] {
    [
        Field::new(at, 8, &[MAX_BLOCK_LEN as u64]),
        Field::new(at + 8, 4, &[MAX_BLOCK_LEN as u64]),
        Field::new(at + 12, 1, &[3, 4, 6, 7, 9]),
        Field::new(at + 13, 1, &[ww_tape_summary::NODE_SHIFT_MAX as u64]),
        Field::new(at + 14, 1, &[1, 2]),
        Field::new(at + 15, 1, ENTRY_FLAGS),
        Field::new(at + 16, 8, &[]),
        Field::new(at + 24, 4, &[]),
        Field::new(at + 32, 8, &[]),
        Field::new(at + 40, 8, &[]),
        Field::new(at + 48, 8, &[]),
        Field::new(at + 56, 8, &[]),
        Field::new(at + 64, 8, &[]),
        // the range pair, low above high among the draws
        Field::new(at + 72, 8, &[u64::MAX]),
        Field::new(at + 80, 8, &[0]),
        Field::new(at + 88, 8, &[]),
    ]
}

/// Compute an index block's CRCs again from what its head says: each track's entries CRC, the head CRC and the
/// block CRC; a head whose offsets don't fit keeps its old CRCs there.
pub(crate) fn reseal_index(b: &mut [u8]) {
    let entries_at = get(b, 32, 4) as usize;
    let tracks = get(b, 36, 4) as usize;
    let entry_len = get(b, 58, 2) as usize;
    let dir_fits = DIRECTORY
        .checked_add(tracks.saturating_mul(TRACK_LEN))
        .is_some_and(|end| end <= entries_at && end <= b.len());
    if dir_fits {
        for t in 0..tracks {
            let at = DIRECTORY + t * TRACK_LEN;
            let first = get(b, at + 8, 4) as usize;
            let count = get(b, at + 12, 4) as usize;
            let from = first
                .checked_mul(entry_len)
                .and_then(|o| o.checked_add(entries_at));
            let to = count
                .checked_mul(entry_len)
                .zip(from)
                .and_then(|(n, f)| n.checked_add(f));
            if let Some(entries) = from.zip(to).and_then(|(f, t)| span(b, f, t)) {
                let crc = crc32c(&[entries]);
                b[at + 16..at + 20].copy_from_slice(&crc.to_le_bytes());
            }
        }
    }
    if let (Some(a), Some(c)) = (span(b, 0, 28), span(b, 32, entries_at)) {
        let crc = crc32c(&[a, c]);
        b[28..32].copy_from_slice(&crc.to_le_bytes());
    }
    seal(b);
}

/// Entries of blocks drawn from the input, sorted by track for a final index.
fn entries(d: &mut Draw<'_>, n: usize, sorted: bool) -> Vec<IndexEntry> {
    let mut entries: Vec<IndexEntry> = (0..n)
        .map(|i| {
            let kind = [
                BlockKind::Events,
                BlockKind::Series,
                BlockKind::Summary,
                BlockKind::Records,
            ][d.below(4)];
            IndexEntry {
                offset: 64 * i as u64 + u64::from(d.u8()),
                len: 36 + u32::from(d.u8()),
                kind,
                level: d.u8() % 3,
                tree: d.u8() % 3,
                track_id: TrackId::new(u64::from(d.u8() % 4)),
                number: u32::from(d.u8() % 4),
                second: u32::from(d.u8() % 4),
                first_item: ItemNum::new(u64::from(d.u16())),
                count: 1 + u64::from(d.u8()),
                first_tick: i64::from(d.u16() as i16),
                last_tick: i64::from(i16::MAX) + i64::from(d.u8()),
                latest_end: i64::from(i16::MAX) + 256 + i64::from(d.u8()),
                repeat: d.bool(),
                relation_break: false,
                unit: ww_tape::UnitHash([d.u8(); 32]),
                unhashed: false,
                range: matches!(kind, BlockKind::Events | BlockKind::Series)
                    .then(|| {
                        let lo = u64::from(d.u8());
                        (lo, lo + (u64::from(d.u8()) << 8))
                    })
                    .filter(|_| d.bool()),
            }
        })
        .collect();
    if sorted {
        entries.sort_by_key(|e| (e.track_id, e.offset));
        entries.dedup_by_key(|e| (e.track_id, e.offset));
        // a final index lists a track's blocks without overlap: each block moves past the one before it
        for i in 1..entries.len() {
            let (before, rest) = entries.split_at_mut(i);
            let (p, e) = (&before[i - 1], &mut rest[0]);
            if p.track_id == e.track_id {
                e.offset = e.offset.max(p.offset + u64::from(p.len));
            }
        }
    }
    entries
}

/// A checkpoint or final index block of entries drawn from the input.
pub(crate) fn index_block(
    d: &mut Draw<'_>,
    id: BlockId,
    kind: IndexKind,
    entries: &[IndexEntry],
) -> Vec<u8> {
    let tracks = if kind == IndexKind::Final {
        track_count(entries)
    } else {
        0
    };
    let mut buf = vec![0u8; index_len(entries.len(), tracks)];
    let head = IndexHead {
        kind,
        self_offset: u64::from(d.u16()),
        prev: d.bool().then_some(u64::from(d.u8())),
    };
    let n = write_index(&mut buf, id, &head, entries).expect("a valid index");
    buf.truncate(n);
    buf
}

/// Hostile values over an index block's head, directory or entries.
pub(crate) fn hit_index(d: &mut Draw<'_>, b: &mut [u8]) {
    let tracks = get(b, 36, 4) as usize;
    let entries = get(b, 12, 4) as usize;
    let entries_at = get(b, 32, 4) as usize;
    match d.u8() % 10 {
        9 => hit(d, b, LEN_FIELD),
        1 => {
            let f = HEADER_FIELDS[d.below(HEADER_FIELDS.len())];
            hit(d, b, f);
        }
        2..5 => hit_some(d, b, &INDEX_FIELDS),
        5 | 6 => {
            let at = DIRECTORY + d.below(tracks + 1) * TRACK_LEN;
            let n = [entries as u64];
            let fields = [
                Field::new(at, 8, &[]),
                Field::new(at + 8, 4, &n),
                Field::new(at + 12, 4, &n),
            ];
            hit_some(d, b, &fields);
        }
        _ => {
            let entry_len = get(b, 58, 2) as usize;
            let at = entries_at + d.below(entries + 1) * entry_len;
            hit_some(d, b, &entry_fields(at));
        }
    }
}

fn ident(d: &mut Draw<'_>) -> PieceIdent {
    PieceIdent {
        id: PieceId([d.u8() | 1; 16]),
        owner: OwnerId([d.u8(); 32]),
        seq: u64::from(d.u8()),
    }
}

/// A close record of tracks drawn from the input, naming `offsets` (checkpoint, final index).
pub(crate) fn close_block(
    d: &mut Draw<'_>,
    id: BlockId,
    offsets: [Option<u64>; 2],
    hash: ContentHash,
) -> Vec<u8> {
    let tracks: Vec<TrackRange> = (0..d.below(3))
        .map(|t| TrackRange {
            track_id: TrackId::new(t as u64 + 1),
            track_number: TrackNum::new(t as u32 + 1),
            chunks: u32::from(d.u8()),
            first_item: ItemNum::new(u64::from(d.u16())),
            items: u64::from(d.u16()),
            first_tick: i64::from(d.u8()),
            last_tick: 300,
            latest_end: 400,
            owner: OwnerId([d.u8(); 32]),
            hash: TrackHash([d.u8(); 32]),
            records: u64::from(d.u16()),
            incomplete: d.bool(),
        })
        .collect();
    let close = Close {
        reason: CloseReason::Closed,
        hash,
        checkpoint: offsets[0],
        final_index: offsets[1],
        prev: None,
        summary: None,
        ident: d.bool().then(|| ident(d)),
        next: None,
    };
    let mut seq = 0u64;
    let removed: Vec<RemovedPiece> = (0..d.below(4))
        .map(|_| {
            seq += 1 + u64::from(d.u8() % 3);
            RemovedPiece {
                seq,
                id: PieceId([d.u8() | 1; 16]),
                hash: ContentHash([d.u8(); 32]),
            }
        })
        .collect();
    // 2 tracks, 3 removed rows and the most padding make a close of 1020 bytes: a builder's block stays within 1 KiB
    let padding = [0, 1, 384][d.below(3)];
    let mut buf = vec![0u8; close_len(tracks.len(), removed.len()) + padding];
    debug_assert!(buf.len() <= 1024, "a close of {} bytes", buf.len());
    let n = write_close(&mut buf, id, &close, (&tracks, &removed), padding).expect("a valid close");
    buf.truncate(n);
    buf
}

/// The fields of a close record with `tracks` and `removed` removed pieces.
pub(crate) fn close_fields<'c>(
    d: &mut Draw<'_>,
    (tracks, removed): (usize, usize),
    len: &'c [u64],
) -> Vec<Field<'c>> {
    let caps: &'static [u64] = &[u64::MAX, MAX_BLOCK_LEN as u64];
    let mut f = vec![
        Field::new(32, 1, &[1, 2, 3, 4]),
        Field::new(72, 8, caps),
        Field::new(80, 8, caps),
        Field::new(88, 8, caps),
        Field::new(160, 8, &[]),
        Field::new(168, 4, &[1]),
        Field::new(172, 4, &[4095, 4096, u32::MAX as u64]),
        Field::new(176, 8, caps),
        Field::new(184, 4, &[]),
        Field::new(34, 2, &[0, 128, 135, 137, 0xFFFF]),
        Field::new(36, 2, &[0, 55, 57, 0xFFFF]),
        Field::new(
            188,
            4,
            &[
                REMOVED_PIECES_MAX as u64,
                REMOVED_PIECES_MAX as u64 + 1,
                u32::MAX as u64,
            ],
        ),
    ];
    // a removed piece's seq: out of order, or one named twice
    let row = 192 + tracks * 136 + d.below(removed + 1) * 56;
    f.push(Field::new(row, 8, &[0, 1, u64::MAX]));
    let at = 192 + d.below(tracks + 1) * 136;
    f.extend([
        Field::new(at + 12, 4, &[]),
        Field::new(at + 16, 8, len),
        Field::new(at + 24, 8, len),
        Field::new(at + 32, 8, &[]),
        Field::new(at + 40, 8, &[]),
        Field::new(at + 48, 8, &[]),
        Field::new(at + 120, 8, &[]),
        Field::new(at + 128, 4, &[1, 2, 3]),
        Field::new(at + 132, 4, &[]),
    ]);
    f
}

/// Most bytes [widen_entries] adds to each entry.
const ENTRY_EXTRA_MAX: usize = 64;

/// The index block `b` with each entry `extra` bytes longer, the extra bytes `fill` and the head's entry length
/// saying so: a well-formed index of a later writer, once resealed.
fn widen_entries(b: &[u8], extra: usize, fill: u8) -> Vec<u8> {
    let entries_at = get(b, 32, 4) as usize;
    let n = get(b, 12, 4) as usize;
    let Some(entries) = span(b, entries_at, entries_at + n * ENTRY_LEN) else {
        return b.to_vec();
    };
    let mut out = b[..entries_at].to_vec();
    for e in entries.chunks(ENTRY_LEN) {
        out.extend_from_slice(e);
        out.extend(std::iter::repeat_n(fill, extra));
    }
    out.extend_from_slice(&[0; 4]);
    let len = out.len();
    out[8..12].copy_from_slice(&(len as u32).to_le_bytes());
    out[58..60].copy_from_slice(&((ENTRY_LEN + extra) as u16).to_le_bytes());
    out
}

/// A piece head block of tracks drawn from the input.
pub(crate) fn piece_head_block(d: &mut Draw<'_>, id: BlockId) -> Vec<u8> {
    let tracks: Vec<TrackStart> = (0..d.below(3))
        .map(|t| TrackStart {
            track_id: TrackId::new(t as u64 + 1),
            track_number: TrackNum::new(t as u32 + 1),
            next_item: ItemNum::new(u64::from(d.u16())),
            from_tick: i64::from(d.u16() as i16),
            owner: OwnerId([d.u8(); 32]),
            hash: TrackHash([d.u8(); 32]),
            incomplete: d.bool(),
            not_carried: d.bool(),
        })
        .collect();
    let head = PieceHead {
        ident: ident(d),
        prev: d
            .bool()
            .then(|| (ContentHash([d.u8(); 32]), PieceId([d.u8() | 1; 16]))),
        prev_session: SessionTag::from_u32(d.u32()),
        integrity: ww_tape::Integrity::Full,
    };
    let mut buf = vec![0u8; piece_head_len(tracks.len())];
    let n = write_piece_head(&mut buf, id, &head, &tracks).expect("a valid piece head");
    buf.truncate(n);
    buf
}

/// A tail naming a close of `close_len` bytes at `close_offset` in a piece of `piece_len` bytes.
pub(crate) fn tail_block(
    id: BlockId,
    piece_len: u64,
    close_offset: u64,
    close_len: u32,
    hash: ContentHash,
) -> Vec<u8> {
    let tail = Tail {
        piece_len,
        close_offset,
        close_len,
        reason: CloseReason::Closed,
        hash,
        ident: None,
        next: None,
    };
    let mut b = vec![0u8; TAIL_LEN];
    tail.write(&mut b, id);
    b
}

/// The fields of a tail.
pub(crate) const TAIL_FIELDS: [Field<'static>; 5] = [
    Field::new(32, 8, &[TAIL_LEN as u64, u64::MAX - 343]),
    Field::new(40, 8, &[u64::MAX - 343, u64::MAX]),
    Field::new(48, 4, &[u32::MAX as u64, 180]),
    Field::new(52, 1, &[1, 2, 3, 4]),
    Field::new(53, 1, &[1]),
];

/// An index, close, piece head or tail block drawn from the input, whose head, directory, entries, offsets or
/// tracks take hostile values.
pub fn index(data: &[u8]) -> Vec<u8> {
    let mut d = Draw::new(data);
    let id = BlockId::new(SESSION, u64::from(d.u8()));
    match d.u8() % 6 {
        0 => {
            let mut b = close_block(&mut d, id, [Some(0), Some(64)], ContentHash([7; 32]));
            let tracks = get(&b, 12, 4) as usize;
            let removed = get(&b, 188, 4) as usize;
            let len = [b.len() as u64];
            let fields = close_fields(&mut d, (tracks, removed), &len);
            hit_some(&mut d, &mut b, &fields);
            seal(&mut b);
            b
        }
        1 => {
            let mut b = piece_head_block(&mut d, id);
            let tracks = get(&b, 12, 4) as usize;
            let at = 152 + d.below(tracks + 1) * 96;
            let fields = [
                Field::new(88, 4, &[1, 2]),
                Field::new(92, 4, &[]),
                Field::new(144, 2, &[0, 95, 97, 0xFFFF]),
                // the integrity level: CrcOnly, None, the first unknown
                Field::new(146, 1, &[1, 2, 3]),
                Field::new(at, 8, &[]),
                Field::new(at + 16, 8, &[u64::MAX]),
                Field::new(at + 24, 8, &[]),
                Field::new(12, 4, &[]),
            ];
            hit_some(&mut d, &mut b, &fields);
            seal(&mut b);
            b
        }
        2 => {
            let mut b = tail_block(id, 2000, 1000, 400, ContentHash([3; 32]));
            hit_some(&mut d, &mut b, &TAIL_FIELDS);
            seal(&mut b);
            b
        }
        k => {
            let kind = if k % 2 == 0 {
                IndexKind::Final
            } else {
                IndexKind::Checkpoint
            };
            let n = d.below(8);
            let e = entries(&mut d, n, kind == IndexKind::Final);
            let mut b = index_block(&mut d, id, kind, &e);
            if d.u8().is_multiple_of(4) {
                b = widen_entries(&b, 1 + d.below(ENTRY_EXTRA_MAX), d.u8());
                reseal_index(&mut b);
            }
            hit_index(&mut d, &mut b);
            reseal_index(&mut b);
            b
        }
    }
}
