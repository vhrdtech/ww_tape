//! A piece closed more than once, through the public API: a crash at every byte of an append, appends one after
//! another, any sequence of close, reopen, append and crash against a model, and the scans and the checkpoint walk
//! past inner closes.

use crate::pieces::records;
use crate::resume::{begin, head, options};
use ww_tape::index::IndexEntry;
use ww_tape::piece::{APPEND_HEAD_LEN, Close};
use ww_tape::reader::Found;
use ww_tape::{
    ALIGNMENT, BlockKind, ContentHash, FileBlocks, Medium, MemSink, OpenOptions, Recovery,
    ResumeError, Resumed, TapeReader, TapeSink, TapeWriter, WriteError,
};

/// The ticks of the record blocks of data in `bytes`, in file order, each once.
fn ticks(bytes: &[u8]) -> Vec<i64> {
    let mut rd = FileBlocks::with_recovery(bytes, Recovery::Prefix);
    let mut out = vec![];
    while let Some(Ok(b)) = rd.next_block() {
        if b.kind == BlockKind::Records && !b.is_repeat() && b.first_track() != Some(0) {
            out.push(b.records().next().unwrap().unwrap().at.unwrap().ticks);
        }
    }
    out
}

/// Give `w` a records block with each tick of `ticks`.
fn write<S: TapeSink>(w: &mut TapeWriter<S>, ticks: impl IntoIterator<Item = i64>) {
    for n in ticks {
        let block = records(w.next_id(), n);
        w.write_block(crate::definitions::base_defs(), &block)
            .unwrap();
    }
}

/// Close `w`; the bytes of its tape.
fn close(w: TapeWriter<MemSink>) -> Vec<u8> {
    let tape = w.sink().tape();
    w.close().unwrap();
    tape.to_vec().unwrap()
}

/// Resume the piece `bytes` written with `options(alignment)`.
fn reopen(bytes: &[u8], alignment: u32) -> (TapeWriter<MemSink>, Resumed) {
    let mut sink = MemSink::new();
    sink.write_all(bytes).unwrap();
    TapeWriter::resume_on(sink, options(alignment)).unwrap()
}

/// The close records of the one piece of `bytes`, oldest first.
fn closes(bytes: &[u8]) -> Vec<(u64, Close)> {
    let r = TapeReader::open(bytes).unwrap();
    assert_eq!(r.pieces().len(), 1, "one piece");
    r.closes(0).unwrap()
}

/// The content hash the last close of `bytes` states.
fn close_hash(bytes: &[u8]) -> ContentHash {
    closes(bytes).last().unwrap().1.hash
}

/// The ends of the blocks of `bytes` past `from`, and bytes in between: where an append is cut.
fn cuts_after(bytes: &[u8], from: usize) -> Vec<usize> {
    let mut rd = FileBlocks::with_recovery(bytes, Recovery::Prefix);
    let mut out = vec![from, from + 1, from + APPEND_HEAD_LEN - 1, bytes.len()];
    while let Some(Ok(_)) = rd.next_block() {
        let end = rd.offset() as usize;
        if end > from {
            out.extend([end - 1, end, end + 1]);
        }
    }
    let mut x = 0x2545_f491_4f6c_dd1du64;
    for _ in 0..60 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.push(from + (x % (bytes.len() - from) as u64) as usize);
    }
    out.retain(|c| (from..=bytes.len()).contains(c));
    out.sort_unstable();
    out.dedup();
    out
}

