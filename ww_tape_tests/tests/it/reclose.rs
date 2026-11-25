//! A piece closed more than once, through the public API: a crash at every byte of an append, appends one after
//! another, any sequence of close, reopen, append and crash against a model, and the scans and the checkpoint walk
//! past inner closes.

use crate::pieces::{push_hex, records};
use crate::resume::{begin, head, options};
use ww_tape::RecordTrack;
use ww_tape::index::{IndexEntry, IndexKind};
use ww_tape::piece::{APPEND_HEAD_LEN, Close, PIECE_ALIGN, TAIL_LEN};
use ww_tape::reader::Found;
use ww_tape::{
    ALIGNMENT, Block, BlockKind, ByteSource, CloseReason, ContentHash, FileBlocks, Medium, MemSink,
    OpenOptions, Recovery, ResumeError, Resumed, TapeReader, TapeSink, TapeWriter, WriteError,
};

/// The ticks of the record blocks of data in `bytes`, in file order, each once.
pub(crate) fn ticks(bytes: &[u8]) -> Vec<i64> {
    let mut rd = FileBlocks::with_recovery(bytes, Recovery::Prefix);
    let mut out = vec![];
    while let Some(Ok(b)) = rd.next_block() {
        if b.kind == BlockKind::Records
            && !b.is_repeat()
            && b.first_track() != Some(RecordTrack::Definitions)
        {
            out.push(b.records().next().unwrap().unwrap().at.unwrap().ticks);
        }
    }
    out
}

/// Give `w` a records block with each tick of `ticks`.
pub(crate) fn write<S: TapeSink>(w: &mut TapeWriter<S>, ticks: impl IntoIterator<Item = i64>) {
    for n in ticks {
        let block = records(w.next_id(), n);
        w.write_block(crate::definitions::base_defs(), &block)
            .unwrap();
    }
}

/// Close `w`; the bytes of its tape.
pub(crate) fn close(w: TapeWriter<MemSink>) -> Vec<u8> {
    let tape = w.sink().tape();
    w.close().unwrap();
    tape.to_vec().unwrap()
}

/// Resume the piece `bytes` written with `options(alignment)`.
pub(crate) fn reopen(bytes: &[u8], alignment: u32) -> (TapeWriter<MemSink>, Resumed) {
    let mut sink = MemSink::new();
    sink.write_all(bytes).unwrap();
    TapeWriter::resume_on(sink, options(alignment)).unwrap()
}

/// The close records of the one piece of `bytes`, oldest first.
pub(crate) fn closes(bytes: &[u8]) -> Vec<(u64, Close)> {
    let r = TapeReader::open(bytes).unwrap();
    assert_eq!(r.pieces().len(), 1, "one piece");
    r.closes(0).unwrap()
}

/// The content hash the last close of `bytes` states.
fn close_hash(bytes: &[u8]) -> ContentHash {
    closes(bytes).last().unwrap().1.hash
}

/// An append cut at any byte, from its append head to the new tail: the reader takes the old close, the bytes after
/// it an unclosed tail and no damage, and the prefix up to the old close still verifies over its stated hash; whole,
/// the new close. A resume of the cut bytes, given the blocks it lost, ends with the bytes of the append written
/// without the crash; one of the whole append, given nothing, writes nothing.
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
    for cut in old.len()..=new.len() {
        let torn = &new[..cut];
        torn_append_reads_the_old_close(torn, &old, &new);
        let (mut w, resumed) = reopen(torn, alignment);
        assert!(resumed.kept_bytes >= old.len() as u64, "cut at {cut}");
        let have = ticks(&w.sink().tape().to_vec().unwrap());
        write(&mut w, (6..12).filter(|n| !have.contains(n)));
        let got = close(w);
        // whole, nothing is left to write: the close writes no byte
        assert!(
            got == new,
            "alignment {alignment}, cut at {cut} of {}",
            new.len()
        );
    }
}

