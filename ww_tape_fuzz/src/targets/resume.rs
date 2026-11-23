//! `TapeWriter::resume_on` on an unclosed piece a writer wrote, cut and mutated by the input: it never panics, and a
//! piece it resumes takes more blocks and closes whole: a reader finds it by its tail at the start of the bytes and
//! its content hash verifies. The bytes it cuts off hold no good block of the piece's session but puts belonging with
//! a block after them and what ends a piece.
//!
//! `PieceChain::resume_on` on a file of several pieces ([PieceTapes::One]) whose last piece is cut and mutated: it
//! never panics, and a chain it resumes keeps the pieces before byte for byte, takes more blocks and closes whole: a
//! reader finds every piece, each verifies, and they link up from the first to a complete end. A last piece the input
//! left closed but damaged is kept as it is: the chain starts a new piece behind it.
//!
//! Input: a mode byte (bit 0 aligned; bit 2 a file of several pieces; bit 1 no piece head for a single piece, the last
//! piece closed for a file of several; bits 3 to 4 the pieces' level: 0 Full, 1 CrcOnly, 2 and 3 None), a cut (u16 little endian, modulo the last piece's length + 1), then flips of
//! three bytes each (an offset, u16 little endian, modulo the cut last piece's length, and the bits to flip).
use std::sync::OnceLock;
use ww_tape::ItemNum;
use ww_tape::{ClockNum, TrackNum};

use shrink_wrap::prelude::{RefVec, UNib32};
use ww_tape::chunk::ChunkHead;
use ww_tape::index::IndexPolicy;
use ww_tape::piece::{OwnerId, PieceHead, PieceId, PieceIdent, TrackHash};
use ww_tape::reader::Found;
use ww_tape::writer::NearPolicy;
use ww_tape::{
    BlockId, BlockKind, BlockWriter, ChainOptions, ClockOrigin, ClockSpec, ClockTime, Codec,
    ColumnKind, ColumnSpec, CutPolicy, Definitions, EndMode, EventsDef, FullVersion, Layout,
    Magnitude, MemSink, MemTapes, PieceChain, PieceTapes, Segment, SessionTag, SyncPolicy,
    TapeReader, TapeSink, TapeWriter, TrackKind, TrackSpec, Version, WriterOptions, clock_key,
    track_key,
};

/// Rounds of blocks in a template piece.
const ROUNDS: u64 = 6;

pub(crate) fn options(aligned: bool) -> WriterOptions {
    WriterOptions {
        sync: SyncPolicy::None,
        clock: ww_tape::ClockRef::NONE,
        index: IndexPolicy {
            bytes: exact_units::Bytes::new(1024),
            interval: None,
            ..IndexPolicy::default()
        },
        layout: if aligned {
            Layout::ALIGNED
        } else {
            Layout::UNALIGNED
        },
        producer: "fuzz".to_owned(),
        integrity: ww_tape::Integrity::Full,
        near: NearPolicy {
            copy_spacing: exact_units::Bytes::new(2 << 10),
            memory_max: exact_units::Count::new(4),
            ..NearPolicy::default()
        },
    }
}

/// A records block of one record at tick `n` of clock `clock` on track `track`.
pub(crate) fn records(id: BlockId, clock: ClockNum, track: TrackNum, n: u64) -> Vec<u8> {
    let mut buf = [0u8; 128];
    let mut b = BlockWriter::new(&mut buf, BlockKind::Records, id, &[clock]).expect("a block fits");
    b.push(track, ClockTime::new(clock, n as i64), &[&[n as u8; 20]])
        .expect("a record fits");
    let len = b.finish();
    buf[..len].to_vec()
}