/// An append cut at any byte, from its append head to the new tail: the reader takes the old close, the bytes after
/// it an unclosed tail and no damage, and the prefix up to the old close still verifies over its stated hash; whole,
/// the new close. A resume of the cut bytes, given the blocks it lost, ends with the bytes of the append written
/// without the crash; one of the whole append writes on after its close again.
fn append_cut_anywhere(alignment: u32) {
    let mut w = begin(alignment);
    write(&mut w, 0..6);
    let old = close(w);
    let (mut w, resumed) = reopen(&old, alignment);
    assert_eq!(resumed.kept_bytes, old.len() as u64);
    assert!(resumed.after_close.is_some(), "written on after the close");
    write(&mut w, 6..12);
    let new = close(w);
    assert_eq!(
        new[..old.len()],
        old[..],
        "no byte before the old tail moves"
    );
    assert_eq!(new[old.len() + 5], BlockKind::AppendHead.to_u8());
    let both = closes(&new);
    assert_eq!(both.len(), 2);
    assert_eq!(both[0].1.hash, close_hash(&old), "the old close stays");
    assert_eq!(both[1].1.prev, Some(both[0].0), "the new close names it");
    assert!(
        TapeReader::open(&new[..])
            .unwrap()
            .verify(0)
            .map(|v| v.holds)
            .unwrap()
    );
    assert_eq!(ticks(&new), (0..12).collect::<Vec<_>>());
    let cuts = cuts_after(&new, old.len());
    assert!(cuts.len() > 30, "{} cuts", cuts.len());
    for cut in cuts {
        let torn = &new[..cut];
        let r = TapeReader::open(torn).unwrap();
        assert_eq!(r.pieces().len(), 1, "cut at {cut}: one piece");
        let p = &r.pieces()[0];
        let whole = cut == new.len();
        let (in_force, _) = p.close.as_ref().expect("a close is in force");
        assert_eq!(in_force.hash, close_hash(if whole { &new } else { &old }));
        assert!(
            p.head_error.is_none() && p.close_error.is_none(),
            "cut at {cut}"
        );
        assert!(
            p.gaps.is_empty() && p.replaced.is_empty(),
            "cut at {cut}: no damage"
        );
        assert_eq!(
            p.closed_end(),
            Some(if whole { new.len() } else { old.len() } as u64)
        );
        assert_eq!(
            p.is_closed(),
            whole || p.end == old.len() as u64,
            "cut at {cut}"
        );
        if !whole && cut > old.len() {
            assert!(
                p.end > old.len() as u64 || p.cut == Some(old.len() as u64),
                "cut at {cut}: the bytes after the old tail are an unclosed tail"
            );
        }
        assert!(
            r.verify(0).map(|v| v.holds).unwrap(),
            "cut at {cut}: the closed prefix verifies"
        );
        assert!(ticks(torn).starts_with(&ticks(&old)), "cut at {cut}");
        let (mut w, resumed) = reopen(torn, alignment);
        assert!(resumed.kept_bytes >= old.len() as u64, "cut at {cut}");
        let have = ticks(&w.sink().tape().to_vec().unwrap());
        write(&mut w, (6..12).filter(|n| !have.contains(n)));
        let got = close(w);
        if whole {
            assert_eq!(got[..new.len()], new[..], "written on after the new close");
            assert_eq!(closes(&got).len(), 3);
        } else {
            assert!(
                got == new,
                "alignment {alignment}, cut at {cut} of {}",
                new.len()
            );
        }
    }
}

#[test]
fn an_append_cut_anywhere_reads_the_old_close_or_the_new_one() {
    append_cut_anywhere(0);
}

#[test]
fn an_aligned_append_cut_anywhere_reads_the_old_close_or_the_new_one() {
    append_cut_anywhere(ALIGNMENT);
}

/// A piece closed four times: each close names the one before, states the hash of its prefix as closed then, and every
/// prefix stays as it was; the piece verifies stretch by stretch and lists every block.
#[test]
fn a_piece_closed_four_times_reads_every_close() {
    let mut w = begin(0);
    write(&mut w, 0..3);
    let mut prefixes = vec![close(w)];
    for round in 1..4 {
        let (mut w, _) = reopen(prefixes.last().unwrap(), 0);
        write(&mut w, round * 3..round * 3 + 3);
        prefixes.push(close(w));
    }
    let last = prefixes.last().unwrap();
    let all = closes(last);
    assert_eq!(all.len(), 4);
    for (k, prefix) in prefixes.iter().enumerate() {
        assert_eq!(last[..prefix.len()], prefix[..], "prefix {k} as it was");
        assert_eq!(all[k].1.hash, close_hash(prefix), "close {k}");
        assert_eq!(
            all[k].1.prev,
            k.checked_sub(1).map(|j| all[j].0),
            "close {k}"
        );
    }
    let r = TapeReader::open(&last[..]).unwrap();
    assert!(r.verify(0).map(|v| v.holds).unwrap());
    let listed = record_offsets(&r.entries(0).unwrap());
    assert!(listed.len() >= 12, "{} record blocks listed", listed.len());
    assert!(
        listed.iter().any(|at| *at > all[2].0),
        "blocks after the third close"
    );
    assert_eq!(ticks(last), (0..12).collect::<Vec<_>>());
}