/// `new` cut to `torn`: the reader takes the close in force of `old` (of `new` when whole), the bytes after it an
/// unclosed tail and no damage, and the closed prefix verifies.
fn torn_append_reads_the_old_close(torn: &[u8], old: &[u8], new: &[u8]) {
    let cut = torn.len();
    let r = TapeReader::open(torn).unwrap();
    assert_eq!(r.pieces().len(), 1, "cut at {cut}: one piece");
    let p = &r.pieces()[0];
    let whole = cut == new.len();
    let (in_force, _) = p.close.as_ref().expect("a close is in force");
    assert_eq!(in_force.hash, close_hash(if whole { new } else { old }));
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
    assert!(ticks(torn).starts_with(&ticks(old)), "cut at {cut}");
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

impl Model {
    /// A close left `bytes`: a new close, or none when nothing was appended since the last one and no byte changed.
    fn closed(&mut self, bytes: &[u8], seed: u64) {
        match self.closed.last() {
            Some(last) if last[..] == bytes[..] => {
                assert_eq!(
                    ticks(last),
                    self.ticks,
                    "seed {seed}: a close that wrote nothing lost no tick"
                );
            }
            _ => self.closed.push(bytes.to_vec()),
        }
    }
}

/// Any sequence of appends, closes, reopens and crashes (bytes after the last close cut anywhere) ends with the piece
/// the model has: the ticks it holds in order, a close per close not torn that followed an append, each closed prefix
/// as it was, and the piece verifies.
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
                    model.closed(&bytes, seed);
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
            model.closed(&bytes, seed);
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
            scan_back_max: exact_units::Bytes::new(0),
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

/// A close after an append of no block but a close summary still follows a final index block of its own: a reader
/// walking back from a checkpoint after it takes that close, not the one before it, whose final index the chain
/// passes too. Each append that writes costs one 4 KiB step at least.
#[test]
fn a_summary_only_append_closes_after_a_final_index_of_its_own() {
    let mut w = begin(0);
    write(&mut w, 0..6);
    let mut bytes = close(w);
    for ticks in [6..6, 6..12] {
        let before = bytes.len();
        let (mut w, _) = reopen(&bytes, 0);
        let summary_only = ticks.is_empty();
        write(&mut w, ticks);
        let tape = w.sink().tape();
        if summary_only {
            w.close_with_summary(CloseReason::Closed, b"a summary")
                .unwrap();
        } else {
            w.close().unwrap();
        }
        bytes = tape.to_vec().unwrap();
        assert_eq!(
            (bytes.len() - before) % PIECE_ALIGN as usize,
            0,
            "an append costs whole 4 KiB steps"
        );
        if summary_only {
            let (at, c) = closes(&bytes).pop().unwrap();
            let (summary_at, summary_len) = c.summary.unwrap();
            assert_eq!(summary_at, (before + APPEND_HEAD_LEN) as u64);
            assert_eq!(
                c.final_index.unwrap(),
                summary_at + u64::from(summary_len),
                "an empty final index follows the summary"
            );
            assert!(c.final_index.unwrap() < at);
            assert!(bytes.len() as u64 - TAIL_LEN as u64 > at);
        }
    }
    let all = closes(&bytes);
    assert_eq!(all.len(), 3);
    assert_ne!(
        all[1].1.final_index, all[0].1.final_index,
        "a final index of its own"
    );
    for (at, c) in &all {
        let Some(named) = c.checkpoint else { continue };
        let b = Block::read(&bytes[named as usize..]).unwrap();
        let kind = ww_tape::index::IndexBlock::read(&b).unwrap().kind();
        assert_eq!(
            kind,
            IndexKind::Checkpoint,
            "the close at {at} names a checkpoint"
        );
    }
    assert_eq!(
        all[1].1.checkpoint, all[0].1.checkpoint,
        "an append of no block places no checkpoint: its close names the newest one before"
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
    let (mut w, _) = reopen(&old, 0);
    write(&mut w, 6..7);
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
            assert_eq!(offset, old.len() as u64);
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
            assert!(offset > old.len() as u64, "{offset}");
        }
        other => panic!("{:?}", other.map(|(_, r)| r)),
    }
    assert_eq!(tape.to_vec().unwrap(), torn, "left as it was");
}

/// A sink that counts the bytes read back from it.
struct Counting {
    inner: MemSink,
    read: std::cell::Cell<u64>,
}

