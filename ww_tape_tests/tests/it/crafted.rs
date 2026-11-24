//! A piece closed more than once with fields crafted and their CRCs made good again: what a reader takes on a stated
//! value alone. A tail, close record or append head that states what is not so is never taken as it states: the bytes
//! it would hide are read, or reported as bytes no close covers.

use crate::pieces::records;
use crate::resume::{begin, options};
use ww_tape::piece::{TAIL_LEN, Tail};
use ww_tape::reader::Found;
use ww_tape::{
    Block, BlockId, BlockKind, ContentHash, FileBlocks, Medium, MemSink, OpenOptions, Recovery,
    TapeReader, TapeSink, TapeWriter,
};

const CRC: crc::Crc<u32> = crc::Crc::<u32>::new(&crc::CRC_32_ISCSI);

/// A piece closed `n` times, six record blocks per stretch: the bytes after each close.
fn closed_n(n: usize) -> Vec<Vec<u8>> {
    let mut w = begin(0);
    let mut out = vec![];
    for k in 0..n as i64 {
        for t in k * 6..k * 6 + 6 {
            let block = records(w.next_id(), t);
            w.write_block(crate::definitions::base_defs(), &block)
                .unwrap();
        }
        let tape = w.sink().tape();
        w.close().unwrap();
        out.push(tape.to_vec().unwrap());
        let mut sink = MemSink::new();
        sink.write_all(out.last().unwrap()).unwrap();
        w = TapeWriter::resume_on(sink, options(0)).unwrap().0;
    }
    out
}

/// Where each block of `kind` starts in `bytes`.
fn starts(bytes: &[u8], kind: BlockKind) -> Vec<usize> {
    blocks(bytes)
        .into_iter()
        .filter(|b| b.2 == kind)
        .map(|b| b.0)
        .collect()
}

/// Each block of `bytes`: where it starts and ends, and its kind.
fn blocks(bytes: &[u8]) -> Vec<(usize, usize, BlockKind)> {
    let mut rd = FileBlocks::with_recovery(bytes, Recovery::Prefix);
    let mut out = vec![];
    while let Some(Ok(b)) = rd.next_block() {
        let (kind, len) = (b.kind, b.len());
        // the reader's offset is where the block it gave starts
        let at = rd.offset() as usize;
        out.push((at, at + len, kind));
    }
    out
}

/// Make the CRC of the plain block `block` good again.
fn reseal(block: &mut [u8]) {
    let n = block.len() - 4;
    let crc = CRC.checksum(&block[..n]);
    block[n..].copy_from_slice(&crc.to_le_bytes());
}

/// The tail at `at` in `bytes`, read.
fn tail_at(bytes: &[u8], at: usize) -> Tail {
    Tail::read(&bytes[at..at + TAIL_LEN]).unwrap()
}

/// `tail` written as a good tail block with the id of the tail block at `like` in `bytes`.
fn sealed(tail: &Tail, bytes: &[u8], like: usize) -> Vec<u8> {
    let b = Block::read(&bytes[like..like + TAIL_LEN]).unwrap();
    let mut buf = vec![0u8; TAIL_LEN];
    tail.write(&mut buf, BlockId::new(b.session, b.seq));
    buf
}

/// The hash the close in force of piece 0 of `bytes` states, and whether it ends the piece.
fn in_force(bytes: &[u8], options: OpenOptions) -> (Option<ContentHash>, bool) {
    let r = TapeReader::open_with(bytes, options).unwrap();
    let p = &r.pieces()[0];
    (p.close.as_ref().map(|c| c.0.hash), p.is_closed())
}

/// The hash each close of the piece states, oldest first.
fn hashes(prefixes: &[Vec<u8>]) -> Vec<ContentHash> {
    let r = TapeReader::open(&prefixes.last().unwrap()[..]).unwrap();
    r.closes(0).unwrap().iter().map(|(_, c)| c.hash).collect()
}

/// A good tail after the last one, naming an older close and stating the whole length: the reader takes the
/// newest close, which ends where its own tail starts, and the extra tail is bytes of a piece of their own.
#[test]
fn a_tail_after_the_last_names_no_older_close() {
    let p = closed_n(3);
    let h = hashes(&p);
    let bytes = &p[2];
    let tails = starts(bytes, BlockKind::Tail);
    let mut fake = tail_at(bytes, tails[1]);
    fake.piece_len = (bytes.len() + TAIL_LEN) as u64;
    let grown = [&bytes[..], &sealed(&fake, bytes, tails[1])].concat();
    let r = TapeReader::open(&grown[..]).unwrap();
    let p0 = &r.pieces()[0];
    assert_eq!(p0.close.as_ref().unwrap().0.hash, h[2], "the newest close");
    assert!(p0.is_closed());
    assert_eq!(
        r.pieces().len(),
        2,
        "the extra tail is reported: {:?}",
        r.pieces()
    );
    assert_eq!(r.pieces()[1].start, bytes.len() as u64);
    assert_eq!(r.closes(0).unwrap().len(), 3);
}

