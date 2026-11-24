//! Pieces of a chain removed on purpose while it is written ([PieceChain::remove]), and the resume of a chain whose
//! first or middle pieces are gone: from its lowest piece, over holes its closes name.

use std::path::Path;

use crate::pieces::{chunk, column, scratch};
use crate::reclose::close;
use ww_tape::piece::{ChainEnd, ChainLink, ChainReport};
use ww_tape::sink::TapeSink;
use ww_tape::{
    ChainOptions, CutPolicy, Definitions, InputError, MemError, MemPiece, MemPieces, MemSink,
    OwnerId, PieceChain, PieceStore, PieceTapes, RemovedRefusal, ResumeError, SyncPolicy,
    TapeReader, TapeWriter, WriteError, WriterOptions,
};

const OWNER: OwnerId = OwnerId([0x52; 32]);

fn options(tapes: PieceTapes) -> ChainOptions {
    ChainOptions {
        tapes,
        writer: WriterOptions {
            sync: SyncPolicy::None,
            ..Default::default()
        },
        cut: CutPolicy {
            chunks: exact_units::Count::new(2),
            ..Default::default()
        },
    }
}

fn defs() -> Definitions {
    crate::definitions::base_defs().clone()
}

/// Chunks `from..to` of track 0, two to a piece.
fn write<T: PieceStore>(c: &mut PieceChain<T>, defs: &Definitions, from: u64, to: u64) {
    for k in from..to {
        let (h, b) = chunk(0, k);
        c.write_chunk(defs, &h, &column(&b)).unwrap();
    }
}

/// The pieces of `mem`, by key, back to back.
pub(crate) fn bytes_of(mem: &MemPieces) -> Vec<u8> {
    mem.keys()
        .unwrap()
        .iter()
        .flat_map(|k| mem.get(k).unwrap().unwrap().to_vec().unwrap())
        .collect()
}

pub(crate) fn report(bytes: &[u8]) -> ChainReport {
    TapeReader::open(bytes).unwrap().chain()
}

/// Every link holds, by hash or over named removed pieces, and the chain ends at a close.
pub(crate) fn linked(c: &ChainReport) {
    assert!(
        c.links
            .iter()
            .all(|l| matches!(l, ChainLink::Linked | ChainLink::Removed { .. })),
        "{:?}",
        c.links
    );
    assert!(c.strays.is_empty(), "{:?}", c.strays);
    assert!(matches!(c.end, ChainEnd::Closed { .. }), "{:?}", c.end);
}

/// A chain in memory at piece 4 after 8 chunks, pieces 0..4 closed.
fn chain_at_four(mem: &MemPieces, tapes: PieceTapes) -> PieceChain<MemPieces> {
    let defs = defs();
    let mut c = PieceChain::create_on(mem.clone(), OWNER, 0, &[], options(tapes)).unwrap();
    write(&mut c, &defs, 0, 9);
    assert_eq!(c.seq(), 4);
    c
}

/// Memory pieces whose removals fail after `left` of them: a crash part way through [PieceChain::remove].
struct Faulty {
    mem: MemPieces,
    left: usize,
}

impl PieceStore for Faulty {
    type Key = u64;
    type Error = MemError;
    type Sink = MemSink;
    type Source = MemPiece;
    fn key_of(&self, seq: u64) -> u64 {
        seq
    }
    fn create(&mut self, key: &u64) -> Result<MemSink, MemError> {
        self.mem.create(key)
    }
    fn reopen(&mut self, key: &u64) -> Result<Option<MemSink>, MemError> {
        self.mem.reopen(key)
    }
    fn closed(&self, key: &u64) -> Result<Option<MemPiece>, MemError> {
        self.mem.closed(key)
    }
    fn create_whole(&mut self, key: &u64) -> Result<MemSink, MemError> {
        self.mem.create_whole(key)
    }
    fn open_whole(&mut self, key: &u64) -> Result<Option<MemSink>, MemError> {
        self.mem.open_whole(key)
    }
    fn first_from(&self, from: u64) -> Result<Option<u64>, MemError> {
        self.mem.first_from(from)
    }
    fn remove(&mut self, key: &u64) -> Result<bool, MemError> {
        self.left = self.left.checked_sub(1).ok_or(MemError::Poisoned)?;
        PieceStore::remove(&mut self.mem, key)
    }
    fn drop_front(&mut self, key: &u64, to: u64) -> Result<Option<u64>, MemError> {
        self.mem.drop_front(key, to)
    }
}