impl TapeSink for Counting {
    type Error = <MemSink as TapeSink>::Error;
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), Self::Error> {
        self.inner.write_all(bytes)
    }
    fn sync(&mut self) -> Result<(), Self::Error> {
        self.inner.sync()
    }
    fn close(&mut self) -> Result<(), Self::Error> {
        self.inner.close()
    }
    fn size(&self) -> Result<u64, Self::Error> {
        self.inner.size()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), Self::Error> {
        self.read.set(self.read.get() + buf.len() as u64);
        self.inner.read_at(offset, buf)
    }
    fn truncate(&mut self, len: u64) -> Result<(), Self::Error> {
        self.inner.truncate(len)
    }
    fn rebase(&mut self, base: u64) -> Result<(), Self::Error> {
        self.inner.rebase(base)
    }
}

/// The bytes a resume of the closed piece `bytes` reads back.
fn resume_reads(bytes: &[u8]) -> u64 {
    let mut inner = MemSink::new();
    inner.write_all(bytes).unwrap();
    let sink = Counting {
        inner,
        read: std::cell::Cell::new(0),
    };
    let (w, _) = TapeWriter::resume_on(sink, options(0)).unwrap();
    w.sink().read.get()
}

/// A resume of a closed piece reads its tail, its close record, the piece head and the puts after it, the index
/// chain, and each track's last chunk: never the blocks of data. Without chunks the index is not read; with them it
/// is, and one chunk per track.
#[test]
fn a_resume_of_a_closed_piece_reads_no_block_of_data() {
    use crate::pieces::{chunk, column};
    let defs = crate::definitions::base_defs();
    for n in [100, 1000, 4000] {
        let mut w = begin(0);
        write(&mut w, 0..n);
        let bytes = close(w);
        let read = resume_reads(&bytes);
        // the tail, the close record twice (to find it, then hashed on), the head and the puts after it
        assert!(
            read < 16384,
            "{n} record blocks: {read} read of {}",
            bytes.len()
        );
    }
    for n in [10, 100, 400] {
        let mut w = begin(0);
        for k in 0..n {
            for t in 0..2 {
                let (h, c) = chunk(t, k);
                w.write_chunk(defs, &h, &column(&c)).unwrap();
            }
        }
        let bytes = close(w);
        let read = resume_reads(&bytes);
        let index = closes(&bytes)[0].0 - closes(&bytes)[0].1.final_index.unwrap();
        assert!(
            read < index + 16384,
            "{n} chunks: {read} read of {}",
            bytes.len()
        );
    }
}

/// Damage before the close in force (an older tail, close record, append head or final index, a block of data)
/// never stops an append: a resume reads none of them. It writes on after the last close, every byte before as it
/// was, and the piece closes again; a verify then reports the damage.
#[test]
fn damage_before_the_close_in_force_never_stops_an_append() {
    let mut w = begin(0);
    write(&mut w, 0..3);
    let mut bytes = close(w);
    for round in 1..3 {
        let (mut w, _) = reopen(&bytes, 0);
        write(&mut w, round * 3..round * 3 + 3);
        bytes = close(w);
    }
    let all = closes(&bytes);
    let mut rd = FileBlocks::with_recovery(&bytes[..], Recovery::Prefix);
    let mut kinds = vec![];
    while let Some(Ok(b)) = rd.next_block() {
        let kind = b.kind;
        kinds.push((rd.offset() as usize, kind));
    }
    let first = |kind: BlockKind| kinds.iter().find(|(_, k)| *k == kind).unwrap().0;
    let last_close = all.last().unwrap().0 as usize;
    for (name, at) in [
        ("an older tail", first(BlockKind::Tail) + 40),
        ("an older close record", first(BlockKind::Close) + 40),
        ("an append head", first(BlockKind::AppendHead) + 40),
        ("an older final index", first(BlockKind::Index) + 40),
        (
            "the last final index",
            all.last().unwrap().1.final_index.unwrap() as usize + 40,
        ),
        ("a block of data", first(BlockKind::Records) + 40),
    ] {
        assert!(at < last_close, "{name}");
        let mut rotten = bytes.clone();
        rotten[at] ^= 0x10;
        let (mut w, resumed) = reopen(&rotten, 0);
        assert_eq!(resumed.kept_bytes, bytes.len() as u64, "{name}");
        write(&mut w, 100..102);
        let out = close(w);
        assert_eq!(
            out[..rotten.len()],
            rotten[..],
            "{name}: the bytes before stay"
        );
        let r = TapeReader::open(&out[..]).unwrap();
        assert!(r.pieces()[0].is_closed(), "{name}");
        assert_eq!(
            r.pieces()[0].close.as_ref().unwrap().0.prev,
            Some(last_close as u64),
            "{name}"
        );
        assert!(
            !r.verify(0).is_ok_and(|v| v.holds),
            "{name}: the damage is reported"
        );
    }
}