/// The last tail replaced by one naming an older close, its length the whole piece: the older close is in force, and
/// the bytes after it are reported as no close's, never checked as if covered.
#[test]
fn a_last_tail_naming_an_older_close_leaves_the_bytes_after_it_unchecked() {
    let p = closed_n(3);
    let h = hashes(&p);
    let mut bytes = p[2].clone();
    let tails = starts(&bytes, BlockKind::Tail);
    let mut fake = tail_at(&bytes, tails[1]);
    fake.piece_len = bytes.len() as u64;
    let sealed = sealed(&fake, &bytes, tails[2]);
    bytes[tails[2]..].copy_from_slice(&sealed);
    let r = TapeReader::open(&bytes[..]).unwrap();
    let p0 = &r.pieces()[0];
    assert_ne!(p0.found, Found::Tail);
    assert_eq!(p0.close.as_ref().unwrap().0.hash, h[1]);
    assert!(!p0.is_closed());
    let end_of_two = p[1].len() as u64;
    assert_eq!(p0.closed_end(), Some(end_of_two));
    let v = r.verify(0).unwrap();
    assert!(v.holds, "{v:?}");
    assert_eq!(v.checked_end, end_of_two);
    assert_eq!(v.unchecked, Some(end_of_two..bytes.len() as u64));
    assert_eq!(r.unclosed(0).unwrap(), v.unchecked);
}

/// The last tail stating a piece that starts at an append head: no piece starts there, and the whole piece keeps its
/// newest close.
#[test]
fn a_tail_claiming_a_shorter_piece_starts_no_piece_at_an_append_head() {
    let p = closed_n(3);
    let h = hashes(&p);
    let mut bytes = p[2].clone();
    let tails = starts(&bytes, BlockKind::Tail);
    let mut fake = tail_at(&bytes, tails[2]);
    // the close and tail are those of the last stretch, said to start after the second close's tail
    let from = p[1].len() as u64;
    fake.piece_len -= from;
    fake.close_offset -= from;
    let sealed = sealed(&fake, &bytes, tails[2]);
    bytes[tails[2]..].copy_from_slice(&sealed);
    let r = TapeReader::open(&bytes[..]).unwrap();
    assert!(
        r.pieces().iter().all(|q| q.start == 0 || q.close.is_none()),
        "no closed piece starts inside: {:?}",
        r.pieces().iter().map(|q| q.start).collect::<Vec<_>>()
    );
    assert_ne!(r.pieces()[0].close.as_ref().map(|c| c.0.hash), Some(h[2]));
}

/// The last tail stating another content hash, its CRC good: it is not the tail of the close it names. A reader takes
/// the close before, and reports what follows.
#[test]
fn a_last_tail_stating_another_hash_ends_no_close() {
    let p = closed_n(3);
    let h = hashes(&p);
    let mut bytes = p[2].clone();
    let tails = starts(&bytes, BlockKind::Tail);
    let mut fake = tail_at(&bytes, tails[2]);
    fake.hash = h[1];
    let sealed = sealed(&fake, &bytes, tails[2]);
    bytes[tails[2]..].copy_from_slice(&sealed);
    let (hash, closed) = in_force(&bytes, OpenOptions::default());
    assert_eq!(hash, Some(h[1]));
    assert!(!closed);
    for (name, o) in [
        (
            "raw",
            OpenOptions {
                medium: Medium::Raw,
                ..OpenOptions::default()
            },
        ),
        (
            "scan",
            OpenOptions {
                scan_back_max: exact_units::Bytes::new(0),
                ..OpenOptions::default()
            },
        ),
    ] {
        assert_eq!(in_force(&bytes, o), (Some(h[1]), false), "{name}");
    }
}