/// Retention: the first pieces removed while the chain is written, a crash, a resume from the lowest piece left.
/// The chain reads as head cut, every link held; no row is written for a head.
#[test]
fn a_chain_resumes_after_its_first_pieces_are_removed() {
    let defs = defs();
    let mem = MemPieces::new();
    let mut c = chain_at_four(&mem, PieceTapes::Each);
    let before = mem.get(&4).unwrap().unwrap().to_vec().unwrap();
    c.remove(&[0, 1]).unwrap();
    assert_eq!(mem.keys().unwrap(), [2, 3, 4]);
    assert_eq!(c.closed().len(), 2);
    assert_eq!(
        mem.get(&4).unwrap().unwrap().to_vec().unwrap(),
        before,
        "a head removal writes no row"
    );
    write(&mut c, &defs, 9, 12);
    drop(c);

    let (mut c, _) =
        PieceChain::resume_on(mem.clone(), OWNER, 0, options(PieceTapes::Each)).unwrap();
    assert_eq!(c.seq(), 5);
    assert_eq!(c.closed().len(), 3);
    write(&mut c, &defs, 12, 15);
    c.close().unwrap();

    let r = report(&bytes_of(&mem));
    assert!(r.head_cut);
    assert_eq!(r.head_removed, 0);
    linked(&r);
    assert!(r.links.iter().all(|l| *l == ChainLink::Linked));
}

/// Retention that a crash stops after the first of three removals: a head cut at piece 1, which resumes.
#[test]
fn a_crash_during_retention_leaves_a_chain_that_resumes() {
    let defs = defs();
    let mem = MemPieces::new();
    let mut c = PieceChain::create_on(
        Faulty {
            mem: mem.clone(),
            left: 1,
        },
        OWNER,
        0,
        &[],
        options(PieceTapes::Each),
    )
    .unwrap();
    write(&mut c, &defs, 0, 9);
    let e = c.remove(&[0, 1, 2]).unwrap_err();
    assert!(matches!(e, WriteError::Sink { .. }), "{e:?}");
    // the piece deleted before the failure is no longer listed
    let listed: Vec<u64> = c
        .closed()
        .iter()
        .filter_map(|p| p.piece.ident)
        .map(|i| i.seq)
        .collect();
    assert_eq!(listed, [1, 2, 3]);
    drop(c);
    assert_eq!(mem.keys().unwrap(), [1, 2, 3, 4]);

    let (mut c, _) =
        PieceChain::resume_on(mem.clone(), OWNER, 0, options(PieceTapes::Each)).unwrap();
    assert_eq!((c.seq(), c.closed().len()), (4, 3));
    c.remove(&[1, 2]).unwrap();
    write(&mut c, &defs, 9, 10);
    c.close().unwrap();
    let r = report(&bytes_of(&mem));
    assert!(r.head_cut);
    linked(&r);
}

/// Pieces from the middle removed: named in the piece written, closed and synced there, then deleted. A crash
/// after more chunks, a resume over the hole, and the chain reads whole.
#[test]
fn a_chain_resumes_over_pieces_removed_from_its_middle() {
    let defs = defs();
    let mem = MemPieces::new();
    let mut c = chain_at_four(&mem, PieceTapes::Each);
    // --8<-- [start:remove]
    // pieces 1 and 2 named in piece 4, which is closed and synced, then taken out of the store
    c.remove(&[1, 2]).unwrap();
    assert_eq!(mem.keys().unwrap(), [0, 3, 4]);
    // --8<-- [end:remove]
    let named: Vec<u64> = c
        .writer()
        .unwrap()
        .removed()
        .iter()
        .map(|r| r.seq)
        .collect();
    assert_eq!(named, [1, 2]);
    // the rows are in a close of piece 4 before any piece goes: a crash right now reads whole
    let now = report(&bytes_of(&mem));
    assert_eq!(now.links[0], ChainLink::Removed { pieces: 2 });
    write(&mut c, &defs, 9, 10);
    drop(c);

    // --8<-- [start:resume]
    // a crash, then a resume over the hole: pieces 0 and 3 closed, piece 4 written on
    let (mut c, _) =
        PieceChain::resume_on(mem.clone(), OWNER, 0, options(PieceTapes::Each)).unwrap();
    assert_eq!((c.seq(), c.closed().len()), (4, 2));
    // --8<-- [end:resume]
    write(&mut c, &defs, 10, 13);
    c.close().unwrap();
    let r = report(&bytes_of(&mem));
    assert_eq!(r.links[0], ChainLink::Removed { pieces: 2 });
    linked(&r);
    assert!(r.is_whole());
    assert_eq!(r.removed.iter().map(|p| p.seq).collect::<Vec<_>>(), [1, 2]);
}

