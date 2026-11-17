//! Resume after a crash, memory sinks and PieceChain on any sink, through the public API.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::definitions::{base_defs, clock, state, track, tracks};
use crate::id;
use crate::pieces::{chunk, column, records, scratch};
use ww_tape::index::IndexPolicy;
use ww_tape::piece::{PieceId, piece_name};
use ww_tape::writer::NearPolicy;
use ww_tape::{
    ALIGNMENT, BlockKind, ChainOptions, CutPolicy, Definitions, Describe, FileBlocks, Files,
    Layout, MemSink, MemTapes, OwnerId, PieceChain, PieceHead, PieceIdent, Recovery, SessionTag,
    SinkFactory, Slot, StatePolicy, SyncPolicy, TapeReader, TapeSink, TapeWriter, WriteError,
    WriterOptions,
};

/// One call a test makes: on a writer (each but a put places one counted block) or on the definitions.
#[derive(Clone, Copy, Debug)]
enum Op {
    /// Hold definition `i` of those added mid-piece.
    Add(usize),
    /// A records block with tick `n`.
    Records(i64),
    /// A records block with tick `n`, deferred.
    Defer(i64),
    /// Track `t`'s chunk `k`.
    Chunk(u64, u64),
    /// Put item `k` on state track `t`.
    Put(u32, u64),
}

/// The calls of a recording: `rounds` rounds of two chunks, a records block and a deferred one, definitions added
/// after half of them, and puts on the two state tracks they hold from then on: with the writer's memory of three
/// slots, puts of items the memory forgot.
fn ops(rounds: u64) -> Vec<Op> {
    let mut out = vec![];
    for k in 0..rounds {
        if k == rounds / 2 {
            out.extend((0..added().len()).map(Op::Add));
        }
        if k >= rounds / 2 {
            out.extend([Op::Put(11, k % 3), Op::Put(12, k % 2)]);
        }
        out.extend([Op::Chunk(0, k), Op::Records(k as i64)]);
        if k >= rounds / 2 {
            out.push(Op::Put(11, (k + 1) % 3));
        }
        out.extend([Op::Chunk(1, k), Op::Defer(1000 + k as i64)]);
    }
    out
}

/// Track 0's policy in the tests: a definition is put again 3 KiB on, with two copies.
const DEFINITIONS: StatePolicy = StatePolicy {
    near: exact_units::Bytes::new(3 << 10),
    copies: exact_units::Count::new(2),
};

/// The definitions added mid-piece: clock 2, track 10 on it, state tracks 11 and 12.
fn added() -> Vec<Describe<'static>> {
    vec![clock(2), track(10, 2), state(11), state(12)]
}

/// Make the call `op` on `w` with the definitions `defs`.
fn apply<S: TapeSink>(w: &mut TapeWriter<S>, defs: &mut Definitions, op: Op) {
    match op {
        Op::Add(i) => defs.define(&added()[i]).unwrap(),
        Op::Records(n) => w.write_block(defs, &records(w.next_id(), n)).unwrap(),
        Op::Defer(n) => w.defer_block(defs, records(id(0), n)).unwrap(),
        Op::Chunk(t, k) => {
            let (h, c) = chunk(t, k);
            w.write_chunk(defs, &h, &column(&c)).unwrap();
        }
        Op::Put(t, k) => {
            w.put(defs, t, k as i64, &[k as u8; 40]).unwrap();
        }
    }
}

/// The options of the tests: checkpoints every 1 KiB, copies 2 KiB apart, a memory of three slots, so a piece holds
/// every kind of block a writer adds and the writer forgets slots on the way.
fn options(alignment: u32) -> WriterOptions {
    WriterOptions {
        sync: SyncPolicy::None,
        index: IndexPolicy {
            bytes: 1024,
            interval: None,
            ..IndexPolicy::default()
        },
        layout: if alignment == 0 {
            Layout::UNALIGNED
        } else {
            Layout::ALIGNED
        }
        .with_definitions(DEFINITIONS),
        producer: "resume".to_owned(),
        near: NearPolicy {
            copy_spacing: 2 << 10,
            memory_max: 3,
        },
    }
}