/// Cut after a checkpoint of the third stretch, the walk back from it passes the second close's final index: it takes
/// the close after that final index only when the close names it. A second close naming the first final index (its CRC
/// good) is not taken: the first close is in force.
#[test]
fn the_checkpoint_walk_takes_only_a_close_naming_the_final_index_before_it() {
    let p = closed_n(3);
    let h = hashes(&p);
    let mut bytes = p[2].clone();
    let finals = TapeReader::open(&bytes[..])
        .unwrap()
        .closes(0)
        .unwrap()
        .iter()
        .map(|(_, c)| c.final_index.unwrap())
        .collect::<Vec<_>>();
    let checkpoint = blocks(&bytes)
        .into_iter()
        .find(|(at, _, kind)| {
            *kind == BlockKind::Index && *at > p[1].len() && *at as u64 != finals[2]
        })
        .expect("a checkpoint in the third stretch");
    let torn = &bytes[..checkpoint.1];
    assert_eq!(
        in_force(torn, OpenOptions::default()).0,
        Some(h[1]),
        "as written"
    );
    let at = starts(&bytes, BlockKind::Close)[1];
    let len = starts(&bytes, BlockKind::Tail)[1] - at;
    bytes[at + 80..at + 88].copy_from_slice(&finals[0].to_le_bytes());
    reseal(&mut bytes[at..at + len]);
    let torn = &bytes[..checkpoint.1];
    let r = TapeReader::open(torn).unwrap();
    assert!(
        matches!(r.pieces()[0].found, Found::Checkpoint { .. }),
        "{:?}",
        r.pieces()[0].found
    );
    assert_eq!(
        r.pieces()[0].close.as_ref().map(|c| c.0.hash),
        Some(h[0]),
        "close 2 doesn't name the final index before it"
    );
}

/// A scan goes on past a tail only at an append head naming that close, that tail and that hash: one naming another
/// hash starts no stretch of the piece.
#[test]
fn a_scan_goes_on_only_after_an_append_head_naming_the_close() {
    let p = closed_n(2);
    let h = hashes(&p);
    let mut bytes = p[1].clone();
    let heads = starts(&bytes, BlockKind::AppendHead);
    let at = heads[0];
    // the append head's hash field, at 48, names another close
    bytes[at + 48..at + 80].copy_from_slice(&h[1].0);
    reseal(&mut bytes[at..at + 84]);
    let raw = OpenOptions {
        medium: Medium::Raw,
        ..OpenOptions::default()
    };
    let r = TapeReader::open_with(&bytes[..], raw).unwrap();
    assert_eq!(r.pieces()[0].close.as_ref().unwrap().0.hash, h[0]);
    assert_eq!(
        r.pieces()[0].end,
        p[0].len() as u64,
        "the piece ends at its first tail"
    );
    // read by its tail the piece is whole, but verify says the append head doesn't go on from its close
    let r = TapeReader::open(&bytes[..]).unwrap();
    assert_eq!(r.pieces()[0].close.as_ref().unwrap().0.hash, h[1]);
    let v = r.verify(0).unwrap();
    assert!(!v.holds, "the append head names another hash");
}

/// Verify says what it checked: a closed piece up to its last tail, with nothing unchecked; an unclosed tail after the
/// close in force is named, and its blocks are not among the closed entries.
#[test]
fn verify_says_what_it_did_not_check() {
    let p = closed_n(2);
    let r = TapeReader::open(&p[1][..]).unwrap();
    let v = r.verify(0).unwrap();
    assert!(v.holds);
    assert_eq!(
        (v.stretches, v.checked_end, v.unchecked),
        (2, p[1].len() as u64, None)
    );
    assert_eq!(
        r.closed_entries(0).unwrap(),
        r.entries(0)
            .unwrap()
            .into_iter()
            .filter(|e| e.offset < v.close.0)
            .collect::<Vec<_>>()
    );
    // an append torn in its close: the blocks of the append are entries, not closed entries
    let torn = &p[1][..p[1].len() - 200];
    let r = TapeReader::open(torn).unwrap();
    let v = r.verify(0).unwrap();
    assert!(v.holds);
    assert_eq!(v.stretches, 1);
    assert_eq!(v.unchecked, Some(p[0].len() as u64..torn.len() as u64));
    let closed = r.closed_entries(0).unwrap();
    assert!(closed.iter().all(|e| e.offset < p[0].len() as u64));
    assert!(
        r.entries(0).unwrap().len() > closed.len(),
        "the append's blocks are listed apart"
    );
}