/// A crash between the rows and the deletes, or between two deletes: the chain resumes and reads whole, and the
/// removal is made again.
#[test]
fn a_crash_between_naming_and_deleting_leaves_a_chain_that_resumes() {
    for left in [0, 1] {
        let defs = defs();
        let mem = MemPieces::new();
        let mut c = PieceChain::create_on(
            Faulty {
                mem: mem.clone(),
                left,
            },
            OWNER,
            0,
            &[],
            options(PieceTapes::Each),
        )
        .unwrap();
        write(&mut c, &defs, 0, 9);
        c.remove(&[1, 2]).unwrap_err();
        drop(c);
        let r = report(&bytes_of(&mem));
        linked(&r);
        assert!(r.links.contains(&ChainLink::Linked), "{:?}", r.links);

        let (mut c, _) =
            PieceChain::resume_on(mem.clone(), OWNER, 0, options(PieceTapes::Each)).unwrap();
        assert_eq!(c.seq(), 4, "left {left}");
        c.remove(
            &c.closed()
                .iter()
                .filter_map(|p| p.piece.ident)
                .map(|i| i.seq)
                .filter(|s| (1..3).contains(s))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert_eq!(mem.keys().unwrap(), [0, 3, 4]);
        c.close().unwrap();
        let r = report(&bytes_of(&mem));
        assert_eq!(r.links[0], ChainLink::Removed { pieces: 2 });
        assert!(r.is_whole(), "left {left}: {r:?}");
    }
}

/// A removed piece's rows of a hole removed before are named again by the piece written, so the hole they cover
/// still links.
#[test]
fn removing_the_piece_that_names_a_hole_names_it_again() {
    let defs = defs();
    let mem = MemPieces::new();
    let mut c = chain_at_four(&mem, PieceTapes::Each);
    // piece 4 names 2; then piece 4 is cut and itself removed from the middle
    c.remove(&[2]).unwrap();
    write(&mut c, &defs, 9, 14);
    assert_eq!(c.seq(), 6);
    c.remove(&[4]).unwrap();
    let named: Vec<u64> = c
        .writer()
        .unwrap()
        .removed()
        .iter()
        .map(|r| r.seq)
        .collect();
    assert_eq!(named, [2, 4]);
    drop(c);
    let (c, _) = PieceChain::resume_on(mem.clone(), OWNER, 0, options(PieceTapes::Each)).unwrap();
    c.close().unwrap();
    let r = report(&bytes_of(&mem));
    assert!(r.is_whole(), "{r:?}");
}

/// A piece missing from the middle without a row naming it: the resume is refused, naming the missing pieces.
#[test]
fn a_resume_over_an_unnamed_hole_is_refused() {
    let mem = MemPieces::new();
    let c = chain_at_four(&mem, PieceTapes::Each);
    c.close().unwrap();
    assert!(mem.remove(&1).unwrap());
    assert!(mem.remove(&2).unwrap());
    let e = PieceChain::resume_on(mem.clone(), OWNER, 0, options(PieceTapes::Each))
        .err()
        .unwrap();
    assert!(
        matches!(e, WriteError::Resume(ResumeError::Gap { from: 1, to: 3 })),
        "{e:?}"
    );
    assert!(e.to_string().contains("1..3"), "{e}");
    assert!(!report(&bytes_of(&mem)).is_whole());
}

/// What [PieceChain::remove] refuses, before anything is written or deleted.
#[test]
fn a_removal_takes_only_closed_pieces_at_hand() {
    let mem = MemPieces::new();
    let mut c = chain_at_four(&mem, PieceTapes::Each);
    let refused = |e: WriteError<MemError>| match e {
        WriteError::Input(InputError::Removed(r)) => r,
        e => panic!("{e:?}"),
    };
    assert_eq!(
        refused(c.remove(&[4]).unwrap_err()),
        RemovedRefusal::NotAtHand { seq: 4 }
    );
    assert_eq!(
        refused(c.remove(&[1, 9]).unwrap_err()),
        RemovedRefusal::NotAtHand { seq: 9 }
    );
    assert_eq!(mem.keys().unwrap(), [0, 1, 2, 3, 4]);
    c.remove(&[]).unwrap();
}

/// A chain in one memory tape: its first pieces go as the tape's front ([MemPieces::drop_front]), and it resumes from
/// the first piece kept; a piece from the middle of the tape is refused.
#[test]
fn a_chain_in_one_memory_tape_resumes_after_its_front_is_dropped() {
    let defs = defs();
    let mem = MemPieces::new();
    let mut c = chain_at_four(&mem, PieceTapes::One);
    let refused = c.remove(&[2]).unwrap_err();
    assert!(
        matches!(
            refused,
            WriteError::Input(InputError::Removed(RemovedRefusal::InTape { seq: 2 }))
        ),
        "{refused:?}"
    );
    let third = c.closed()[2].piece.start;
    c.remove(&[0, 1]).unwrap();
    assert!(ww_tape::ByteSource::start(&mem.get(&0).unwrap().unwrap()).unwrap() <= third);
    write(&mut c, &defs, 9, 10);
    drop(c);

    let (mut c, _) =
        PieceChain::resume_on(mem.clone(), OWNER, 0, options(PieceTapes::One)).unwrap();
    assert_eq!((c.seq(), c.closed().len()), (4, 2));
    write(&mut c, &defs, 10, 13);
    let pieces = c.close().unwrap();
    assert_eq!(pieces.len(), 5);
    let tape = mem.get(&0).unwrap().unwrap();
    let r = TapeReader::open(tape).unwrap().chain();
    assert!(r.head_cut);
    linked(&r);
}

/// A writer's close in place gives the bytes of a close, a resume and the same calls after it: chunks and state
/// items on both sides of the close.
#[test]
fn a_close_in_place_writes_what_a_close_and_a_resume_do() {
    let mut defs = defs();
    crate::definitions::hold(
        &mut defs,
        core::slice::from_ref(&crate::definitions::state(5)),
    )
    .unwrap();
    let calls = |w: &mut TapeWriter<MemSink>, from: u64, to: u64| {
        for k in from..to {
            w.put(&defs, ww_tape::TrackNum::new(5), k as i64, &[k as u8])
                .unwrap();
            let (h, b) = chunk(0, k);
            w.write_chunk(&defs, &h, &column(&b)).unwrap();
        }
    };
    let mut a = crate::resume::begin(0);
    calls(&mut a, 0, 3);
    let (a, first) = a.close_and_go_on().unwrap();
    let (mut a, again) = a.close_and_go_on().unwrap();
    assert_eq!(again, first, "nothing since: nothing written");
    calls(&mut a, 3, 6);
    let a = close(a);

    let mut b = crate::resume::begin(0);
    calls(&mut b, 0, 3);
    let closed = close(b);
    assert_eq!(a[..closed.len()], closed[..]);
    let mut sink = MemSink::new();
    sink.write_all(&closed).unwrap();
    let (mut b, _) = TapeWriter::resume_on(sink, crate::resume::options(0)).unwrap();
    calls(&mut b, 3, 6);
    let b = close(b);
    let at = a.iter().zip(&b).position(|(x, y)| x != y);
    assert_eq!((a.len(), at), (b.len(), None));
    let r = TapeReader::open(&a[..]).unwrap();
    assert_eq!(r.closes(0).unwrap().len(), 2);
    assert!(r.verify(0).unwrap().holds);
}

/// Piece files: the head deleted by hand resumes as a head cut; a middle removal resumes over its hole; a file
/// deleted from the middle by hand is refused, naming it.
#[test]
fn a_chain_of_files_resumes_from_its_lowest_piece() {
    let defs = defs();
    let dir = scratch("chain_removal");
    let each = options(PieceTapes::Each);
    let mut c = ww_tape_fs::chain::create(&dir, OWNER, 0, &[], each.clone()).unwrap();
    write(&mut c, &defs, 0, 13);
    assert_eq!(c.seq(), 6);
    c.remove(&[3, 4]).unwrap();
    drop(c);
    std::fs::remove_file(dir.join(ww_tape_fs::piece_name(0))).unwrap();

    let (mut c, _) = ww_tape_fs::chain::resume(&dir, OWNER, 0, each.clone()).unwrap();
    assert_eq!((c.seq(), c.closed().len()), (6, 3));
    write(&mut c, &defs, 13, 14);
    c.close().unwrap();
    let r = report(&files_of(&dir));
    assert!(r.head_cut);
    assert_eq!(r.links[1], ChainLink::Removed { pieces: 2 });
    linked(&r);

    std::fs::remove_file(dir.join(ww_tape_fs::piece_name(2))).unwrap();
    let e = ww_tape_fs::chain::resume(&dir, OWNER, 0, each.clone())
        .err()
        .unwrap();
    assert!(
        matches!(e, WriteError::Resume(ResumeError::Gap { from: 2, to: 5 })),
        "{e:?}"
    );

    for seq in [1, 5, 6] {
        std::fs::remove_file(dir.join(ww_tape_fs::piece_name(seq))).unwrap();
    }
    let e = ww_tape_fs::chain::resume(&dir, OWNER, 0, each)
        .err()
        .unwrap();
    assert!(
        matches!(
            e,
            WriteError::Sink {
                error: ww_tape_fs::FsError::NoPiece(_),
                ..
            }
        ),
        "{e:?}"
    );
}

/// The piece files of `dir` in name order, back to back.
fn files_of(dir: &Path) -> Vec<u8> {
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    names.sort();
    names
        .iter()
        .flat_map(|p| std::fs::read(p).unwrap())
        .collect()
}

/// Every piece file of `dir`, by name, with its bytes.
fn snapshot(dir: &Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let p = e.unwrap().path();
            let b = std::fs::read(&p).unwrap();
            (p, b)
        })
        .collect();
    files.sort();
    files
}