/// A piece closed once is read as before: one close naming none before it, no append head.
#[test]
fn a_piece_closed_once_has_one_close_and_no_append_head() {
    let mut w = begin(0);
    write(&mut w, 0..4);
    let bytes = close(w);
    let all = closes(&bytes);
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].1.prev, None);
    let mut rd = FileBlocks::with_recovery(&bytes[..], Recovery::Prefix);
    while let Some(Ok(b)) = rd.next_block() {
        assert_ne!(b.kind, BlockKind::AppendHead);
    }
}

/// A small xorshift generator: the model test's choices, the same each run.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

/// The piece as a naive model has it: the ticks written that a crash didn't take, in order, and the bytes up to each
/// close.
#[derive(Default)]
struct Model {
    ticks: Vec<i64>,
    closed: Vec<Vec<u8>>,
}

/// Any sequence of appends, closes, reopens and crashes (bytes after the last close cut anywhere) ends with the piece
/// the model has: the ticks it holds in order, a close per close not torn, each closed prefix as it was, and the
/// piece verifies.
#[test]
fn close_reopen_append_and_crash_match_a_model() {
    // crashes after a close, and closes, over all runs: the sequences reach what the test is about
    let (mut torn_appends, mut closes_made) = (0, 0);
    for seed in 1..=150u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        let mut model = Model::default();
        let mut tick = 0i64;
        let mut w = Some(begin(0));
        let mut bytes = vec![];
        for _ in 0..12 {
            match (w.take(), rng.below(4)) {
                (Some(mut open), 0 | 1) => {
                    let n = rng.below(5) as i64;
                    write(&mut open, tick..tick + n);
                    model.ticks.extend(tick..tick + n);
                    tick += n;
                    w = Some(open);
                }
                (Some(open), 2) => {
                    bytes = close(open);
                    model.closed.push(bytes.clone());
                }
                (Some(open), _) => {
                    // a crash: the bytes after the last close torn anywhere, then a resume
                    let now = open.sink().tape().to_vec().unwrap();
                    let from = model.closed.last().map_or(0, Vec::len);
                    torn_appends += usize::from(from > 0);
                    let cut = from + rng.below((now.len() - from + 1) as u64) as usize;
                    drop(open);
                    if cut == 0 {
                        w = Some(begin(0));
                        model.ticks.clear();
                        continue;
                    }
                    let (mut resumed, _) = reopen(&now[..cut], 0);
                    if resumed.bytes() == 0 {
                        // cut inside the head: the piece starts again
                        resumed.begin_piece(&head(), &[]).unwrap();
                    }
                    let kept = ticks(&resumed.sink().tape().to_vec().unwrap());
                    assert!(
                        model.ticks.starts_with(&kept),
                        "seed {seed}: no tick made up"
                    );
                    let closed = model.closed.last().map_or(&[][..], |c| &c[..]);
                    assert!(
                        kept.starts_with(&ticks(closed)),
                        "seed {seed}: no closed tick lost"
                    );
                    model.ticks = kept;
                    w = Some(resumed);
                }
                (None, _) => {
                    let (open, resumed) = reopen(&bytes, 0);
                    assert!(resumed.after_close.is_some(), "seed {seed}");
                    w = Some(open);
                }
            }
        }
        if let Some(open) = w {
            bytes = close(open);
            model.closed.push(bytes.clone());
        }
        assert_eq!(ticks(&bytes), model.ticks, "seed {seed}");
        let all = closes(&bytes);
        assert_eq!(all.len(), model.closed.len(), "seed {seed}");
        for (k, prefix) in model.closed.iter().enumerate() {
            assert_eq!(bytes[..prefix.len()], prefix[..], "seed {seed}, close {k}");
            assert_eq!(all[k].1.hash, close_hash(prefix), "seed {seed}, close {k}");
        }
        let r = TapeReader::open(&bytes[..]).unwrap();
        assert!(r.pieces()[0].is_closed(), "seed {seed}");
        assert!(r.verify(0).map(|v| v.holds).unwrap(), "seed {seed}");
        closes_made += all.len();
    }
    assert!(torn_appends >= 50, "{torn_appends} crashes after a close");
    assert!(closes_made >= 300, "{closes_made} closes");
}