/// A piece of chunks whose index chain doesn't read: a resume still writes on after its close, taking each track's
/// newest values as unread, and the piece closes again.
#[test]
fn an_unread_index_chain_leaves_tracks_unread_and_appends() {
    use crate::pieces::{chunk, column};
    let defs = crate::definitions::base_defs();
    let mut w = begin(0);
    for k in 0..4 {
        for t in 0..2 {
            let (h, c) = chunk(t, k);
            w.write_chunk(defs, &h, &column(&c)).unwrap();
        }
    }
    let mut bytes = close(w);
    let final_index = closes(&bytes)[0].1.final_index.unwrap() as usize;
    bytes[final_index + 40] ^= 0x10;
    assert!(TapeReader::open(&bytes[..]).unwrap().entries(0).is_err());
    let (mut w, resumed) = reopen(&bytes, 0);
    assert_eq!(resumed.kept_bytes, bytes.len() as u64);
    write(&mut w, 100..102);
    let out = close(w);
    assert_eq!(out[..bytes.len()], bytes[..]);
    assert!(TapeReader::open(&out[..]).unwrap().pieces()[0].is_closed());
}

/// What a sink saw: a block written (by kind) or a sync.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Write(BlockKind),
    Sync,
}

/// A sink that records each write by the kind of the block it starts with, and each sync, in `ops`.
struct Recording {
    inner: MemSink,
    ops: std::rc::Rc<std::cell::RefCell<Vec<Op>>>,
}

impl TapeSink for Recording {
    type Error = <MemSink as TapeSink>::Error;
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), Self::Error> {
        if bytes.len() > 5 && bytes[..4] == *b"WWTP" {
            self.ops
                .borrow_mut()
                .push(Op::Write(BlockKind::from_u8(bytes[5])));
        }
        self.inner.write_all(bytes)
    }
    fn sync(&mut self) -> Result<(), Self::Error> {
        self.ops.borrow_mut().push(Op::Sync);
        self.inner.sync()
    }
    fn close(&mut self) -> Result<(), Self::Error> {
        self.inner.close()
    }
    fn size(&self) -> Result<u64, Self::Error> {
        self.inner.size()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), Self::Error> {
        self.inner.read_at(offset, buf)
    }
    fn truncate(&mut self, len: u64) -> Result<(), Self::Error> {
        self.inner.truncate(len)
    }
    fn rebase(&mut self, base: u64) -> Result<(), Self::Error> {
        self.inner.rebase(base)
    }
}

/// A close syncs after its final index, before the close record, and again after the tail, even with no sync
/// policy: a disk that writes pages out of order never holds a whole close and tail naming blocks it lost. The same
/// for the close after an append.
#[test]
fn a_close_syncs_before_its_close_record_and_after_its_tail() {
    let none = ww_tape::WriterOptions {
        sync: ww_tape::SyncPolicy::None,
        ..options(0)
    };
    let ends = |ops: &[Op]| {
        let close = ops
            .iter()
            .rposition(|o| *o == Op::Write(BlockKind::Close))
            .unwrap();
        assert!(!ops[..close - 2].contains(&Op::Sync), "{ops:?}");
        assert_eq!(
            ops[close - 2..],
            [
                Op::Write(BlockKind::Index),
                Op::Sync,
                Op::Write(BlockKind::Close),
                Op::Write(BlockKind::Tail),
                Op::Sync
            ],
            "{ops:?}"
        );
    };
    // the first close
    let ops = std::rc::Rc::new(std::cell::RefCell::new(vec![]));
    let sink = Recording {
        inner: MemSink::new(),
        ops: ops.clone(),
    };
    let tape = sink.inner.tape();
    let mut w = TapeWriter::create_on(sink, none.sync);
    w.set_layout(none.layout).unwrap();
    w.set_producer(&none.producer);
    w.set_index_policy(none.index).unwrap();
    w.begin_piece(&head(), &[]).unwrap();
    write(&mut w, 0..3);
    w.close().unwrap();
    ends(&ops.borrow());
    // the close after an append
    let bytes = tape.to_vec().unwrap();
    let ops = std::rc::Rc::new(std::cell::RefCell::new(vec![]));
    let mut inner = MemSink::new();
    inner.write_all(&bytes).unwrap();
    let sink = Recording {
        inner,
        ops: ops.clone(),
    };
    let (mut w, _) = TapeWriter::resume_on(sink, none).unwrap();
    write(&mut w, 3..5);
    w.close().unwrap();
    assert_eq!(ops.borrow()[0], Op::Write(BlockKind::AppendHead));
    ends(&ops.borrow());
}