/// A resume refused for a hole writes nothing: neither the torn last piece after the hole nor a stray piece past a
/// missing end is cut or marked.
#[test]
fn a_refused_resume_writes_nothing() {
    let defs = defs();
    let each = options(PieceTapes::Each);
    // the last piece torn inside its last block, piece 2 gone without a row
    let dir = scratch("refused_torn");
    let mut c = ww_tape_fs::chain::create(&dir, OWNER, 0, &[], each.clone()).unwrap();
    write(&mut c, &defs, 0, 10);
    drop(c);
    let last = dir.join(ww_tape_fs::piece_name(4));
    let len = std::fs::metadata(&last).unwrap().len();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&last)
        .unwrap()
        .set_len(len - 7)
        .unwrap();
    std::fs::remove_file(dir.join(ww_tape_fs::piece_name(2))).unwrap();
    let before = snapshot(&dir);
    let e = ww_tape_fs::chain::resume(&dir, OWNER, 0, each.clone())
        .err()
        .unwrap();
    assert!(
        matches!(e, WriteError::Resume(ResumeError::Gap { from: 2, to: 3 })),
        "{e:?}"
    );
    assert!(snapshot(&dir) == before, "a refused resume wrote");

    // a crash between a cut and the next piece's start, and a stray piece far past it
    let dir = scratch("refused_stray");
    let mut c = ww_tape_fs::chain::create(&dir, OWNER, 0, &[], each.clone()).unwrap();
    write(&mut c, &defs, 0, 10);
    drop(c);
    let torn = std::fs::read(&last).unwrap();
    let stray = dir.join(ww_tape_fs::piece_name(9));
    std::fs::write(&stray, &torn[..torn.len() - 7]).unwrap();
    std::fs::remove_file(dir.join(ww_tape_fs::piece_name(4))).unwrap();
    let before = snapshot(&dir);
    let e = ww_tape_fs::chain::resume(&dir, OWNER, 0, each)
        .err()
        .unwrap();
    assert!(
        matches!(e, WriteError::Resume(ResumeError::Gap { from: 4, to: 9 })),
        "{e:?}"
    );
    assert!(snapshot(&dir) == before, "a refused resume wrote");
}