fn head() -> PieceHead {
    let owner = OwnerId([3; 32]);
    PieceHead {
        ident: PieceIdent {
            id: PieceId::first(&owner),
            owner,
            seq: 0,
        },
        prev: None,
        prev_session: SessionTag::NONE,
    }
}

/// A first piece of `ops` on a memory sink with `options`, begun but given no call yet.
fn begin(alignment: u32) -> TapeWriter<MemSink> {
    let mut w = TapeWriter::create_on(MemSink::new(Path::new("p")), SyncPolicy::None);
    let o = options(alignment);
    w.set_layout(o.layout).unwrap();
    w.set_producer(&o.producer);
    w.set_index_policy(o.index)
        .expect("a policy with room for a piece head");
    w.set_near_policy(o.near);
    w.begin_piece(&head(), &[]).unwrap();
    w
}

/// The seq of each record block by its tick in `bytes`, the first records of each.
fn record_seqs(bytes: &[u8]) -> BTreeMap<i64, u64> {
    let mut rd = FileBlocks::with_recovery(bytes, Recovery::Prefix);
    let mut out = BTreeMap::new();
    while let Some(Ok(b)) = rd.next_block() {
        if b.kind == BlockKind::Records && !b.is_repeat() && b.first_track() != Some(0) {
            let at = b.records().next().unwrap().unwrap().at.unwrap();
            out.insert(at.ticks, b.seq);
        }
    }
    out
}

/// Where each call of a run was made: the seq of the block it placed (a put: of the last block before it), the
/// bytes of its piece before and after it (a put places deferred blocks before its own) and its piece.
#[derive(Clone, Debug)]
struct Made {
    seqs: Vec<u64>,
    before: Vec<u64>,
    after: Vec<u64>,
    pieces: Vec<u64>,
}

/// Whether call `op`, made where `made` says at `i`, is to be given again to a writer resumed in piece `piece` with
/// next seq `next` that kept `kept` bytes: a block it doesn't hold, a put made after the last block it kept (in this
/// piece, or in a later one). Definitions are the application's: always made again.
fn lost(op: &Op, made: &Made, i: usize, piece: u64, next: u64, kept: u64) -> bool {
    match op {
        Op::Add(_) => true,
        Op::Put(..) => {
            let (before, after) = (made.before[i], made.after[i]);
            // a put whose last block is kept isn't given again: its own records changed what the writer remembers
            made.pieces[i] > piece
                || (made.pieces[i] == piece && (after > kept || (after == kept && before == after)))
        }
        _ => made.seqs[i] >= next,
    }
}

/// The piece of `ops` written without a crash, and where each call was made.
fn reference(alignment: u32, ops: &[Op]) -> (Vec<u8>, Made) {
    let mut w = begin(alignment);
    let mut defs = base_defs().clone();
    let mut made = Made {
        seqs: vec![],
        before: vec![],
        after: vec![],
        pieces: vec![],
    };
    for op in ops {
        made.before.push(w.bytes());
        apply(&mut w, &mut defs, *op);
        made.after.push(w.bytes());
        made.seqs.push(w.next_seq().wrapping_sub(1));
        made.pieces.push(0);
    }
    let tape = w.sink().tape();
    w.finish().unwrap();
    let bytes = tape.to_vec().unwrap();
    // a deferred block takes its seq where it is placed
    let placed = record_seqs(&bytes);
    for (op, seq) in ops.iter().zip(&mut made.seqs) {
        if let Op::Defer(n) = op {
            *seq = placed[n];
        }
    }
    (bytes, made)
}

