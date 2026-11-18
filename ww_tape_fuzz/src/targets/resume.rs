//! `TapeWriter::resume_on` on an unfinished piece a writer wrote, cut and mutated by the input: it never panics, and a
//! piece it resumes takes more blocks and finishes whole: a reader finds it by its tail at the start of the bytes and
//! its content hash verifies.
//!
//! Input: a mode byte (bit 0 aligned, bit 1 no piece head), a cut (u16 little endian, modulo the piece's length + 1),
//! then flips of three bytes each (an offset, u16 little endian, modulo the cut length, and the bits to flip).
use std::path::Path;
use std::sync::OnceLock;

use shrink_wrap::prelude::{RefVec, UNib32};
use ww_tape::chunk::ChunkHead;
use ww_tape::index::IndexPolicy;
use ww_tape::piece::{OwnerId, PieceHead, PieceId, PieceIdent};
use ww_tape::reader::Found;
use ww_tape::writer::NearPolicy;
use ww_tape::{
    BlockId, BlockKind, BlockWriter, ClockDef, ClockOrigin, ClockTime, Codec, ColumnKind,
    ColumnSpec, Definitions, Describe, EndMode, EventsDef, FullVersion, Layout, Magnitude, MemSink,
    Segment, SessionTag, SyncPolicy, TapeReader, TapeSink, TapeWriter, TrackDef, TrackKind,
    Version, WriterOptions, clock_key, track_key,
};

/// Rounds of blocks in a template piece.
const ROUNDS: u64 = 6;

fn options(aligned: bool) -> WriterOptions {
    WriterOptions {
        sync: SyncPolicy::None,
        index: IndexPolicy {
            bytes: 1024,
            interval: None,
            ..IndexPolicy::default()
        },
        layout: if aligned {
            Layout::ALIGNED
        } else {
            Layout::UNALIGNED
        },
        producer: "fuzz".to_owned(),
        near: NearPolicy {
            copy_spacing: 2 << 10,
            memory_max: 4,
        },
    }
}

fn records(id: BlockId, n: u64) -> Vec<u8> {
    let mut buf = [0u8; 128];
    let mut b = BlockWriter::new(&mut buf, BlockKind::Records, id, &[1]).expect("a block fits");
    b.push(9, ClockTime::new(1, n as i64), &[&[n as u8; 20]])
        .expect("a record fits");
    let len = b.finish();
    buf[..len].to_vec()
}

/// Track `t`'s chunk `k`: 16 items with raw starts.
fn chunk(t: u64, k: u64) -> (ChunkHead, Vec<u8>) {
    let first = k * 16;
    let starts: Vec<u8> = (first..first + 16)
        .flat_map(|i| (i as i64 * 4 + t as i64).to_le_bytes())
        .collect();
    let head = ChunkHead {
        kind: BlockKind::Events,
        track_id: 0x1000 + t,
        track_number: t as u32 + 1,
        first_item: first,
        count: 16,
        first_tick: first as i64 * 4 + t as i64,
        last_tick: (first + 15) as i64 * 4 + t as i64,
        latest_end: (first + 15) as i64 * 4 + t as i64,
        time_mode: 0,
        end_mode: EndMode::Absent,
        grid: None,
    };
    (head, starts)
}

fn write_chunk<S: TapeSink>(w: &mut TapeWriter<S>, defs: &Definitions, t: u64, k: u64) {
    let (h, c) = chunk(t, k);
    let spec = ColumnSpec {
        kind: ColumnKind::Starts,
        codec: Codec::RAW,
        param: 0,
        len: c.len(),
    };
    w.write_chunk(defs, &h, &[(spec, &c)])
        .expect("a chunk writes");
}

/// An unfinished piece: rounds of chunks, records and deferred records, a clock and a track defined after half of
/// them; the writer puts the definitions each block needs before it.
fn template(aligned: bool, head: bool) -> Vec<u8> {
    let o = options(aligned);
    let mut w = TapeWriter::create_on(MemSink::new(Path::new("t")), o.sync);
    w.set_layout(o.layout).expect("a new piece takes a layout");
    w.set_producer(&o.producer);
    w.set_index_policy(o.index)
        .expect("a policy with room for a piece head");
    w.set_near_policy(o.near);
    if head {
        let owner = OwnerId([1; 32]);
        let h = PieceHead {
            ident: PieceIdent {
                id: PieceId::first(&owner),
                owner,
                seq: 0,
            },
            prev: None,
            prev_session: SessionTag::NONE,
        };
        w.begin_piece(&h, &[]).expect("a head writes");
    }
    let mut defs = Definitions::new();
    defs.define_all(&[clock(1), track(1, 1), track(2, 1), track(9, 1)])
        .expect("the definitions hold");
    for k in 0..ROUNDS {
        if k == ROUNDS / 2 {
            defs.define_all(&[clock(2), track(10, 2)])
                .expect("the definitions hold");
        }
        write_chunk(&mut w, &defs, 0, k);
        w.write_block(&defs, &records(w.next_id(), k))
            .expect("a records block writes");
        write_chunk(&mut w, &defs, 1, k);
        w.defer_block(&defs, records(w.next_id(), 100 + k))
            .expect("a deferred block");
    }
    w.flush_deferred().expect("deferred blocks write");
    w.sink().tape().to_vec().expect("the bytes")
}