/// The synced length of each tape of a [Durable] store, by key.
type SyncedLens = std::sync::Arc<std::sync::Mutex<std::collections::BTreeMap<u64, u64>>>;

/// Memory pieces that remember how much of each tape was synced: [Durable::crash] keeps only that, as a disk does.
/// A tape is created synced and empty, as a file and its name are; a removal is durable.
#[derive(Clone, Default)]
struct Durable {
    mem: MemPieces,
    synced: SyncedLens,
    /// Syncs that still succeed; None for every one
    syncs_left: std::sync::Arc<std::sync::Mutex<Option<usize>>>,
}

struct DurableSink {
    inner: MemSink,
    key: u64,
    synced: SyncedLens,
    syncs_left: std::sync::Arc<std::sync::Mutex<Option<usize>>>,
}

impl TapeSink for DurableSink {
    type Error = MemError;
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), MemError> {
        self.inner.write_all(bytes)
    }
    fn sync(&mut self) -> Result<(), MemError> {
        if let Some(left) = self.syncs_left.lock().unwrap().as_mut() {
            *left = left.checked_sub(1).ok_or(MemError::Poisoned)?;
        }
        let len = self.inner.base() + self.inner.size()?;
        self.synced.lock().unwrap().insert(self.key, len);
        self.inner.sync()
    }
    fn close(&mut self) -> Result<(), MemError> {
        self.inner.close()
    }
    fn size(&self) -> Result<u64, MemError> {
        self.inner.size()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), MemError> {
        self.inner.read_at(offset, buf)
    }
    fn truncate(&mut self, len: u64) -> Result<(), MemError> {
        self.inner.truncate(len)?;
        self.sync()
    }
    fn base(&self) -> u64 {
        self.inner.base()
    }
    fn front(&self) -> Result<u64, MemError> {
        self.inner.front()
    }
    fn holds_pieces(&self) -> bool {
        self.inner.holds_pieces()
    }
    fn rebase(&mut self, base: u64) -> Result<(), MemError> {
        self.inner.rebase(base)
    }
}

impl Durable {
    fn wrap(&self, key: u64, inner: MemSink) -> DurableSink {
        DurableSink {
            inner,
            key,
            synced: self.synced.clone(),
            syncs_left: self.syncs_left.clone(),
        }
    }

    /// Let `n` more syncs succeed, and fail every one after them.
    fn fail_after(&self, n: usize) {
        *self.syncs_left.lock().unwrap() = Some(n);
    }

    /// The tapes as a crash leaves them: each cut to its synced length, marked closed as it was.
    fn crash(&self) -> MemPieces {
        self.crash_keeping(false)
    }

    /// [Self::crash]; with `written`, each tape keeps every byte written, as a disk that wrote them unasked does.
    fn crash_keeping(&self, written: bool) -> MemPieces {
        let after = MemPieces::new();
        let synced = self.synced.lock().unwrap();
        for key in self.mem.keys().unwrap() {
            let tape = self.mem.get(&key).unwrap().unwrap();
            let bytes = tape.to_vec().unwrap();
            let len = match written {
                true => bytes.len(),
                false => synced.get(&key).copied().unwrap_or(0) as usize,
            };
            let mut sink = after.create(&key).unwrap();
            sink.write_all(&bytes[..len.min(bytes.len())]).unwrap();
            if tape.is_closed() {
                sink.close().unwrap();
            }
        }
        after
    }
}