/// The ends of the blocks of `bytes` and some places in between, ascending: where the tests cut a piece.
fn cut_points(bytes: &[u8], extra: usize) -> Vec<usize> {
    let mut rd = FileBlocks::with_recovery(bytes, Recovery::Prefix);
    let mut out = vec![0];
    while let Some(Ok(_)) = rd.next_block() {
        out.push(rd.offset() as usize);
    }
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    for _ in 0..extra {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.push((x % bytes.len() as u64) as usize);
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Give a writer resumed after a crash, keeping `kept` bytes, the calls it lost, then finish it; its bytes.
fn finish_resumed(mut w: TapeWriter<MemSink>, ops: &[Op], made: &Made, kept: u64) -> Vec<u8> {
    let next = w.next_seq();
    let mut defs = base_defs().clone();
    for (i, op) in ops.iter().enumerate() {
        if lost(op, made, i, 0, next, kept) {
            apply(&mut w, &mut defs, *op);
        }
    }
    let tape = w.sink().tape();
    w.finish().unwrap();
    tape.to_vec().unwrap()
}

/// The blocks of `bytes` around `at`, one line each (offset, kind, flags, seq, the slots of its records): what a
/// failed comparison shows.
fn blocks_around(bytes: &[u8], at: u64) -> String {
    let mut rd = FileBlocks::with_recovery(bytes, Recovery::Prefix);
    let mut out = vec![];
    while let Some(Ok(b)) = rd.next_block() {
        let slots: Vec<Option<Slot>> = if b.kind == BlockKind::Records {
            b.records()
                .flatten()
                .map(|r| Slot::of(r.track, r.item))
                .collect()
        } else {
            vec![]
        };
        let line = format!("{:?} flags {} seq {} {slots:?}", b.kind, b.flags, b.seq);
        let offset = rd.offset();
        if offset + 600 > at && offset < at + 1500 {
            out.push(format!("{offset} {line}"));
        }
    }
    out.join("\n")
}

/// A piece cut at every block boundary and at bytes in between resumes, takes the calls it lost and ends with the
/// bytes, and so the content hash, of the piece written without the crash: with deferred blocks, checkpoints,
/// definitions put again and copied, state items put, a memory small enough to forget, definitions made mid-piece,
/// and cuts inside the end of the piece.
fn cut_anywhere(alignment: u32) {
    let ops = ops(8);
    let (want, made) = reference(alignment, &ops);
    let r = TapeReader::open(&want[..]).unwrap();
    let entries = r.entries(0).unwrap();
    let defs = r.track_entries(0, ww_tape::index::STATE_TRACK_ID).unwrap();
    let puts = crate::definitions::puts(&want).len();
    assert!(puts >= 8, "puts and copies: {puts}");
    assert!(defs.iter().all(|e| e.repeat), "definitions are put");
    assert!(entries.len() >= 40, "{} blocks", entries.len());
    let all = r.definitions_at(0, u64::MAX).unwrap();
    assert_eq!(
        tracks(&all),
        [1, 2, 9, 11, 12],
        "the tracks with data or items, those added mid-piece among them"
    );
    let forgotten = (0..3u64).all(|k| {
        let hash = ww_tape::ItemHash::of(&[k as u8; 40]);
        r.state_entries(0)
            .unwrap()
            .iter()
            .filter(|(_, s)| s.slot == Slot::State { track: 11 } && s.hash == hash)
            .count()
            >= 2
    });
    assert!(forgotten, "items of track 11 written again");
    let mut ended = 0;
    for cut in cut_points(&want, 40) {
        let mut sink = MemSink::new(Path::new("p"));
        sink.write_all(&want[..cut]).unwrap();
        let (mut w, resumed) = TapeWriter::resume_on(sink, options(alignment)).unwrap();
        assert!(resumed.kept_bytes <= cut as u64);
        assert_eq!(resumed.kept_bytes + resumed.dropped_bytes, cut as u64);
        assert!(
            resumed
                .entries
                .iter()
                .all(|e| e.offset < resumed.kept_bytes)
        );
        ended += usize::from(resumed.ended.is_some());
        if w.bytes() == 0 {
            // cut inside the head: the piece starts again
            w.begin_piece(&head(), &[]).unwrap();
        }
        let got = finish_resumed(w, &ops, &made, resumed.kept_bytes);
        assert!(
            got == want,
            "alignment {alignment}, cut at {cut} of {}, kept {}\nwant\n{}\ngot\n{}",
            want.len(),
            resumed.kept_bytes,
            blocks_around(&want, cut as u64),
            blocks_around(&got, cut as u64)
        );
    }
    assert!(ended > 0, "cuts after the finish");
}

#[test]
fn a_piece_cut_anywhere_resumes_to_the_same_bytes() {
    cut_anywhere(0);
}

#[test]
fn an_aligned_piece_cut_anywhere_resumes_to_the_same_bytes() {
    cut_anywhere(ALIGNMENT);
}

/// A resume whose options contradict the layout the piece describes is refused before the piece is touched: an
/// aligned piece with unaligned options and the reverse. The error names both alignments.
#[test]
fn a_resume_with_another_alignment_is_refused() {
    let ops = ops(8);
    for (written, given) in [(ALIGNMENT, 0), (0, ALIGNMENT)] {
        let (want, _) = reference(written, &ops);
        let cut = want.len() / 2;
        let mut sink = MemSink::new(Path::new("p"));
        sink.write_all(&want[..cut]).unwrap();
        let tape = sink.tape();
        let Err(e) = TapeWriter::resume_on(sink, options(given)) else {
            panic!("alignment {given} on a piece written with {written} is refused");
        };
        match &e {
            WriteError::Layout {
                described,
                given: g,
            } => {
                assert_eq!(described.alignment.get(), written);
                assert_eq!(g.alignment.get(), given);
            }
            other => panic!("{other:?}"),
        }
        let text = e.to_string();
        assert!(
            text.contains(&written.to_string()) && text.contains(&given.to_string()),
            "{text}"
        );
        assert_eq!(
            tape.to_vec().unwrap(),
            want[..cut],
            "the piece is untouched"
        );

        // the options it was written with resume it, and say what was checked
        let mut sink = MemSink::new(Path::new("p"));
        sink.write_all(&want[..cut]).unwrap();
        let (_, resumed) = TapeWriter::resume_on(sink, options(written)).unwrap();
        assert_eq!(resumed.layout.map(|l| l.alignment.get()), Some(written));
    }
}

/// A piece cut off inside the block that holds its layout (the first after the head) has no layout to check against:
/// it resumes with any options, and says nothing was checked.
#[test]
fn a_piece_without_its_layout_resumes_unchecked() {
    let ops = ops(2);
    let (want, _) = reference(0, &ops);
    let r = TapeReader::open(&want[..]).unwrap();
    let first = (r.track_entries(0, ww_tape::index::STATE_TRACK_ID).unwrap())
        .into_iter()
        .next()
        .unwrap();
    // inside the block of the layout and the producer
    let cut = (first.offset + 10) as usize;
    let mut sink = MemSink::new(Path::new("p"));
    sink.write_all(&want[..cut]).unwrap();
    let (_, resumed) = TapeWriter::resume_on(sink, options(ALIGNMENT)).unwrap();
    assert_eq!(resumed.layout, None);
}

/// [TapeWriter::resume] reopens `NAME.partial`, keeps its good blocks and reports them.
#[test]
fn resume_reopens_the_partial_file() {
    let dir = scratch("resume_file");
    let path = dir.join("p.wwt");
    let ops = ops(6);
    let (want, made) = reference(0, &ops);
    let cut = want.len() * 2 / 3;
    std::fs::write(ww_tape::writer::partial_name(&path), &want[..cut]).unwrap();
    let (mut w, resumed) = TapeWriter::resume(&path, options(0)).unwrap();
    assert!(resumed.kept_bytes <= cut as u64);
    assert!(
        resumed
            .entries
            .iter()
            .all(|e| e.offset < resumed.kept_bytes)
    );
    assert!(resumed.entries.iter().any(|e| e.kind == BlockKind::Events));
    let next = w.next_seq();
    let mut defs = base_defs().clone();
    for (i, op) in ops.iter().enumerate() {
        if lost(op, &made, i, 0, next, resumed.kept_bytes) {
            apply(&mut w, &mut defs, *op);
        }
    }
    let f = w.finish().unwrap();
    assert_eq!(std::fs::read(&f.path).unwrap(), want);
    // finished: nothing to resume
    assert!(matches!(
        TapeWriter::resume(&path, options(0)),
        Err(ww_tape::WriteError::Exists(_))
    ));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A resumed piece whose newest block of track 1's definition is damaged takes that slot as unknown (the index lists
/// it): the definition is put again before the next data, so every block of data after the resume has its
/// definitions close behind and readable; it ends whole, its content hash over the damaged bytes (writing side
/// review 2, point 6).
#[test]
fn a_damaged_block_of_definitions_is_put_again_after_a_resume() {
    let ops = ops(12);
    let (want, made) = reference(0, &ops);
    let r = TapeReader::open(&want[..]).unwrap();
    let cut = want.len() / 2;
    let newest = r
        .state_entries(0)
        .unwrap()
        .into_iter()
        .rfind(|(e, s)| s.slot == Slot::Track { number: 1 } && (e.offset as usize) < cut - 200)
        .unwrap()
        .0;
    let flipped = newest.offset as usize + 40;
    let mut sink = MemSink::new(Path::new("p"));
    let mut damaged = want[..cut].to_vec();
    damaged[flipped] ^= 1;
    sink.write_all(&damaged).unwrap();
    let (w, resumed) = TapeWriter::resume_on(sink, options(0)).unwrap();
    let kept = resumed.kept_bytes;
    let got = finish_resumed(w, &ops, &made, kept);
    let r = TapeReader::open(&got[..]).unwrap();
    assert!(r.verify(0).unwrap());
    let data: Vec<u64> = r
        .entries(0)
        .unwrap()
        .iter()
        .filter(|e| {
            e.kind == BlockKind::Events || (e.kind == BlockKind::Records && e.track_id == 0)
        })
        .map(|e| e.offset)
        .filter(|at| *at >= kept)
        .collect();
    assert!(!data.is_empty());
    for at in data {
        for slot in [Slot::Track { number: 1 }, Slot::Clock { number: 1 }] {
            let found = r.state_at(0, slot, at).unwrap();
            assert!(found.item.is_some(), "at {at}: {slot:?}");
        }
    }
}

/// A chain written to memory reads through [TapeReader] while it is written and once finished, and its pieces are the
/// bytes of the same chain written to files.
#[test]
fn a_chain_in_memory_reads_and_matches_files() {
    let dir = scratch("mem_chain");
    let opts = ChainOptions {
        writer: options(0),
        cut: CutPolicy {
            chunks: 5,
            ..Default::default()
        },
    };
    let owner = OwnerId([5; 32]);
    let mem = MemTapes::new();
    let mut in_mem =
        PieceChain::create_on(mem.clone(), Path::new("mem"), owner, 1, &[], opts.clone()).unwrap();
    let mut on_disk = PieceChain::create(&dir, owner, 1, &[], opts).unwrap();
    let (mut a, mut b) = (base_defs().clone(), base_defs().clone());
    for op in ops(10) {
        apply_chain(&mut in_mem, &mut a, op);
        apply_chain(&mut on_disk, &mut b, op);
    }
    // the open piece reads while it is written
    let live = mem
        .get(&Path::new("mem").join(piece_name(in_mem.seq())))
        .unwrap()
        .unwrap();
    let r = TapeReader::open(live.clone()).unwrap();
    assert!(!r.pieces().is_empty());
    assert!(
        r.entries(0)
            .unwrap()
            .iter()
            .any(|e| e.kind == BlockKind::Events)
    );
    let a = in_mem.finish().unwrap();
    let b = on_disk.finish().unwrap();
    assert!(a.len() >= 3, "{} pieces", a.len());
    assert_eq!(a.len(), b.len());
    let mut all = vec![];
    for (fa, fb) in a.iter().zip(&b) {
        let tape = mem.get(&fa.path).unwrap().unwrap();
        assert!(tape.is_finished());
        let bytes = tape.to_vec().unwrap();
        assert_eq!(bytes, std::fs::read(&fb.path).unwrap());
        assert_eq!(fa.hash, fb.hash);
        let r = TapeReader::open(tape).unwrap();
        assert!(r.verify(0).unwrap());
        all.extend(bytes);
    }
    let r = TapeReader::open(&all[..]).unwrap();
    assert!(r.chain().is_whole(), "{:?}", r.chain());
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The calls of a chain: each chunk through [PieceChain::write_chunk], so cuts come before chunks.
fn apply_chain<F: SinkFactory>(c: &mut PieceChain<F>, defs: &mut Definitions, op: Op) {
    match op {
        Op::Chunk(t, k) => {
            let (h, b) = chunk(t, k);
            c.write_chunk(defs, &h, &column(&b)).unwrap();
        }
        _ => apply(c.writer().unwrap(), defs, op),
    }
}

fn chain_options() -> ChainOptions {
    ChainOptions {
        writer: options(0),
        cut: CutPolicy {
            chunks: 6,
            ..Default::default()
        },
    }
}

/// The pieces of a chain of `ops` written to `dir` without a crash, and where each call was made.
fn reference_chain(dir: &Path, ops: &[Op]) -> (Vec<PathBuf>, Made) {
    let mut c = PieceChain::create(dir, OwnerId([9; 32]), 0, &[], chain_options()).unwrap();
    let mut defs = base_defs().clone();
    let mut made = Made {
        seqs: vec![],
        before: vec![],
        after: vec![],
        pieces: vec![],
    };
    for op in ops {
        // a chunk may cut first: the bytes before it are in the piece it lands in
        if matches!(op, Op::Chunk(..)) && c.cut_due() {
            c.cut_now().unwrap();
        }
        made.before.push(c.writer().unwrap().bytes());
        apply_chain(&mut c, &mut defs, *op);
        made.after.push(c.writer().unwrap().bytes());
        made.seqs
            .push(c.writer().unwrap().next_seq().wrapping_sub(1));
        made.pieces.push(c.seq());
    }
    let paths: Vec<PathBuf> = c.finish().unwrap().into_iter().map(|f| f.path).collect();
    let mut placed = BTreeMap::new();
    for p in &paths {
        placed.extend(record_seqs(&std::fs::read(p).unwrap()));
    }
    for (op, seq) in ops.iter().zip(&mut made.seqs) {
        if let Op::Defer(n) = op {
            *seq = placed[n];
        }
    }
    (paths, made)
}

/// Give a chain resumed keeping `kept` bytes of its last piece the calls it lost.
fn replay_chain<F: SinkFactory>(c: &mut PieceChain<F>, ops: &[Op], made: &Made, kept: u64) {
    let next = c.writer().unwrap().next_seq();
    let piece = c.seq();
    let mut defs = base_defs().clone();
    for (i, op) in ops.iter().enumerate() {
        if lost(op, made, i, piece, next, kept) {
            apply_chain(c, &mut defs, *op);
        }
    }
}

/// A chain cut off anywhere in any piece, or between a cut's finish and the next piece, resumes and ends with the
/// pieces of the chain written without the crash; the pieces finished before are read back.
#[test]
fn a_chain_cut_anywhere_resumes_to_the_same_pieces() {
    let dir = scratch("chain_resume");
    let want_dir = dir.join("want");
    std::fs::create_dir(&want_dir).unwrap();
    let ops = ops(14);
    let (paths, made) = reference_chain(&want_dir, &ops);
    assert!(paths.len() >= 4, "{} pieces", paths.len());
    let want: Vec<Vec<u8>> = paths.iter().map(|p| std::fs::read(p).unwrap()).collect();
    let mut runs = 0;
    for (i, piece) in want.iter().enumerate() {
        // a crash between the cut's finish and the next piece: no file of piece i yet
        let mut cuts: Vec<Option<usize>> = vec![None];
        cuts.extend(cut_points(piece, 20).into_iter().map(Some));
        for cut in cuts {
            if cut.is_none() && i == 0 {
                continue;
            }
            let d = dir.join(format!("run{runs}"));
            runs += 1;
            std::fs::create_dir(&d).unwrap();
            for (j, w) in want.iter().enumerate().take(i) {
                std::fs::write(d.join(piece_name(j as u64)), w).unwrap();
            }
            if let Some(cut) = cut {
                let partial = ww_tape::writer::partial_name(&d.join(piece_name(i as u64)));
                std::fs::write(partial, &piece[..cut]).unwrap();
            }
            let (mut c, resumed) = PieceChain::resume(&d, OwnerId([9; 32]), 0, chain_options())
                .unwrap_or_else(|e| panic!("piece {i}, cut {cut:?}: {e}"));
            assert_eq!(c.finished().len(), i);
            replay_chain(&mut c, &ops, &made, resumed.kept_bytes);
            let got = c.finish().unwrap();
            assert_eq!(got.len(), want.len(), "piece {i}, cut {cut:?}");
            for (j, w) in want.iter().enumerate() {
                let g = std::fs::read(d.join(piece_name(j as u64))).unwrap();
                let first = g.iter().zip(w).position(|(a, b)| a != b);
                assert!(
                    &g == w,
                    "piece {i}, cut {cut:?}: piece {j} differs at {first:?}, {} vs {} bytes",
                    g.len(),
                    w.len()
                );
            }
            std::fs::remove_dir_all(&d).unwrap();
        }
    }
    // a chain whose last piece is complete takes no more
    assert!(matches!(
        PieceChain::resume(&want_dir, OwnerId([9; 32]), 0, chain_options()),
        Err(ww_tape::WriteError::Ended(ww_tape::FinishState::Complete))
    ));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A chain in memory left unfinished resumes on the same [MemTapes] and ends as one written in one go.
#[test]
fn a_chain_in_memory_resumes() {
    let tmp = scratch("mem_resume");
    let ops = ops(10);
    let (paths, made) = reference_chain(&tmp, &ops);
    let mem = MemTapes::new();
    let dir = Path::new("a");
    let mut c =
        PieceChain::create_on(mem.clone(), dir, OwnerId([9; 32]), 0, &[], chain_options()).unwrap();
    let mut defs = base_defs().clone();
    for op in &ops[..ops.len() / 2] {
        apply_chain(&mut c, &mut defs, *op);
    }
    drop(c);
    let (mut c, resumed) =
        PieceChain::resume_on(mem.clone(), dir, OwnerId([9; 32]), 0, chain_options()).unwrap();
    assert!(!resumed.entries.is_empty());
    assert!(!c.finished().is_empty());
    replay_chain(&mut c, &ops, &made, resumed.kept_bytes);
    let got = c.finish().unwrap();
    assert_eq!(got.len(), paths.len());
    for (f, p) in got.iter().zip(&paths) {
        assert_eq!(
            mem.get(&f.path).unwrap().unwrap().to_vec().unwrap(),
            std::fs::read(p).unwrap()
        );
    }
    // no piece of a chain in a place: nothing to resume
    assert!(Files.finished(&tmp.join("none")).unwrap().is_none());
    assert!(PieceChain::resume(&tmp.join("none"), OwnerId([9; 32]), 0, chain_options()).is_err());
    std::fs::remove_dir_all(&tmp).unwrap();
}