/// Events track `number` on clock `clock`.
fn track(number: u32, clock: u32) -> Describe<'static> {
    Describe::Track(TrackDef {
        number: UNib32(number),
        key: track_key(0xf2, &[Segment::Index(number)]),
        clock: UNib32(clock),
        dimensions: RefVec::Slice { slice: &[] },
        api: FullVersion::new("fuzz_api", Version::new(0, 1, 0)),
        bundle: RefVec::Slice { slice: &[] },
        path: RefVec::Slice {
            slice: &[UNib32(1)],
        },
        keys: RefVec::Slice { slice: &[] },
        item: None,
        label: "fuzz",
        repeat: ww_tape::TrackRepeat::Stream,
        kind: TrackKind::Events(EventsDef::INSTANTS),
        parent: None,
        context: RefVec::Slice { slice: &[] },
        entity_key: None,
    })
}

fn clock(number: u32) -> Describe<'static> {
    Describe::Clock(ClockDef {
        number: UNib32(number),
        key: clock_key(0xf2, &[], number.into()),
        rate: Magnitude::pow10(9).into(),
        resolution: Magnitude::pow10(-9).into(),
        origin: ClockOrigin::Boot,
        label: "host",
    })
}

/// The template pieces by mode, made once.
fn templates() -> &'static [Vec<u8>; 4] {
    static T: OnceLock<[Vec<u8>; 4]> = OnceLock::new();
    T.get_or_init(|| core::array::from_fn(|m| template(m & 1 != 0, m & 2 == 0)))
}

pub fn resume(data: &[u8]) {
    let (mode, rest) = data.split_first().map_or((0, data), |(m, r)| (*m, r));
    let aligned = mode & 1 != 0;
    let mut bytes = templates()[usize::from(mode & 3)].clone();
    let (cut, flips) = match rest {
        [a, b, flips @ ..] => (usize::from(u16::from_le_bytes([*a, *b])), flips),
        _ => (usize::MAX, rest),
    };
    bytes.truncate(cut % (bytes.len() + 1));
    if !bytes.is_empty() {
        for [lo, hi, bits] in flips.as_chunks::<3>().0 {
            let at = usize::from(u16::from_le_bytes([*lo, *hi])) % bytes.len();
            bytes[at] ^= bits;
        }
    }
    let mut sink = MemSink::new(Path::new("p"));
    sink.write_all(&bytes).expect("memory takes the bytes");
    let tape = sink.tape();
    let Ok((mut w, resumed)) = TapeWriter::resume_on(sink, options(aligned)) else {
        return;
    };
    assert!(resumed.kept_bytes <= bytes.len() as u64);
    assert_eq!(
        resumed.kept_bytes + resumed.dropped_bytes,
        bytes.len() as u64
    );
    assert!(
        resumed
            .entries
            .iter()
            .all(|e| e.offset < resumed.kept_bytes)
    );
    let mut defs = Definitions::new();
    defs.define_all(&[clock(1), track(1, 1), track(9, 1)])
        .expect("the definitions hold");
    for k in 0..2 {
        w.write_block(&defs, &records(w.next_id(), 200 + k))
            .expect("a resumed piece takes blocks");
        write_chunk(&mut w, &defs, 0, ROUNDS + k);
    }
    let finished = w.finish().expect("a resumed piece finishes");
    let out = tape.to_vec().expect("the bytes");
    assert_eq!(finished.len, out.len() as u64);
    let r = TapeReader::open(&out[..]).expect("a finished piece opens");
    let last = r.pieces().len().checked_sub(1).expect("a piece");
    let p = &r.pieces()[last];
    assert_eq!(
        (p.start, p.found),
        (0, Found::Tail),
        "found whole by its tail"
    );
    assert!(
        r.verify(last).expect("a finished piece"),
        "the content hash verifies"
    );
}
