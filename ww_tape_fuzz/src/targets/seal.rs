//! `MemPieces::seal` of a closed piece or chain written at a level below Full, mutated by the input. It never panics.
//! Unmutated, the sealed tape is the one a writer of Full writes from the same calls. Mutated, a seal refuses and
//! leaves the tape as it was, leaves a tape of Full pieces as it is, or gives a tape whose pieces are all of Full and
//! verify.
//!
//! Input: a mode byte (bits 0 to 1 the level: 0 Full, 1 CrcOnly, 2 and 3 None; bit 2 aligned; bit 3 a chain of
//! several pieces in one tape, else one piece closed, written on after its close and closed again), then flips of
//! three bytes each (an offset, u16 little endian, modulo the tape's length, and the bits to flip).
use std::sync::OnceLock;

use ww_tape::piece::{CloseReason, OwnerId, PieceHead, PieceId, PieceIdent};
use ww_tape::{
    ChainOptions, Integrity, MemPieces, MemSink, PieceChain, SessionTag, TapeReader, TapeSink,
    TapeWriter,
};

use super::resume::{
    chain_chunk, chain_options, chain_records, options, records, template_definitions, write_chunk,
};

/// Rounds of blocks before and after the close of a single piece.
const ROUNDS: u64 = 4;

/// The level of mode bits 0 to 1.
fn level(mode: u8) -> Integrity {
    match mode & 3 {
        0 => Integrity::Full,
        1 => Integrity::CrcOnly,
        _ => Integrity::None,
    }
}

/// A piece of `level`: rounds of chunks and records, closed with a close summary, then written on after its close
/// with more rounds and closed again.
fn piece(aligned: bool, level: Integrity) -> Vec<u8> {
    let mut o = options(aligned);
    let mut w = TapeWriter::create_on(MemSink::new(), o.sync);
    w.set_layout(o.layout).expect("a new piece takes a layout");
    w.set_producer(&o.producer);
    w.set_index_policy(o.index)
        .expect("a policy with room for a piece head");
    w.set_near_policy(o.near);
    let owner = OwnerId([1; 32]);
    let head = PieceHead {
        ident: PieceIdent {
            id: PieceId::first(&owner),
            owner,
            seq: 0,
        },
        prev: None,
        prev_session: SessionTag::NONE,
        integrity: level,
        unlisted_keys_unknown: false,
    };
    w.begin_piece(&head, &[]).expect("a head writes");
    let (defs, on) = template_definitions();
    for k in 0..ROUNDS {
        write_chunk(&mut w, &defs, on, 0, k);
        w.write_block(&defs, &records(w.next_id(), on.clock, on.records, k))
            .expect("a records block writes");
        write_chunk(&mut w, &defs, on, 1, k);
    }
    let tape = w.sink().tape();
    w.close_with_summary(CloseReason::Closed, b"seal")
        .expect("a piece closes");
    let mem = MemPieces::new();
    let mut sink = mem.create(&0).expect("a new tape");
    sink.write_all(&tape.to_vec().expect("the bytes"))
        .expect("memory takes the bytes");
    sink.close().expect("a tape closes");
    drop(sink);
    let sink = mem.reopen(&0).expect("memory").expect("the tape");
    o.integrity = level;
    let (mut w, _) = TapeWriter::resume_on(sink, o).expect("a closed piece is written on");
    for k in ROUNDS..2 * ROUNDS {
        write_chunk(&mut w, &defs, on, 0, k);
        w.write_block(&defs, &records(w.next_id(), on.clock, on.records, k))
            .expect("a records block writes");
    }
    w.close().expect("a piece closes");
    mem.get(&0)
        .expect("memory")
        .expect("the tape")
        .to_vec()
        .expect("the bytes")
}

/// A chain of `level` in one tape: chunks and records over several pieces, closed.
fn chain(aligned: bool, level: Integrity) -> Vec<u8> {
    let o: ChainOptions = chain_options(aligned, level);
    let mut c = PieceChain::create_on(MemPieces::new(), OwnerId([1; 32]), 0, &[], o)
        .expect("a chain starts");
    let (defs, on) = template_definitions();
    for k in 0..7 {
        chain_chunk(&mut c, &defs, on, 0, k);
        chain_records(&mut c, &defs, on, k);
        chain_chunk(&mut c, &defs, on, 1, k);
    }
    let mem = c.store().clone();
    c.close().expect("a chain closes");
    mem.get(&0)
        .expect("memory")
        .expect("the tape")
        .to_vec()
        .expect("the bytes")
}

/// The tapes by mode bits 0 to 3, made once.
fn templates() -> &'static [Vec<u8>; 16] {
    static T: OnceLock<[Vec<u8>; 16]> = OnceLock::new();
    T.get_or_init(|| {
        core::array::from_fn(|m| {
            let m = m as u8;
            match m & 8 != 0 {
                true => chain(m & 4 != 0, level(m)),
                false => piece(m & 4 != 0, level(m)),
            }
        })
    })
}

pub fn seal(data: &[u8]) {
    let (mode, flips) = data.split_first().map_or((0, data), |(m, r)| (*m, r));
    let template = &templates()[usize::from(mode & 15)];
    let mut bytes = template.clone();
    for [lo, hi, bits] in flips.as_chunks::<3>().0 {
        let at = usize::from(u16::from_le_bytes([*lo, *hi])) % bytes.len();
        bytes[at] ^= bits;
    }
    let mem = MemPieces::new();
    let mut sink = match mode & 8 != 0 {
        true => mem.create_whole(&0),
        false => mem.create(&0),
    }
    .expect("a new tape");
    sink.write_all(&bytes).expect("memory takes the bytes");
    sink.close().expect("a tape closes");
    drop(sink);
    let sealed = mem.seal(&0, None);
    let got = mem
        .get(&0)
        .expect("memory")
        .expect("the tape")
        .to_vec()
        .expect("the bytes");
    if bytes == *template {
        let full = &templates()[usize::from(mode & 12)];
        assert!(got == *full, "a sealed tape is Full's");
        let sealed = sealed.expect("an unmutated tape seals").expect("the tape");
        assert_eq!(sealed.is_empty(), level(mode) == Integrity::Full);
        return;
    }
    match sealed {
        Err(_) => assert!(got == bytes, "a refused seal leaves the tape"),
        Ok(Some(s)) if s.is_empty() => assert!(got == bytes, "a tape of Full stays as it is"),
        Ok(Some(_)) => {
            let r = TapeReader::open(&got[..]).expect("a sealed tape opens");
            for i in 0..r.pieces().len() {
                assert_eq!(r.pieces()[i].integrity(), Integrity::Full);
                assert!(
                    r.verify(i).expect("a sealed piece verifies").holds,
                    "a sealed piece holds against its index"
                );
            }
        }
        Ok(None) => unreachable!("the tape is there"),
    }
}