/// A close's summary of the caller's own: placed right before the final index, named by the close, read back from the
/// tail, covered by the close's hash; on a first close and on the close after an append, and an older close's stays
/// where it was.
#[test]
fn a_close_names_a_summary_of_the_callers_own() {
    let mut w = begin(0);
    write(&mut w, 0..3);
    let tape = w.sink().tape();
    w.close_with_summary(ww_tape::piece::CloseReason::Closed, b"first: 3 records")
        .unwrap();
    let first = tape.to_vec().unwrap();
    let mut buf = vec![];
    let r = TapeReader::open(&first[..]).unwrap();
    assert_eq!(
        r.close_summary(0, &mut buf).unwrap(),
        Some(&b"first: 3 records"[..])
    );
    let (close_at, first_close) = closes(&first).pop().unwrap();
    let (at, len) = first_close.summary.unwrap();
    assert_eq!(
        at + len as u64,
        first_close.final_index.unwrap(),
        "right before the final index"
    );
    assert!(at + (len as u64) < close_at);
    assert!(r.verify(0).unwrap().holds);
    // the next close after an append names its own
    let (mut w, _) = reopen(&first, 0);
    write(&mut w, 3..5);
    let tape = w.sink().tape();
    let body = vec![0xa5; ww_tape::piece::CLOSE_SUMMARY_MAX];
    w.close_with_summary(ww_tape::piece::CloseReason::Closed, &body)
        .unwrap();
    let second = tape.to_vec().unwrap();
    assert_eq!(second[..first.len()], first[..]);
    let r = TapeReader::open(&second[..]).unwrap();
    assert_eq!(r.close_summary(0, &mut buf).unwrap(), Some(&body[..]));
    assert!(r.verify(0).unwrap().holds);
    let all = r.closes(0).unwrap();
    assert_eq!(
        all[0].1.summary, first_close.summary,
        "the older close's summary stays"
    );
    // a flip in the summary: refused by the reader, the hash fails
    let mut flipped = second.clone();
    let (at, _) = all[1].1.summary.unwrap();
    flipped[at as usize + 40] ^= 1;
    let r = TapeReader::open(&flipped[..]).unwrap();
    assert!(r.close_summary(0, &mut buf).is_err());
    assert!(!r.verify(0).unwrap().holds);
    // no summary: none
    let mut w = begin(0);
    write(&mut w, 0..1);
    let plain = close(w);
    let r = TapeReader::open(&plain[..]).unwrap();
    assert_eq!(r.close_summary(0, &mut buf).unwrap(), None);
}