impl PieceStore for Durable {
    type Key = u64;
    type Error = MemError;
    type Sink = DurableSink;
    type Source = MemPiece;
    fn key_of(&self, seq: u64) -> u64 {
        seq
    }
    fn create(&mut self, key: &u64) -> Result<DurableSink, MemError> {
        let sink = self.mem.create(key)?;
        self.synced.lock().unwrap().insert(*key, 0);
        Ok(self.wrap(*key, sink))
    }
    fn reopen(&mut self, key: &u64) -> Result<Option<DurableSink>, MemError> {
        Ok(self.mem.reopen(key)?.map(|s| self.wrap(*key, s)))
    }
    fn closed(&self, key: &u64) -> Result<Option<MemPiece>, MemError> {
        self.mem.closed(key)
    }
    fn create_whole(&mut self, key: &u64) -> Result<DurableSink, MemError> {
        let sink = self.mem.create_whole(key)?;
        self.synced.lock().unwrap().insert(*key, 0);
        Ok(self.wrap(*key, sink))
    }
    fn open_whole(&mut self, key: &u64) -> Result<Option<DurableSink>, MemError> {
        Ok(self.mem.open_whole(key)?.map(|s| self.wrap(*key, s)))
    }
    fn first_from(&self, from: u64) -> Result<Option<u64>, MemError> {
        self.mem.first_from(from)
    }
    fn remove(&mut self, key: &u64) -> Result<bool, MemError> {
        PieceStore::remove(&mut self.mem, key)
    }
    fn drop_front(&mut self, key: &u64, to: u64) -> Result<Option<u64>, MemError> {
        self.mem.drop_front(key, to)
    }
}

/// Retention right after a cut removes every closed piece, so the piece being written becomes the chain's lowest
/// while its head is not yet synced. A crash then must still find that head: the removal syncs it first.
#[test]
fn retention_right_after_a_cut_survives_a_crash() {
    let defs = defs();
    let store = Durable::default();
    let mut c =
        PieceChain::create_on(store.clone(), OWNER, 0, &[], options(PieceTapes::Each)).unwrap();
    write(&mut c, &defs, 0, 8);
    c.cut_now(&defs).unwrap();
    assert_eq!(c.seq(), 4);
    c.remove(&[0, 1, 2, 3]).unwrap();
    let crashed = store.crash();
    drop(c);
    assert_eq!(crashed.keys().unwrap(), [4]);

    let (mut c, _) =
        PieceChain::resume_on(crashed.clone(), OWNER, 0, options(PieceTapes::Each)).unwrap();
    assert_eq!((c.seq(), c.closed().len()), (4, 0));
    write(&mut c, &defs, 8, 11);
    c.close().unwrap();
    let r = report(&bytes_of(&crashed));
    assert!(r.head_cut);
    linked(&r);
}

/// A crash while the rows of a removal are written, before their sync: the first sync of the close fails (no close
/// record written), or the second (close and tail written, not synced), and the disk keeps the synced bytes or every
/// byte written. No piece was deleted; the chain resumes and links whole, and the removal is made again.
#[test]
fn a_crash_before_the_rows_are_synced_leaves_every_piece() {
    for (syncs, written) in [(0, false), (0, true), (1, false), (1, true)] {
        let defs = defs();
        let store = Durable::default();
        let mut c =
            PieceChain::create_on(store.clone(), OWNER, 0, &[], options(PieceTapes::Each)).unwrap();
        write(&mut c, &defs, 0, 9);
        store.fail_after(syncs);
        c.remove(&[1, 2]).unwrap_err();
        let crashed = store.crash_keeping(written);
        drop(c);
        assert_eq!(
            crashed.keys().unwrap(),
            [0, 1, 2, 3, 4],
            "{syncs} {written}"
        );

        let (mut c, _) =
            PieceChain::resume_on(crashed.clone(), OWNER, 0, options(PieceTapes::Each)).unwrap();
        assert_eq!((c.seq(), c.closed().len()), (4, 4));
        c.remove(&[1, 2]).unwrap();
        write(&mut c, &defs, 9, 11);
        c.close().unwrap();
        let r = report(&bytes_of(&crashed));
        assert_eq!(r.links[0], ChainLink::Removed { pieces: 2 });
        assert!(r.is_whole(), "{syncs} {written}: {r:?}");
    }
}

/// Where each block of `bytes` starts, and its end.
fn boundaries(bytes: &[u8]) -> Vec<usize> {
    let mut at = 0usize;
    let mut out = vec![0];
    while at + 12 <= bytes.len() {
        let len = u32::from_le_bytes(bytes[at + 8..at + 12].try_into().unwrap()) as usize;
        if len == 0 || at + len > bytes.len() {
            break;
        }
        at += len;
        out.push(at);
    }
    out
}