/// Track `t`'s chunk `k`: 16 items with raw starts, on the track of key index `t + 1`, numbered `number`.
pub(crate) fn chunk(t: u64, number: TrackNum, k: u64) -> (ChunkHead, Vec<u8>) {
    let first = k * 16;
    let starts: Vec<u8> = (first..first + 16)
        .flat_map(|i| (i as i64 * 4 + t as i64).to_le_bytes())
        .collect();
    let head = ChunkHead {
        kind: BlockKind::Events,
        track_id: track_key(0xf2, &[Segment::Index(t as u32 + 1)]).id(),
        track_number: number,
        first_item: ItemNum::new(first),
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

pub(crate) fn write_chunk<S: TapeSink>(
    w: &mut TapeWriter<S>,
    defs: &Definitions,
    on: Defined,
    t: u64,
    k: u64,
) {
    let (h, c) = chunk(t, on.chunks[t as usize], k);
    let spec = ColumnSpec {
        kind: ColumnKind::Starts,
        codec: Codec::RAW,
        param: 0,
        len: c.len(),
    };
    w.write_chunk(defs, &h, &[(spec, &c)])
        .expect("a chunk writes");
}

/// The level of mode bits 3 to 4.
pub(crate) fn level(mode: u8) -> ww_tape::Integrity {
    match (mode >> 3) & 3 {
        0 => ww_tape::Integrity::Full,
        1 => ww_tape::Integrity::CrcOnly,
        _ => ww_tape::Integrity::None,
    }
}

/// An unclosed piece of `level`: rounds of chunks, records and deferred records, a clock and a track defined after
/// half of them; the writer puts the definitions each block needs before it.
fn template(aligned: bool, head: bool, level: ww_tape::Integrity) -> Vec<u8> {
    let o = options(aligned);
    let mut w = TapeWriter::create_on(MemSink::new(), o.sync);
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
            integrity: level,
        };
        w.begin_piece(&h, &[]).expect("a head writes");
    }
    let (mut defs, on) = template_definitions();
    for k in 0..ROUNDS {
        if k == ROUNDS / 2 {
            let c = defs.define_clock(clock(2)).expect("the clock holds");
            let t = defs.define_track(track(10, c)).expect("the track holds");
            assert_eq!((c.get(), t.get()), (2, 10), "the numbers of the seeds");
        }
        write_chunk(&mut w, &defs, on, 0, k);
        w.write_block(&defs, &records(w.next_id(), on.clock, on.records, k))
            .expect("a records block writes");
        write_chunk(&mut w, &defs, on, 1, k);
        w.defer_block(&defs, records(w.next_id(), on.clock, on.records, 100 + k))
            .expect("a deferred block");
    }
    w.flush_deferred().expect("deferred blocks write");
    w.sink().tape().to_vec().expect("the bytes")
}

/// The owner of the fuzzed chains.
const OWNER: OwnerId = OwnerId([1; 32]);

/// The tape of a chain in memory: a chain in one tape is under the key of its first piece.
const CHAIN_TAPE: u64 = 0;

pub(crate) fn chain_options(aligned: bool, level: ww_tape::Integrity) -> ChainOptions {
    ChainOptions {
        tapes: PieceTapes::One,
        writer: WriterOptions {
            integrity: level,
            ..options(aligned)
        },
        cut: CutPolicy {
            chunks: exact_units::Count::new(4),
            ..CutPolicy::default()
        },
    }
}

pub(crate) fn chain_chunk(
    c: &mut PieceChain<MemTapes>,
    defs: &Definitions,
    on: Defined,
    t: u64,
    k: u64,
) {
    let (h, b) = chunk(t, on.chunks[t as usize], k);
    let spec = ColumnSpec {
        kind: ColumnKind::Starts,
        codec: Codec::RAW,
        param: 0,
        len: b.len(),
    };
    c.write_chunk(defs, &h, &[(spec, &b)])
        .expect("a chunk writes");
}

pub(crate) fn chain_records(c: &mut PieceChain<MemTapes>, defs: &Definitions, on: Defined, n: u64) {
    let w = c.writer().expect("a chain writing");
    let id = w.next_id();
    w.write_block(defs, &records(id, on.clock, on.records, n))
        .expect("a records block writes");
}

/// A file of several pieces of `level`: three closed, the last one closed too when `closed`, else stopped after its
/// deferred blocks; the bytes and where the last piece starts.
pub(crate) fn chain_template(
    aligned: bool,
    closed: bool,
    level: ww_tape::Integrity,
) -> (Vec<u8>, usize) {
    let options = chain_options(aligned, level);
    let mut c =
        PieceChain::create_on(MemTapes::new(), OWNER, 0, &[], options).expect("a chain starts");
    let (defs, on) = template_definitions();
    for k in 0..7 {
        chain_chunk(&mut c, &defs, on, 0, k);
        chain_records(&mut c, &defs, on, k);
        chain_chunk(&mut c, &defs, on, 1, k);
    }
    let w = c.writer().expect("a chain writing");
    w.flush_deferred().expect("deferred blocks write");
    let last = w.sink().base();
    let mem = c.tapes().clone();
    if closed {
        c.close().expect("a chain closes");
    }
    let bytes = mem
        .get(&CHAIN_TAPE)
        .expect("memory")
        .expect("the file")
        .to_vec()
        .expect("the bytes");
    (bytes, usize::try_from(last).expect("a small file"))
}