/// A summary past the cap is refused before anything is written; a summary with a torn tail after it goes with the
/// end it belongs to, and the close given again places it again.
#[test]
fn a_summary_past_its_cap_refuses_and_a_torn_close_drops_it() {
    let mut w = begin(0);
    write(&mut w, 0..3);
    let tape = w.sink().tape();
    let before = tape.to_vec().unwrap();
    let body = vec![1; ww_tape::piece::CLOSE_SUMMARY_MAX + 1];
    match w.close_with_summary(ww_tape::piece::CloseReason::Closed, &body) {
        Err(WriteError::Input(ww_tape::InputError::SummaryTooLong { len, max })) => {
            assert_eq!((len, max), (body.len(), ww_tape::piece::CLOSE_SUMMARY_MAX));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(tape.to_vec().unwrap(), before, "nothing written");
    let mut w = begin(0);
    write(&mut w, 0..3);
    let tape = w.sink().tape();
    let open = tape.to_vec().unwrap();
    w.close_with_summary(ww_tape::piece::CloseReason::Closed, b"s")
        .unwrap();
    let closed = tape.to_vec().unwrap();
    let torn = &closed[..closed.len() - 10];
    let (w, resumed) = reopen(torn, 0);
    assert_eq!(
        resumed.kept_bytes,
        open.len() as u64,
        "summary, final index and close dropped"
    );
    let tape = w.sink().tape();
    w.close_with_summary(ww_tape::piece::CloseReason::Closed, b"s")
        .unwrap();
    assert_eq!(tape.to_vec().unwrap(), closed, "the same close again");
}

/// The golden append: a piece closed with a summary, written on after its close (append head) and closed again with a
/// summary; its blocks pin the bytes of kinds 13 and 14 (`WW_TAPE_GOLDEN=update` rewrites it).
#[test]
fn golden_append() {
    let mut w = begin(0);
    write(&mut w, 0..2);
    let tape = w.sink().tape();
    w.close_with_summary(ww_tape::piece::CloseReason::Closed, b"one")
        .unwrap();
    let (mut w, _) = reopen(&tape.to_vec().unwrap(), 0);
    write(&mut w, 2..3);
    let tape = w.sink().tape();
    w.close_with_summary(ww_tape::piece::CloseReason::Closed, b"two")
        .unwrap();
    golden(&tape.to_vec().unwrap(), "v1-append.hex");
}

/// The blocks of `bytes`, one line of hex each, against `ww_tape/golden/NAME` (`WW_TAPE_GOLDEN=update` rewrites it).
pub(crate) fn golden(bytes: &[u8], name: &str) {
    let mut hex = String::new();
    let mut rd = FileBlocks::new(bytes);
    while let Some(b) = rd.next_block() {
        push_hex(&mut hex, b.unwrap().bytes());
        hex.push('\n');
    }
    let golden = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../ww_tape/golden")
        .join(name);
    if std::env::var("WW_TAPE_GOLDEN").as_deref() == Ok("update") {
        std::fs::write(&golden, &hex).unwrap();
    }
    let want = std::fs::read_to_string(&golden)
        .unwrap_or_else(|_| panic!("golden/{name}: run with WW_TAPE_GOLDEN=update"));
    assert!(
        hex == want,
        "the format changed: bytes differ from golden/{name}"
    );
}

/// A piece closed and written on again, read while it grows: the close in force, the unclosed tail after it, the
/// growth a reader notices by its source's size, and the check of each close (`docs/growing_pieces.md`).
#[test]
fn a_reader_of_a_growing_piece_takes_the_close_in_force() {
    let mut w = begin(0);
    write(&mut w, 0..6);
    let old = close(w);
    let closed = old.len() as u64;
    let (mut w, _) = reopen(&old, 0);
    write(&mut w, 6..9);
    let tape = w.sink().tape();

    // --8<-- [start:close_in_force]
    let r = TapeReader::open(tape.clone()).unwrap();
    let piece = &r.pieces()[0];
    assert_eq!(r.closes(0).unwrap().len(), 1, "one close so far");
    assert_eq!(
        piece.closed_end(),
        Some(closed),
        "the close in force ends here"
    );
    assert!(!piece.is_closed(), "blocks follow its tail");
    assert_eq!(
        r.unclosed(0).unwrap(),
        Some(closed..r.len()),
        "the unclosed tail"
    );
    // --8<-- [end:close_in_force]

    write(&mut w, 9..12);

    // --8<-- [start:growth]
    let grew = r.source().size().unwrap() > r.len();
    assert!(grew, "the writer appended since the open");
    let r = TapeReader::open(tape.clone()).unwrap();
    // --8<-- [end:growth]
    assert_eq!(ticks(&tape.to_vec().unwrap()), (0..12).collect::<Vec<_>>());

    // --8<-- [start:verify]
    let v = r.verify(0).unwrap();
    assert!(v.holds, "the stretch up to the close in force checks");
    assert_eq!(
        v.unchecked,
        Some(closed..r.len()),
        "no close covers the tail yet"
    );
    // --8<-- [end:verify]

    let new = close(w);
    let r = TapeReader::open(&new[..]).unwrap();
    let v = r.verify(0).unwrap();
    assert!(
        v.holds && v.unchecked.is_none(),
        "closed again, it checks whole"
    );
    assert_eq!(v.stretches, 2, "one stretch per close");
}
