//! A piece closed more than once, its last append torn and mutated by the input, read and resumed. The reader never
//! panics and takes the newest close whose close record and tail are whole and untouched, never a torn or damaged
//! one; the closes it lists each name the one before. A resume keeps every byte up to the close before the last
//! append and cuts only a torn end: with one byte changed, a byte it drops lies in the record the cut stops inside
//! of. It takes more blocks and closes whole: the piece then holds each close kept, in order, and verifies when no
//! flip fell before its last kept close. Damage in a whole record refuses.
//!
//! With flips anywhere in the piece (mode bit 1), only the reader's and the resume's own checks hold: no panic, closes
//! listed oldest first, each naming the one before.
//!
//! Input: a mode byte (bit 0 aligned, bit 1 flips anywhere, bits 2 to 3 the appends after the first close: 1 to 4),
//! a byte per append (bits 0 to 2 its record blocks), a cut (u16 little endian, modulo the last append's length + 1),
//! then flips of three bytes each (an offset, u16 little endian, modulo the bytes they may touch, and the bits).
use ww_tape::piece::{Close, OwnerId, PieceHead, PieceId, PieceIdent};
use ww_tape::{Definitions, MemSink, SessionTag, TapeReader, TapeSink, TapeWriter};

use super::resume::{clock, options, records, track};

/// A close the writer made: where its close record starts and where its tail ends, in the piece.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Made {
    pub(crate) close: u64,
    pub(crate) end: usize,
    pub(crate) hash: ww_tape::ContentHash,
}

/// The definitions the records need: clock 1 and track 9 on it.
fn defs() -> Definitions {
    let mut defs = Definitions::new();
    defs.define_all(&[clock(1), track(9, 1)])
        .expect("the definitions hold");
    defs
}

/// Write `n` record blocks from tick `from` on `w`.
fn append<S: TapeSink>(w: &mut TapeWriter<S>, defs: &Definitions, from: u64, n: u64) {
    for k in from..from + n {
        w.write_block(defs, &records(w.next_id(), k))
            .expect("a records block writes");
    }
}

/// Close `w`; the bytes and the close made.
fn close(w: TapeWriter<MemSink>) -> (Vec<u8>, Made) {
    let tape = w.sink().tape();
    w.close().expect("a piece closes");
    let bytes = tape.to_vec().expect("the bytes");
    let r = TapeReader::open(&bytes[..]).expect("a closed piece opens");
    let p = &r.pieces()[0];
    let (c, _) = p.close.as_ref().expect("a close");
    let made = Made {
        close: p.close_offset.expect("its offset"),
        end: bytes.len(),
        hash: c.hash,
    };
    (bytes, made)
}

/// Resume `bytes` written with the options of `aligned`.
fn reopen(
    bytes: &[u8],
    aligned: bool,
) -> Result<(TapeWriter<MemSink>, ww_tape::Resumed), ww_tape::WriteError<ww_tape::MemError>> {
    let mut sink = MemSink::new();
    sink.write_all(bytes).expect("memory takes the bytes");
    TapeWriter::resume_on(sink, options(aligned))
}

/// The piece the input says, closed after each append: its bytes and its closes, oldest first.
pub(crate) fn build(aligned: bool, appends: &[u8]) -> (Vec<u8>, Vec<Made>) {
    let defs = defs();
    let o = options(aligned);
    let mut w = TapeWriter::create_on(MemSink::new(), o.sync);
    w.set_layout(o.layout).expect("a new piece takes a layout");
    w.set_producer(&o.producer);
    w.set_index_policy(o.index)
        .expect("a policy with room for a piece head");
    w.set_near_policy(o.near);
    let owner = OwnerId([2; 32]);
    let head = PieceHead {
        ident: PieceIdent {
            id: PieceId::first(&owner),
            owner,
            seq: 0,
        },
        prev: None,
        prev_session: SessionTag::NONE,
        integrity: ww_tape::Integrity::Full,
    };
    w.begin_piece(&head, &[]).expect("a head writes");
    append(&mut w, &defs, 0, 3);
    let (mut bytes, first) = close(w);
    let mut made = vec![first];
    let mut tick = 3;
    for a in appends {
        let (mut w, _) = reopen(&bytes, aligned).expect("a closed piece reopens");
        let n = u64::from(a & 7);
        append(&mut w, &defs, tick, n);
        tick += n;
        let (b, m) = close(w);
        bytes = b;
        made.push(m);
    }
    (bytes, made)
}

/// Where the record that `bytes` cut at `cut` stops inside of starts: the end of the last whole block before `len`.
fn torn_record(bytes: &[u8], cut: usize) -> usize {
    let mut rd = ww_tape::FileBlocks::with_recovery(bytes, ww_tape::Recovery::Prefix);
    let mut whole = 0;
    while let Some(Ok(b)) = rd.next_block() {
        let len = b.len();
        // the reader's offset is where the block it gave starts
        let end = usize::try_from(rd.offset()).expect("an offset within the bytes") + len;
        if end > cut {
            break;
        }
        whole = end;
    }
    whole
}