/// The offsets of the record blocks an index lists, each once.
fn record_offsets(entries: &[IndexEntry]) -> Vec<u64> {
    let mut out: Vec<u64> = entries
        .iter()
        .filter(|e| e.kind == BlockKind::Records)
        .map(|e| e.offset)
        .collect();
    out.dedup();
    out
}

/// A piece closed three times, with checkpoints after each reopen, read by its tail, by its newest checkpoint (the
/// last tail torn), by a scan of the file and by a walk of a raw medium: each lists the same blocks, across the
/// inner closes.
#[test]
fn scans_and_checkpoints_pass_inner_closes() {
    let mut w = begin(0);
    write(&mut w, 0..20);
    let mut bytes = close(w);
    for round in 1..3 {
        let (mut w, _) = reopen(&bytes, 0);
        write(&mut w, round * 20..round * 20 + 20);
        bytes = close(w);
    }
    assert_eq!(closes(&bytes).len(), 3);
    let by_tail = TapeReader::open(&bytes[..]).unwrap();
    assert_eq!(by_tail.pieces()[0].found, Found::Tail);
    let want = record_offsets(&by_tail.entries(0).unwrap());
    assert!(
        want.len() >= 60,
        "every record block listed: {}",
        want.len()
    );
    let raw = TapeReader::open_with(
        &bytes[..],
        OpenOptions {
            medium: Medium::Raw,
            ..OpenOptions::default()
        },
    )
    .unwrap();
    assert_eq!(raw.pieces().len(), 1, "one piece on a raw medium");
    assert!(raw.pieces()[0].is_closed());
    assert_eq!(record_offsets(&raw.entries(0).unwrap()), want, "raw walk");
    // the last tail torn: the piece is open after its second close
    let torn = &bytes[..bytes.len() - 10];
    let by_checkpoint = TapeReader::open(torn).unwrap();
    assert!(
        matches!(by_checkpoint.pieces()[0].found, Found::Checkpoint { .. }),
        "{:?}",
        by_checkpoint.pieces()[0].found
    );
    assert_eq!(
        record_offsets(&by_checkpoint.entries(0).unwrap()),
        want,
        "checkpoint walk"
    );
    let scanned = TapeReader::open_with(
        torn,
        OpenOptions {
            scan_back_max: 0,
            ..OpenOptions::default()
        },
    )
    .unwrap();
    assert!(
        matches!(scanned.pieces()[0].found, Found::Scan { .. }),
        "{:?}",
        scanned.pieces()[0].found
    );
    assert_eq!(record_offsets(&scanned.entries(0).unwrap()), want, "scan");
    for r in [&by_checkpoint, &scanned] {
        let p = &r.pieces()[0];
        assert_eq!(p.close.as_ref().unwrap().0.hash, closes(&bytes)[1].1.hash);
        assert!(!p.is_closed());
        assert!(r.verify(0).map(|v| v.holds).unwrap());
    }
}

/// An append of no block still closes after a final index block of its own: a reader walking back from a checkpoint
/// after it takes that close, not the one before it, whose final index the chain passes too.
#[test]
fn an_empty_append_closes_after_a_final_index_of_its_own() {
    let mut w = begin(0);
    write(&mut w, 0..6);
    let mut bytes = close(w);
    for ticks in [6..6, 6..12] {
        let (mut w, _) = reopen(&bytes, 0);
        write(&mut w, ticks);
        bytes = close(w);
    }
    let all = closes(&bytes);
    assert_eq!(all.len(), 3);
    assert_ne!(
        all[1].1.final_index, all[0].1.final_index,
        "a final index of its own"
    );
    let torn = &bytes[..bytes.len() - 10];
    let r = TapeReader::open(torn).unwrap();
    let p = &r.pieces()[0];
    assert!(matches!(p.found, Found::Checkpoint { .. }), "{:?}", p.found);
    assert_eq!(
        p.close.as_ref().unwrap().0.hash,
        all[1].1.hash,
        "the newest whole close"
    );
}