/// The closed chain of [chain_template] with its second piece removed and named so by a record appended to its last
/// piece: a seed of the readers and of `check_chain` linking over removed pieces.
pub(crate) fn removed_chain() -> Vec<u8> {
    let (whole, _) = chain_template(false, true, ww_tape::Integrity::Full);
    let r = TapeReader::open(&whole[..]).expect("a closed chain opens");
    let spans: Vec<(usize, usize)> = r
        .pieces()
        .iter()
        .map(|p| {
            let at = |v: u64| usize::try_from(v).expect("a small file");
            (at(p.start), at(p.end))
        })
        .collect();
    assert!(spans.len() >= 3, "a chain of three pieces or more");
    let gone = r.pieces()[1].summary().expect("a piece with a head");
    let (_, hash, _) = gone.end.expect("a closed piece");
    let (from, to) = spans[spans.len() - 1];
    let mut sink = MemSink::new();
    sink.write_all(&whole[from..to])
        .expect("memory takes the bytes");
    let (mut w, _) =
        TapeWriter::resume_on(sink, options(false)).expect("the closed last piece resumes");
    w.record_removed(&[ww_tape::piece::RemovedPiece {
        seq: gone.ident.seq,
        id: gone.ident.id,
        hash,
    }])
    .expect("an earlier piece is named");
    let tape = w.sink().tape();
    w.close().expect("the record closes");
    let mut out = whole[spans[0].0..spans[0].1].to_vec();
    for (a, b) in &spans[2..spans.len() - 1] {
        out.extend_from_slice(&whole[*a..*b]);
    }
    out.extend(tape.to_vec().expect("the bytes"));
    let chain = TapeReader::open(&out[..]).expect("the chain opens").chain();
    assert!(chain.is_whole(), "{chain:?}");
    out
}

/// Events track `index` on clock `clock`: its key is the index's, the number its template gives it.
pub(crate) fn track(index: u32, clock: ClockNum) -> TrackSpec<'static> {
    TrackSpec {
        key: track_key(0xf2, &[Segment::Index(index)]),
        clock,
        dimensions: RefVec::Slice { slice: &[] },
        items: ww_tape::Items::WireWeaver {
            api: FullVersion::new("fuzz_api", Version::new(0, 1, 0)),
            bundle: RefVec::Slice { slice: &[] },
            path: RefVec::Slice {
                slice: &[UNib32(1)],
            },
            keys: RefVec::Slice { slice: &[] },
            item: None,
        },
        label: "fuzz",
        repeat: ww_tape::TrackRepeat::Stream,
        kind: TrackKind::Events(EventsDef::INSTANTS),
        relations: RefVec::Slice { slice: &[] },
        owner: [0; 32],
    }
}

/// Clock `index`: its key is the index's, the number its template gives it.
pub(crate) fn clock(index: u32) -> ClockSpec<'static> {
    ClockSpec {
        key: clock_key(0xf2, &[], index.into()),
        rate: Magnitude::pow10(9).into(),
        resolution: Magnitude::pow10(-9).into(),
        origin: ClockOrigin::Boot,
        label: "host",
    }
}

/// The numbers the data of a template or a resume is written on: its clock, the tracks of the chunks of track
/// indexes 0 and 1 and the track of the records.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Defined {
    pub(crate) clock: ClockNum,
    pub(crate) chunks: [TrackNum; 2],
    pub(crate) records: TrackNum,
}

/// Clock 1 and tracks 1, 2 and 9 on it, under those numbers: the template's definitions, as its seeds were written.
pub(crate) fn template_definitions() -> (Definitions, Defined) {
    let mut defs = Definitions::new();
    let c = defs.define_clock(clock(1)).expect("the clock holds");
    let chunks = [1, 2].map(|i| defs.define_track(track(i, c)).expect("the track holds"));
    let records = super::define_track_as(&mut defs, &track(9, c), TrackNum::new(9));
    assert_eq!(
        (c.get(), chunks.map(TrackNum::get)),
        (1, [1, 2]),
        "the numbers of the seeds"
    );
    (
        defs,
        Defined {
            clock: c,
            chunks,
            records,
        },
    )
}