/// The closes of piece 0 of `r`: each names the one before it and lies after it.
fn closes_chain<S: ww_tape::reader::ByteSource>(r: &TapeReader<S>) -> Vec<(u64, Close)> {
    let Ok(all) = r.closes(0) else {
        return vec![];
    };
    for pair in all.windows(2) {
        let [(a, _), (b, c)] = pair else {
            unreachable!("windows of two")
        };
        assert!(a < b, "closes listed oldest first");
        assert_eq!(c.prev, Some(*a), "each close names the one before");
    }
    all
}

pub fn reclose(data: &[u8]) {
    let (mode, rest) = data.split_first().map_or((0, data), |(m, r)| (*m, r));
    let aligned = mode & 1 != 0;
    let anywhere = mode & 2 != 0;
    let count = usize::from((mode >> 2) & 3) + 1;
    // an append the input leaves out writes no record block
    let appends: Vec<u8> = (0..count)
        .map(|k| rest.get(k).copied().unwrap_or(0))
        .collect();
    let rest = rest.get(count..).unwrap_or_default();
    let (bytes, made) = build(aligned, &appends);
    let (cut, flips) = match rest {
        [a, b, flips @ ..] => (usize::from(u16::from_le_bytes([*a, *b])), flips),
        _ => (usize::MAX, rest),
    };
    // the bytes up to the close before the last append stay; the last append is cut and mutated
    let kept = made[made.len() - 2..][0].end;
    let mut torn = bytes.clone();
    torn.truncate(kept + cut % (bytes.len() - kept + 1));
    let (lo, hi) = if anywhere {
        (0, torn.len())
    } else {
        (kept, torn.len())
    };
    if hi > lo {
        for [a, b, bits] in flips.as_chunks::<3>().0 {
            let at = lo + usize::from(u16::from_le_bytes([*a, *b])) % (hi - lo);
            torn[at] ^= bits;
        }
    }
    // the first byte the flips changed, past the end when none did
    let first_flip = torn
        .iter()
        .zip(&bytes)
        .position(|(a, b)| a != b)
        .unwrap_or(usize::MAX);
    let r = TapeReader::open(&torn[..]);
    match &r {
        Ok(r) => {
            let _ = r.verify(0).map(|v| v.holds);
            let listed = closes_chain(r);
            if !anywhere {
                assert_eq!(r.pieces().len(), 1, "one piece");
                // the last close is in force when its close record and tail are all there, untouched
                let last = made[made.len() - 1];
                let close_at = usize::try_from(last.close).expect("a close within the bytes");
                let whole = torn.len() == bytes.len() && torn[close_at..] == bytes[close_at..];
                let want = if whole { last } else { made[made.len() - 2] };
                let p = &r.pieces()[0];
                let (c, _) = p.close.as_ref().expect("a close in force");
                assert_eq!(c.hash, want.hash, "the newest whole close is in force");
                assert!(
                    listed.len() >= made.len() - 1,
                    "every close before the append listed"
                );
            }
        }
        Err(e) => assert!(anywhere, "a piece torn after its closes opens: {e:?}"),
    }
    let Ok((mut w, resumed)) = reopen(&torn, aligned) else {
        assert!(
            anywhere || first_flip != usize::MAX,
            "a torn append resumes"
        );
        return;
    };
    if anywhere {
        drop(resumed);
        let _ = w.close();
        return;
    }
    assert!(
        resumed.kept_bytes >= kept as u64,
        "the closed bytes are kept"
    );
    // only a torn end is cut: with one byte changed, a changed byte dropped lies in the record the cut stops inside of,
    // never in a whole one. More changed bytes can make damage read as a torn end in every way the bytes show.
    let torn_from = torn_record(&bytes, torn.len());
    let changed = torn.iter().zip(&bytes).filter(|(a, b)| a != b).count();
    if changed == 1
        && let Some(at) = (resumed.kept_bytes as usize..torn.len()).find(|&i| torn[i] != bytes[i])
    {
        assert!(
            at >= torn_from,
            "a whole record cut off: a flip at {at}, the torn record at {torn_from}"
        );
    }
    append(&mut w, &defs(), 100, 2);
    let (out, _) = close(w);
    assert_eq!(
        out[..kept],
        bytes[..kept],
        "no byte before the last append moves"
    );
    let r = TapeReader::open(&out[..]).expect("a closed piece opens");
    assert!(r.pieces()[0].is_closed(), "closed whole");
    let listed = closes_chain(&r);
    for (k, m) in made[..made.len() - 1].iter().enumerate() {
        assert_eq!(listed[k].0, m.close, "close {k} kept in place");
        assert_eq!(listed[k].1.hash, m.hash, "close {k} kept as written");
    }
    if first_flip == usize::MAX {
        assert!(
            r.verify(0).map(|v| v.holds).expect("a closed piece"),
            "the content hash verifies"
        );
    }
}