/// An append head damaged with a good block after it: the close before it stays in force for a reader, and a resume
/// refuses to cut the good block, every byte left as it was. Junk alone after the tail is damage too; blank bytes or
/// fewer than a header are a torn append, cut off.
#[test]
fn a_damaged_append_head_keeps_the_close_before_it() {
    let mut w = begin(0);
    write(&mut w, 0..6);
    let old = close(w);
    let (w, _) = reopen(&old, 0);
    let whole = close(w);
    // the magic broken, or the kind turned into a close record's
    for (at, bits) in [
        (0, 0x30),
        (5, BlockKind::AppendHead.to_u8() ^ BlockKind::Close.to_u8()),
    ] {
        let mut torn = whole[..whole.len() - 10].to_vec();
        torn[old.len() + at] ^= bits;
        refused_after(&old, &torn);
    }
    let junk = [&old[..], &[0x5a; 40][..]].concat();
    let mut sink = MemSink::new();
    sink.write_all(&junk).unwrap();
    let tape = sink.tape();
    match TapeWriter::resume_on(sink, options(0)) {
        Err(ww_tape::WriteError::Resume(ww_tape::ResumeError::Damaged { offset })) => {
            assert_eq!(offset, old.len() as u64)
        }
        other => panic!("{:?}", other.map(|(_, r)| r)),
    }
    assert_eq!(tape.to_vec().unwrap(), junk, "left as it was");
    for torn in [vec![0; 40], vec![0x5a; 31]] {
        let (w, resumed) = reopen(&[&old[..], &torn[..]].concat(), 0);
        assert_eq!(resumed.kept_bytes, old.len() as u64);
        assert_eq!(close(w)[..old.len()], old[..]);
    }
}

/// A flipped bit in the last close record or its tail: the close before it is in force for a reader. A resume takes
/// the flip as damage, never as a torn append: it refuses and every byte stays, the close record a reader took before
/// the flip among them.
#[test]
fn a_flip_in_the_last_close_or_tail_refuses_a_resume() {
    let mut w = begin(0);
    write(&mut w, 0..6);
    let old = close(w);
    let (mut w, _) = reopen(&old, 0);
    write(&mut w, 6..9);
    let whole = close(w);
    let close_at = closes(&whole).last().unwrap().0 as usize;
    let tail_at = whole.len() - ww_tape::piece::TAIL_LEN;
    for (name, at, want) in [
        (
            "close record",
            close_at + 40,
            ResumeError::GoodBlockAfterCut {
                offset: tail_at as u64,
            },
        ),
        (
            "tail hash",
            tail_at + 70,
            ResumeError::Damaged {
                offset: tail_at as u64,
            },
        ),
        (
            "tail CRC",
            whole.len() - 1,
            ResumeError::Damaged {
                offset: tail_at as u64,
            },
        ),
        (
            "tail length",
            tail_at + 8,
            ResumeError::Damaged {
                offset: tail_at as u64,
            },
        ),
    ] {
        let mut flipped = whole.clone();
        flipped[at] ^= 0x10;
        let r = TapeReader::open(&flipped[..]).unwrap();
        assert_eq!(
            r.pieces()[0].close.as_ref().unwrap().0.hash,
            close_hash(&old),
            "{name}"
        );
        let mut sink = MemSink::new();
        sink.write_all(&flipped).unwrap();
        let tape = sink.tape();
        match TapeWriter::resume_on(sink, options(0)) {
            Err(WriteError::Resume(e)) => assert_eq!(e, want, "{name}"),
            other => panic!("{name}: {:?}", other.map(|(_, r)| r)),
        }
        assert_eq!(tape.to_vec().unwrap(), flipped, "{name}: every byte stays");
    }
}

/// `torn`, the piece `old` and a damaged append after it: read with the close of `old` in force, refused by a resume
/// and left as it was.
fn refused_after(old: &[u8], torn: &[u8]) {
    let r = TapeReader::open(torn).unwrap();
    assert_eq!(r.pieces().len(), 1);
    assert_eq!(
        r.pieces()[0].close.as_ref().unwrap().0.hash,
        close_hash(old)
    );
    let mut sink = MemSink::new();
    sink.write_all(torn).unwrap();
    let tape = sink.tape();
    match TapeWriter::resume_on(sink, options(0)) {
        Err(ww_tape::WriteError::Resume(ww_tape::ResumeError::GoodBlockAfterCut { offset })) => {
            assert!(offset > old.len() as u64, "{offset}")
        }
        other => panic!("{:?}", other.map(|(_, r)| r)),
    }
    assert_eq!(tape.to_vec().unwrap(), torn, "left as it was");
}