/// Clock 1 and tracks 1 and 9 on it, numbered as the piece `resumed` read numbers them: the definitions a resume
/// writes on with, its chunks all on track 1.
fn resumed_definitions(resumed: &ww_tape::Resumed) -> (Definitions, Defined) {
    let mut defs = Definitions::with_numbers(resumed.numbers.clone());
    let c = defs.define_clock(clock(1)).expect("the clock holds");
    let first = defs.define_track(track(1, c)).expect("the track holds");
    let records = defs.define_track(track(9, c)).expect("the track holds");
    (
        defs,
        Defined {
            clock: c,
            chunks: [first, first],
            records,
        },
    )
}

/// The templates by mode, made once: the bytes and where the last piece starts.
fn templates() -> &'static [(Vec<u8>, usize); 32] {
    static T: OnceLock<[(Vec<u8>, usize); 32]> = OnceLock::new();
    T.get_or_init(|| {
        core::array::from_fn(|m| {
            let level = level(m as u8);
            if m & 4 == 0 {
                (template(m & 1 != 0, m & 2 == 0, level), 0)
            } else {
                chain_template(m & 1 != 0, m & 2 != 0, level)
            }
        })
    })
}

pub fn resume(data: &[u8]) {
    let (mode, rest) = data.split_first().map_or((0, data), |(m, r)| (*m, r));
    let aligned = mode & 1 != 0;
    let (template, last) = &templates()[usize::from(mode & 31)];
    let mut bytes = template.clone();
    let (cut, flips) = match rest {
        [a, b, flips @ ..] => (usize::from(u16::from_le_bytes([*a, *b])), flips),
        _ => (usize::MAX, rest),
    };
    let piece = bytes.len() - last;
    bytes.truncate(last + cut % (piece + 1));
    let piece = bytes.len() - last;
    if piece != 0 {
        for [lo, hi, bits] in flips.as_chunks::<3>().0 {
            let at = last + usize::from(u16::from_le_bytes([*lo, *hi])) % piece;
            bytes[at] ^= bits;
        }
    }
    if mode & 4 != 0 {
        let open = (mode & 2 == 0).then(|| &template[*last..]);
        resume_chain((aligned, level(mode)), &bytes, *last, open);
        return;
    }
    let mut sink = MemSink::new();
    sink.write_all(&bytes).expect("memory takes the bytes");
    let tape = sink.tape();
    let Ok((mut w, resumed)) = TapeWriter::resume_on(sink, options(aligned)) else {
        return;
    };
    assert!(resumed.kept_bytes <= bytes.len() as u64);
    no_good_block_cut(&template[..], &bytes, resumed.kept_bytes);
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
    let (defs, on) = resumed_definitions(&resumed);
    for k in 0..2 {
        w.write_block(&defs, &records(w.next_id(), on.clock, on.records, 200 + k))
            .expect("a resumed piece takes blocks");
        write_chunk(&mut w, &defs, on, 0, ROUNDS + k);
    }
    let closed = w.close().expect("a resumed piece closes");
    let out = tape.to_vec().expect("the bytes");
    assert_eq!(closed.len, out.len() as u64);
    let r = TapeReader::open(&out[..]).expect("a closed piece opens");
    let last = r.pieces().len().checked_sub(1).expect("a piece");
    let p = &r.pieces()[last];
    assert_eq!(
        (p.start, p.found),
        (0, Found::Tail),
        "found whole by its tail"
    );
    assert!(
        r.verify(last).map(|v| v.holds).expect("a closed piece"),
        "the content hash verifies"
    );
    folds_alike(
        &out,
        r.pieces()[last]
            .close
            .as_ref()
            .and_then(|f| f.0.final_index),
    );
}

/// Track 0's range hash folded from the index of the closed piece `out` and walked from its chunks with the final
/// index at `final_index` made unreadable: when the walk explains every byte, both fold the same chunks to the same
/// hash.
fn folds_alike(out: &[u8], final_index: Option<u64>) {
    let Some(at) = final_index.and_then(|a| usize::try_from(a).ok()) else {
        return;
    };
    let id = track_key(0xf2, &[Segment::Index(1)]).id();
    let hash = |b: &[u8]| {
        TapeReader::open(b)
            .ok()?
            .range_hash(
                id,
                OwnerId::NONE,
                ItemNum::new(0)..ItemNum::new(u64::MAX),
                TrackHash::START,
            )
            .ok()
    };
    let Some(indexed) = hash(out) else {
        return;
    };
    assert!(
        indexed.chunks >= 2,
        "the chunks written after the resume: {indexed:?}"
    );
    let mut walked = out.to_vec();
    // a byte of the index head: its CRC fails, the reader walks the chunks
    walked[at + 40] ^= 1;
    if let Some(w) = hash(&walked)
        && w.skipped == 0
    {
        assert_eq!(
            (w.hash, w.items, w.chunks),
            (indexed.hash, indexed.items, indexed.chunks),
            "folded from the index and from the chunks"
        );
    }
}