/// The number of chunks of track 0 the tapes of `mem` hold, from the newest one's first item: chunk `k` starts at
/// item `k * 50`.
fn chunks_written(mem: &MemPieces) -> u64 {
    let mut next = 0;
    for key in mem.keys().unwrap() {
        let bytes = mem.get(&key).unwrap().unwrap().to_vec().unwrap();
        let ends = boundaries(&bytes);
        for w in ends.windows(2) {
            if let Ok(b) = ww_tape::Block::read(&bytes[w[0]..w[1]])
                && let Some(c) = b.chunk()
            {
                next = next.max(c.head().first_item.get() / 50 + 1);
            }
        }
    }
    next
}

/// A chain with pieces removed from its middle, crashed at every block boundary written after the removal synced its
/// rows, resumed and given the chunks it lost: the same pieces, byte for byte, as the run without a crash.
#[test]
fn a_resume_after_a_removal_at_every_block_boundary_matches_the_run_without_a_crash() {
    let defs = defs();
    let mem = MemPieces::new();
    let mut c = chain_at_four(&mem, PieceTapes::Each);
    c.remove(&[1, 2]).unwrap();
    // what the removal synced: the piece written up to the rows' close
    let synced = mem.get(&4).unwrap().unwrap().to_vec().unwrap().len();
    write(&mut c, &defs, 9, 15);
    c.close().unwrap();
    let keys = mem.keys().unwrap();
    assert_eq!(keys, [0, 3, 4, 5, 6, 7]);
    let reference: Vec<Vec<u8>> = keys
        .iter()
        .map(|k| mem.get(k).unwrap().unwrap().to_vec().unwrap())
        .collect();

    let mut runs = 0;
    for (i, &key) in keys.iter().enumerate().skip(2) {
        let piece = &reference[i];
        for cut in boundaries(piece) {
            if key == 4 && cut < synced {
                continue;
            }
            let crashed = MemPieces::new();
            for (j, &k) in keys.iter().enumerate().take(i + 1) {
                let mut sink = crashed.create(&k).unwrap();
                let bytes = if j == i {
                    &piece[..cut]
                } else {
                    &reference[j][..]
                };
                sink.write_all(bytes).unwrap();
                if j < i {
                    sink.close().unwrap();
                }
            }
            let (mut c, _) =
                PieceChain::resume_on(crashed.clone(), OWNER, 0, options(PieceTapes::Each))
                    .unwrap_or_else(|e| panic!("piece {key} cut at {cut}: {e:?}"));
            let from = chunks_written(&crashed);
            write(&mut c, &defs, from, 15);
            c.close().unwrap();
            assert_eq!(crashed.keys().unwrap(), keys, "piece {key} cut at {cut}");
            for (j, k) in keys.iter().enumerate() {
                let got = crashed.get(k).unwrap().unwrap().to_vec().unwrap();
                assert!(
                    got == reference[j],
                    "piece {key} cut at {cut}: piece {k} differs"
                );
            }
            runs += 1;
        }
    }
    assert!(runs > 20, "{runs} crashes");
}

/// A tape of several pieces whose every closed piece was dropped, and whose next piece has no byte: the piece can't
/// start again and its place in the chain is unknown, so the resume names no sequence number.
#[test]
fn a_tape_dropped_to_a_lost_head_names_no_piece() {
    let defs = defs();
    let mem = MemPieces::new();
    let mut c =
        PieceChain::create_on(mem.clone(), OWNER, 0, &[], options(PieceTapes::One)).unwrap();
    write(&mut c, &defs, 0, 9);
    let starts: Vec<u64> = c.closed().iter().map(|p| p.piece.start).collect();
    let end = c
        .closed()
        .last()
        .map(|p| p.piece.start + p.piece.len)
        .unwrap();
    drop(c);
    let bytes = mem.get(&0).unwrap().unwrap().to_vec().unwrap();

    // the same closed pieces, the next piece cut off before its head, then every closed piece dropped
    let torn = MemPieces::new();
    let mut sink = torn.create_whole(&0).unwrap();
    sink.write_all(&bytes[..end as usize]).unwrap();
    for s in starts.iter().skip(1).chain([&end]) {
        sink.rebase(*s).unwrap();
    }
    drop(sink);
    assert_eq!(torn.drop_front(&0, end).unwrap(), Some(end));
    let e = PieceChain::resume_on(torn, OWNER, 0, options(PieceTapes::One))
        .err()
        .unwrap();
    assert!(
        matches!(e, WriteError::Resume(ResumeError::HeadLost { seq: None })),
        "{e:?}"
    );
}