/// An append head that names another tail, in a piece whose second close states the hash of those very bytes: every
/// stated hash checks, and verify still finds the append head not going on from the close before it.
#[test]
fn verify_checks_each_append_head_against_the_close_before_it() {
    let p = closed_n(2);
    let h = hashes(&p);
    let mut bytes = p[1].clone();
    let closes_at = starts(&bytes, BlockKind::Close);
    let tails = starts(&bytes, BlockKind::Tail);
    let head = starts(&bytes, BlockKind::AppendHead)[0];
    // the append head names the close record as its tail too
    bytes[head + 40..head + 48].copy_from_slice(&(closes_at[0] as u64).to_le_bytes());
    reseal(&mut bytes[head..head + 84]);
    // the second close and its tail state the hash of the bytes as they are now
    let mut hasher = blake3::Hasher::new();
    hasher.update(&h[0].0);
    hasher.update(&bytes[closes_at[0]..closes_at[1]]);
    let forged = ContentHash(*hasher.finalize().as_bytes());
    bytes[closes_at[1] + 40..closes_at[1] + 72].copy_from_slice(&forged.0);
    reseal(&mut bytes[closes_at[1]..tails[1]]);
    let mut tail = tail_at(&bytes, tails[1]);
    tail.hash = forged;
    let sealed = sealed(&tail, &bytes, tails[1]);
    bytes[tails[1]..].copy_from_slice(&sealed);
    let r = TapeReader::open(&bytes[..]).unwrap();
    assert_eq!(r.pieces()[0].found, Found::Tail);
    assert_eq!(r.pieces()[0].close.as_ref().unwrap().0.hash, forged);
    let v = r.verify(0).unwrap();
    assert_eq!(v.stretches, 2);
    assert!(
        !v.holds,
        "the append head doesn't follow the first close's tail"
    );
}

/// The last tail crafted to state the hash of an older close, or another piece length, its CRC made good: it ends no
/// close, so a resume takes it as damage, not a torn append: it refuses, naming the tail, and every byte stays.
#[test]
fn a_resume_refuses_a_crafted_last_tail() {
    let p = closed_n(2);
    let bytes = &p[1];
    let tails = starts(bytes, BlockKind::Tail);
    let older = tail_at(bytes, tails[0]).hash;
    let crafts: [&dyn Fn(&mut Tail); 3] = [&|t| t.hash = older, &|t| t.piece_len += 1, &|t| {
        t.close_len -= 1;
    }];
    for (k, craft) in crafts.iter().enumerate() {
        let mut tail = tail_at(bytes, tails[1]);
        craft(&mut tail);
        let mut crafted = bytes.clone();
        crafted[tails[1]..].copy_from_slice(&sealed(&tail, bytes, tails[1]));
        let mut sink = MemSink::new();
        sink.write_all(&crafted).unwrap();
        let tape = sink.tape();
        match TapeWriter::resume_on(sink, options(0)) {
            Err(ww_tape::WriteError::Resume(ww_tape::ResumeError::Damaged { offset })) => {
                assert_eq!(offset, tails[1] as u64, "craft {k}");
            }
            other => panic!("craft {k}: {:?}", other.map(|(_, r)| r)),
        }
        assert_eq!(tape.to_vec().unwrap(), crafted, "craft {k}: the bytes stay");
    }
}

/// The first close and its tail crafted to name a next piece, as a cut's do, their CRCs made good, an append after
/// them: a resume reads only the close in force and its tail, never the closes before it, so it writes on after the
/// last close, every byte before as it was. The crafted close stays for a verify to report.
#[test]
fn a_resume_reads_no_close_before_the_one_in_force() {
    let p = closed_n(2);
    let mut bytes = p[1].clone();
    let closes = starts(&bytes, BlockKind::Close);
    let tails = starts(&bytes, BlockKind::Tail);
    let ends: Vec<_> = blocks(&bytes).iter().map(|b| (b.0, b.1)).collect();
    let close_end = ends.iter().find(|b| b.0 == closes[0]).unwrap().1;
    bytes[closes[0] + 112..closes[0] + 128].fill(0x5c);
    reseal(&mut bytes[closes[0]..close_end]);
    bytes[tails[0] + 104..tails[0] + 120].fill(0x5c);
    reseal(&mut bytes[tails[0]..tails[0] + TAIL_LEN]);
    let r = TapeReader::open(&bytes[..]).unwrap();
    assert!(
        r.closes(0).unwrap()[0].1.next.is_some(),
        "the first close names a next piece"
    );
    let mut sink = MemSink::new();
    sink.write_all(&bytes).unwrap();
    let tape = sink.tape();
    let (w, resumed) = TapeWriter::resume_on(sink, options(0)).unwrap();
    assert_eq!(resumed.kept_bytes, bytes.len() as u64);
    assert_eq!(resumed.after_close.unwrap().hash, hashes(&p)[1]);
    w.close().unwrap();
    let out = tape.to_vec().unwrap();
    assert_eq!(out[..bytes.len()], bytes[..], "the bytes before stay");
    let r = TapeReader::open(&out[..]).unwrap();
    assert!(r.pieces()[0].is_closed());
    assert!(
        !r.verify(0).unwrap().holds,
        "the crafted close fails the hash chain"
    );
}