/// Resume the chain of `bytes`, whose pieces before `last` are whole, written at `level`, and close it; `open` the
/// last piece before the input mutated it, when it was unclosed.
fn resume_chain(
    (aligned, level): (bool, ww_tape::Integrity),
    bytes: &[u8],
    last: usize,
    open: Option<&[u8]>,
) {
    let mem = MemTapes::new();
    mem.create_whole(&CHAIN_TAPE)
        .expect("memory takes a file")
        .write_all(bytes)
        .expect("memory takes the bytes");
    let options = chain_options(aligned, level);
    let Ok((mut c, resumed)) = PieceChain::resume_on(mem, OWNER, 0, options) else {
        return;
    };
    if let (Some(template), Some(piece)) = (open, bytes.get(last..)) {
        no_good_block_cut(template, piece, resumed.kept_bytes);
    }
    let (defs, on) = resumed_definitions(&resumed);
    for k in 0..6 {
        chain_records(&mut c, &defs, on, 200 + k);
        chain_chunk(&mut c, &defs, on, 0, 100 + k);
    }
    let mem = c.tapes().clone();
    let done = c.close().expect("a resumed chain closes");
    let out = mem
        .get(&CHAIN_TAPE)
        .expect("memory")
        .expect("the file")
        .to_vec()
        .expect("the bytes");
    assert_eq!(out[..last], bytes[..last], "the pieces before stay");
    let end = done.last().map(|f| f.piece.start + f.piece.len);
    assert_eq!(end, Some(out.len() as u64), "the last piece ends the file");
    let r = TapeReader::open(&out[..]).expect("a closed chain opens");
    let n = r.pieces().len();
    assert!(n >= 4, "{n} pieces");
    // a last piece the input left closed stays as the input made it, damage included: resume starts behind it
    let kept = (bytes.len() > last && out.get(last..bytes.len()) == Some(&bytes[last..]))
        .then(|| r.pieces().iter().position(|p| p.start == last as u64))
        .flatten();
    let damaged = kept.filter(|i| !r.verify(*i).map(|v| v.holds).expect("a closed piece"));
    for (i, p) in r.pieces().iter().enumerate() {
        assert_eq!(p.found, Found::Tail, "piece {i} found whole by its tail");
        if Some(i) != damaged {
            assert!(
                r.verify(i).map(|v| v.holds).expect("a closed piece"),
                "piece {i} verifies"
            );
        }
    }
    if damaged.is_none() {
        let chain = r.chain();
        assert!(chain.is_whole(), "{chain:?}");
    }
}

/// The bytes of `piece` past `kept`, which a resume cut off, hold no good block of the session of the piece head at the
/// start of `template` (the piece before the input mutated it), but puts belonging with a block after them
/// ([ww_tape::FLAG_WITH_NEXT]) and what ends a piece (a final index, a close, a tail).
fn no_good_block_cut(template: &[u8], piece: &[u8], kept: u64) {
    let Some(session) = template.get(24..28) else {
        return;
    };
    let from = usize::try_from(kept).expect("a small piece");
    for at in from..piece.len() {
        let Some(header) = piece.get(at..at + ww_tape::HEADER_LEN) else {
            break;
        };
        if header[..4] != ww_tape::MAGIC || header[24..28] != *session {
            continue;
        }
        let Some(block) = ww_tape::block_len(header)
            .ok()
            .and_then(|len| piece.get(at..at + len))
            .and_then(|b| ww_tape::Block::read(b).ok())
        else {
            continue;
        };
        let ends = match block.kind {
            BlockKind::Close | BlockKind::Tail => true,
            BlockKind::Index => ww_tape::index::IndexBlock::read(&block)
                .is_ok_and(|ix| ix.kind() == ww_tape::index::IndexKind::Final),
            _ => false,
        };
        assert!(
            ends || block.flags & ww_tape::FLAG_WITH_NEXT != 0,
            "a resume cut off a good block at {at} of {}, kept {kept}: {:?}",
            piece.len(),
            block.kind
        );
    }
}