/// The lowest piece at hand cut off inside its head, the pieces before it deleted: refused before its bytes are cut.
#[test]
fn a_lowest_piece_without_its_head_is_refused_untouched() {
    let defs = defs();
    let each = options(PieceTapes::Each);
    let dir = scratch("lowest_headless");
    let mut c = ww_tape_fs::chain::create(&dir, OWNER, 0, &[], each.clone()).unwrap();
    write(&mut c, &defs, 0, 9);
    drop(c);
    for seq in 0..4 {
        std::fs::remove_file(dir.join(ww_tape_fs::piece_name(seq))).unwrap();
    }
    let last = dir.join(ww_tape_fs::piece_name(4));
    std::fs::OpenOptions::new()
        .write(true)
        .open(&last)
        .unwrap()
        .set_len(20)
        .unwrap();
    let before = snapshot(&dir);
    let e = ww_tape_fs::chain::resume(&dir, OWNER, 0, each)
        .err()
        .unwrap();
    assert!(
        matches!(
            e,
            WriteError::Resume(ResumeError::HeadLost { seq: Some(4) })
        ),
        "{e:?}"
    );
    assert!(snapshot(&dir) == before, "a refused resume wrote");
}

/// The lowest piece at hand after deleted ones, a byte of its head flipped: refused untouched with the refusal of the
/// piece's own resume, not as a head lost: good blocks after the head, or (the piece cut to its head) damage.
#[test]
fn a_damaged_lowest_piece_is_refused_with_its_own_error() {
    let defs = defs();
    let each = options(PieceTapes::Each);
    for whole in [true, false] {
        let dir = scratch(&format!("lowest_damaged_{whole}"));
        let mut c = ww_tape_fs::chain::create(&dir, OWNER, 0, &[], each.clone()).unwrap();
        write(&mut c, &defs, 0, 9);
        drop(c);
        for seq in 0..4 {
            std::fs::remove_file(dir.join(ww_tape_fs::piece_name(seq))).unwrap();
        }
        let last = dir.join(ww_tape_fs::piece_name(4));
        let mut bytes = std::fs::read(&last).unwrap();
        if !whole {
            let head = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
            bytes.truncate(usize::try_from(head).unwrap());
        }
        bytes[20] ^= 0x40;
        std::fs::write(&last, &bytes).unwrap();
        let copy = scratch(&format!("lowest_damaged_copy_{whole}")).join("piece");
        std::fs::write(&copy, &bytes).unwrap();
        let own = ww_tape_fs::writer::resume(&copy, each.writer.clone())
            .err()
            .unwrap();
        let before = snapshot(&dir);
        let e = ww_tape_fs::chain::resume(&dir, OWNER, 0, each.clone())
            .err()
            .unwrap();
        match whole {
            true => assert!(
                matches!(
                    own,
                    WriteError::Resume(ResumeError::GoodBlockAfterCut { .. })
                ),
                "{own:?}"
            ),
            false => assert!(
                matches!(own, WriteError::Resume(ResumeError::Damaged { .. })),
                "{own:?}"
            ),
        }
        assert_eq!(format!("{e:?}"), format!("{own:?}"));
        assert!(snapshot(&dir) == before, "a refused resume wrote");
    }
}

/// A piece cut whole with torn bytes after its tail (a crash during the cut), the next piece missing: as the chain's
/// end it is repaired and the next piece starts; with a hole past it the resume is refused and nothing is cut.
#[test]
fn a_torn_cut_piece_is_repaired_only_when_the_resume_goes_on() {
    let defs = defs();
    let each = options(PieceTapes::Each);
    for end in [true, false] {
        let dir = scratch(&format!("torn_cut_{end}"));
        let mut c = ww_tape_fs::chain::create(&dir, OWNER, 0, &[], each.clone()).unwrap();
        write(&mut c, &defs, 0, 13);
        drop(c);
        let third = dir.join(ww_tape_fs::piece_name(3));
        let whole = std::fs::read(&third).unwrap();
        let mut torn = whole.clone();
        torn.extend_from_slice(b"WWT");
        std::fs::write(&third, &torn).unwrap();
        let gone: &[u64] = if end { &[4, 5, 6] } else { &[4] };
        for seq in gone {
            std::fs::remove_file(dir.join(ww_tape_fs::piece_name(*seq))).unwrap();
        }
        let before = snapshot(&dir);
        let r = ww_tape_fs::chain::resume(&dir, OWNER, 0, each.clone());
        if end {
            let (c, _) = r.unwrap();
            assert_eq!((c.seq(), c.closed().len()), (4, 4));
            assert_eq!(
                std::fs::read(&third).unwrap(),
                whole,
                "the torn bytes are cut"
            );
        } else {
            let e = r.err().unwrap();
            assert!(
                matches!(e, WriteError::Resume(ResumeError::Gap { from: 4, to: 5 })),
                "{e:?}"
            );
            assert!(snapshot(&dir) == before, "a refused resume wrote");
        }
    }
}
