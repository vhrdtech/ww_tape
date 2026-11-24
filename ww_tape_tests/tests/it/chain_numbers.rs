//! A chain resume gives every track and clock the number it had ([ww_tape::Resumed::numbers]), also when it starts
//! a new piece and reads none of the pieces before it as its own: after a crash between pieces, after a cut off
//! inside the new piece's head, and over a removed front piece; and when it writes on after the last piece's close,
//! whose resume reads only what follows that close.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::chain_removal::{bytes_of, linked, report};
use crate::definitions::{clock, hold, on_tracks, track};
use crate::pieces::{chunk, column, scratch};
use shrink_wrap::prelude::RefVec;
use ww_tape::reader::ByteSource;
use ww_tape::sink::TapeSink;
use ww_tape::{
    BlockId, BlockKind, BlockWriter, ChainOptions, ClockNum, ClockTime, CutPolicy, Definitions,
    Describe, Foreign, Items, MemError, MemPiece, MemPieces, MemSink, OwnerId, PieceChain,
    PieceStore, PieceTapes, Resumed, Schema, Slot, SyncPolicy, TapeReader, TrackDef, TrackNum,
    WriteError, WriterOptions,
};

const OWNER: OwnerId = OwnerId([0x4e; 32]);

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

/// Clocks 1 and 2, events tracks 1 to 4 on clock 1 and track 5 on clock 2.
fn batch() -> Vec<Describe<'static>> {
    let mut out = vec![clock(1), clock(2)];
    out.extend((1..=4).map(|n| track(n, 1)));
    out.push(track(5, 2));
    out
}

/// A record block numbered `id` of one record on track `track` at `ticks` of clock `clock`.
fn record_on(id: BlockId, track: u32, clock: u32, ticks: i64) -> Vec<u8> {
    let mut buf = vec![0u8; 256];
    let mut b =
        BlockWriter::new(&mut buf, BlockKind::Records, id, &[ClockNum::new(clock)]).unwrap();
    b.push(
        TrackNum::new(track),
        ClockTime::new(ClockNum::new(clock), ticks),
        &[&[7u8; 8]],
    )
    .unwrap();
    let len = b.finish();
    buf.truncate(len);
    buf
}

/// Chunks `from..to` of track 1, two to a piece.
fn chunks<T: PieceStore>(c: &mut PieceChain<T>, defs: &Definitions, from: u64, to: u64) {
    for k in from..to {
        let (h, b) = chunk(0, k);
        c.write_chunk(defs, &h, &column(&b)).unwrap();
    }
}

/// The story up to the cut that starts piece 4, its definitions returned: piece 0 holds tracks 1 to 3 on clock 1,
/// piece 1 tracks 4 and 5 (clock 2 only there), both closed in it, so no later piece names them; pieces 2 and 3 hold
/// track 1. With `remove_front`, piece 0 is removed before the cut.
fn write_story<T: PieceStore>(c: &mut PieceChain<T>, remove_front: bool) -> Definitions {
    let mut defs = Definitions::new();
    hold(&mut defs, &batch()).unwrap();
    chunks(c, &defs, 0, 2);
    for n in [2, 3] {
        let id = c.writer().unwrap().next_id();
        c.write_block(&defs, &on_tracks(id, &[(n, 30, b"x")]))
            .unwrap();
    }
    chunks(c, &defs, 2, 3);
    assert_eq!(c.seq(), 1);
    for (n, clock) in [(4, 1), (5, 2)] {
        let id = c.writer().unwrap().next_id();
        c.write_block(&defs, &record_on(id, n, clock, 40)).unwrap();
        assert!(defs.close(TrackNum::new(n)));
    }
    chunks(c, &defs, 3, 8);
    assert_eq!(c.seq(), 3);
    if remove_front {
        c.remove(&[0]).unwrap();
    }
    c.cut_now(&defs).unwrap();
    assert_eq!(c.seq(), 4);
    defs
}

/// The numbers a resume gives are those the story's definitions gave: defining the tracks and clocks again, in
/// another order, gives each its number, a new track takes the number the story's definitions would give it, and the
/// chain writes on with them.
fn writes_on<T: PieceStore>(mut c: PieceChain<T>, resumed: &Resumed, defs: &Definitions) {
    assert_eq!(&resumed.numbers, defs.numbers());
    let mut again = Definitions::with_numbers(resumed.numbers.clone());
    let batch = batch();
    for d in batch.iter().take(2).rev().chain(batch[2..].iter().rev()) {
        match d {
            Describe::Clock(c) => assert_eq!(again.define_clock(c.spec()), Ok(c.number)),
            Describe::Track(t) => assert_eq!(again.define_track(t.spec()), Ok(t.number)),
            _ => unreachable!("a clock or a track"),
        }
    }
    let Describe::Track(new) = track(9, 1) else {
        unreachable!("track() gives a track")
    };
    let mut probe = defs.clone();
    let want = probe.define_track(new.spec()).unwrap();
    assert_eq!(again.define_track(new.spec()), Ok(want));
    chunks(&mut c, &again, 8, 11);
    c.close().unwrap();
}

/// How a crash leaves the piece the cut started.
#[derive(Clone, Copy, Debug)]
enum Crash {
    /// Gone: the cut's close was written, the next piece not.
    BetweenPieces,
    /// Cut off inside its head.
    InHead,
    /// Kept with its head and a chunk, unclosed.
    Unclosed,
    /// No crash: the chain closed ([PieceChain::close]), the usual append after a clean shutdown.
    Closed,
    /// Closed, resumed, a chunk written after the close, then the crash.
    AfterClose,
}

const CRASHES: [Crash; 5] = [
    Crash::BetweenPieces,
    Crash::InHead,
    Crash::Unclosed,
    Crash::Closed,
    Crash::AfterClose,
];

/// End the chain `c` of the story (its definitions `defs`) as `crash` leaves it, before the crash's bytes are cut:
/// dropped, or closed after a chunk, four record blocks, and clock 3 and track 6 on it are defined in the last piece;
/// with [Crash::AfterClose] then resumed by `resume`, a chunk written and dropped.
fn end_as<T: PieceStore>(
    mut c: PieceChain<T>,
    defs: &mut Definitions,
    crash: Crash,
    resume: impl FnOnce() -> PieceChain<T>,
) {
    match crash {
        Crash::BetweenPieces | Crash::InHead => drop(c),
        Crash::Unclosed => chunks(&mut c, defs, 8, 9),
        Crash::Closed | Crash::AfterClose => {
            // past the puts after the head, which a resume of the piece reads
            chunks(&mut c, defs, 8, 9);
            for k in 0..4 {
                let id = c.writer().unwrap().next_id();
                c.write_block(defs, &on_tracks(id, &[(2, 60 + k, b"z")]))
                    .unwrap();
            }
            hold(defs, &[clock(3), track(6, 3)]).unwrap();
            let id = c.writer().unwrap().next_id();
            c.write_block(defs, &record_on(id, 6, 3, 50)).unwrap();
            c.close().unwrap();
        }
    }
    if let Crash::AfterClose = crash {
        let mut c = resume();
        chunks(&mut c, defs, 8, 9);
    }
}

/// The story in memory, crashed as `crash`, resumed, written on and checked.
fn in_memory(tapes: PieceTapes, crash: Crash, remove_front: bool) {
    let mem = MemPieces::new();
    let mut c = PieceChain::create_on(mem.clone(), OWNER, 0, &[], options(tapes)).unwrap();
    let mut defs = write_story(&mut c, remove_front);
    end_as(c, &mut defs, crash, || {
        PieceChain::resume_on(mem.clone(), OWNER, 0, options(tapes))
            .unwrap()
            .0
    });
    let mem = crashed(&mem, tapes, crash);
    let (c, resumed) = PieceChain::resume_on(mem.clone(), OWNER, 0, options(tapes))
        .unwrap_or_else(|e| panic!("{tapes:?} {crash:?} {remove_front}: {e:?}"));
    writes_on(c, &resumed, &defs);
    let r = report(&bytes_of(&mem));
    assert_eq!(r.head_cut, remove_front, "{tapes:?} {crash:?}");
    linked(&r);
}

/// The pieces of `mem` as a crash `crash` leaves them, in a store of their own.
fn crashed(mem: &MemPieces, tapes: PieceTapes, crash: Crash) -> MemPieces {
    let after = MemPieces::new();
    for key in mem.keys().unwrap() {
        let tape = mem.get(&key).unwrap().unwrap();
        let bytes = tape.to_vec().unwrap();
        let start = match tapes {
            PieceTapes::Each if key == 4 => 0,
            PieceTapes::Each => bytes.len() as u64,
            PieceTapes::One => last_start(&bytes),
        };
        let len = match crash {
            Crash::BetweenPieces if tapes == PieceTapes::Each && key == 4 => continue,
            Crash::BetweenPieces => start.min(bytes.len() as u64),
            Crash::InHead => (start + 16).min(bytes.len() as u64),
            Crash::Unclosed | Crash::Closed | Crash::AfterClose => bytes.len() as u64,
        } as usize;
        let mut sink = match tapes {
            PieceTapes::Each => after.create(&key).unwrap(),
            PieceTapes::One => after.create_whole(&key).unwrap(),
        };
        sink.write_all(&bytes[..len]).unwrap();
        if tape.is_closed() && len == bytes.len() {
            sink.close().unwrap();
        }
    }
    after
}

/// Where the last piece of the tape `bytes` starts.
fn last_start(bytes: &[u8]) -> u64 {
    TapeReader::open(bytes)
        .unwrap()
        .pieces()
        .last()
        .unwrap()
        .start
}

#[test]
fn a_chain_resume_keeps_the_numbers_of_tracks_and_clocks() {
    for tapes in [PieceTapes::Each, PieceTapes::One] {
        for crash in CRASHES {
            in_memory(tapes, crash, false);
        }
    }
}

/// A removed front piece (WRITE-16): the lowest piece left gives the numbers of the tracks carried from it.
#[test]
fn a_chain_resume_over_a_removed_front_piece_keeps_the_numbers() {
    for tapes in [PieceTapes::Each, PieceTapes::One] {
        for crash in CRASHES {
            in_memory(tapes, crash, true);
        }
    }
}

/// The story in files, crashed as each [Crash], resumed through `ww_tape_fs`; piece files with the front piece removed.
#[test]
fn a_chain_of_files_resumes_with_its_numbers() {
    for tapes in [PieceTapes::Each, PieceTapes::One] {
        for crash in CRASHES {
            let dir = scratch(&format!("chain_numbers_{tapes:?}_{crash:?}"));
            let at = match tapes {
                PieceTapes::Each => dir.clone(),
                PieceTapes::One => dir.join("chain.wwt"),
            };
            let mut c = ww_tape_fs::chain::create(&at, OWNER, 0, &[], options(tapes)).unwrap();
            // a file of pieces drops no front ([FilePieces](ww_tape_fs::FilePieces))
            let remove_front = tapes == PieceTapes::Each;
            let mut defs = write_story(&mut c, remove_front);
            end_as(c, &mut defs, crash, || {
                ww_tape_fs::chain::resume(&at, OWNER, 0, options(tapes))
                    .unwrap()
                    .0
            });
            let (file, start) = match tapes {
                PieceTapes::Each => (dir.join(ww_tape_fs::chain::piece_name(4)), 0),
                PieceTapes::One => (at.clone(), last_start(&std::fs::read(&at).unwrap())),
            };
            match crash {
                Crash::BetweenPieces if tapes == PieceTapes::Each => {
                    std::fs::remove_file(&file).unwrap();
                }
                Crash::BetweenPieces => cut_file(&file, start),
                Crash::InHead => cut_file(&file, start + 16),
                Crash::Unclosed | Crash::Closed | Crash::AfterClose => {}
            }
            let (c, resumed) = ww_tape_fs::chain::resume(&at, OWNER, 0, options(tapes))
                .unwrap_or_else(|e: WriteError<_>| panic!("{tapes:?} {crash:?}: {e:?}"));
            writes_on(c, &resumed, &defs);
            let r = report(&files_bytes(&dir));
            assert_eq!(r.head_cut, remove_front, "{tapes:?} {crash:?}");
            linked(&r);
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }
}

/// Cut the file at `path` to `len` bytes, as a crash leaves it.
fn cut_file(path: &Path, len: u64) {
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_len(len)
        .unwrap();
}

/// The files in `dir`, by name, back to back.
fn files_bytes(dir: &Path) -> Vec<u8> {
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

/// A definition that doesn't read is counted ([Resumed::numbers_unread]), never skipped silently: the block holding
/// clock 2's definition in piece 1 damaged, the resume after a crash between pieces counts it and gives clock 2 no
/// number; track 5 keeps its number from the close's rows.
#[test]
fn a_chain_resume_counts_the_definitions_it_could_not_read() {
    let mem = MemPieces::new();
    let mut c =
        PieceChain::create_on(mem.clone(), OWNER, 0, &[], options(PieceTapes::Each)).unwrap();
    let defs = write_story(&mut c, false);
    drop(c);
    let after = MemPieces::new();
    for key in mem.keys().unwrap().into_iter().filter(|k| *k != 4) {
        let tape = mem.get(&key).unwrap().unwrap();
        let mut bytes = tape.to_vec().unwrap();
        if key == 1 {
            let at = TapeReader::open(&bytes[..])
                .unwrap()
                .definitions_at(0, u64::MAX)
                .unwrap()
                .items[&Slot::Clock {
                number: ClockNum::new(2),
            }]
                .offset as usize;
            bytes[at + 40] ^= 0xff;
        }
        let mut sink = after.create(&key).unwrap();
        sink.write_all(&bytes).unwrap();
        sink.close().unwrap();
    }
    let (_, resumed) = PieceChain::resume_on(after, OWNER, 0, options(PieceTapes::Each)).unwrap();
    assert!(resumed.numbers_unread >= 1, "{}", resumed.numbers_unread);
    let Describe::Clock(two) = clock(2) else {
        unreachable!("clock() gives a clock")
    };
    assert_eq!(resumed.numbers.clock(two.key), None);
    let Describe::Track(five) = track(5, 2) else {
        unreachable!("track() gives a track")
    };
    assert_eq!(
        resumed.numbers.track(five.id(), five.owner),
        defs.numbers().track(five.id(), five.owner)
    );
}

/// Memory pieces whose closed pieces count the bytes read from them.
struct Counted {
    mem: MemPieces,
    read: Arc<AtomicU64>,
}

/// A closed piece of [Counted].
struct CountedPiece {
    inner: MemPiece,
    read: Arc<AtomicU64>,
}

impl ByteSource for CountedPiece {
    type Error = MemError;
    fn size(&self) -> Result<u64, MemError> {
        self.inner.size()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), MemError> {
        self.read.fetch_add(buf.len() as u64, Ordering::Relaxed);
        self.inner.read_at(offset, buf)
    }
    fn start(&self) -> Result<u64, MemError> {
        self.inner.start()
    }
}

impl PieceStore for Counted {
    type Key = u64;
    type Error = MemError;
    type Sink = MemSink;
    type Source = CountedPiece;
    fn key_of(&self, seq: u64) -> u64 {
        seq
    }
    fn create(&mut self, key: &u64) -> Result<MemSink, MemError> {
        self.mem.create(key)
    }
    fn reopen(&mut self, key: &u64) -> Result<Option<MemSink>, MemError> {
        self.mem.reopen(key)
    }
    fn closed(&self, key: &u64) -> Result<Option<CountedPiece>, MemError> {
        Ok(self.mem.closed(key)?.map(|inner| CountedPiece {
            inner,
            read: self.read.clone(),
        }))
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
        PieceStore::remove(&mut self.mem, key)
    }
    fn drop_front(&mut self, key: &u64, to: u64) -> Result<Option<u64>, MemError> {
        self.mem.drop_front(key, to)
    }
}

/// A chain's resume reads only the track and clock definitions of its closed pieces: the block of a 60 000-byte
/// schema a track names stays unread, and the track keeps its number.
#[test]
fn a_chain_resume_reads_no_other_definition() {
    let body = vec![b's'; 60_000];
    let hash = *blake3::hash(&body).as_bytes();
    let schema = Describe::Schema(Schema {
        hash: RefVec::Slice { slice: &hash },
        media_type: "text/plain",
        bytes: RefVec::Slice { slice: &body },
    });
    let Describe::Track(t) = track(2, 1) else {
        unreachable!("track() gives a track")
    };
    let foreign = Describe::Track(TrackDef {
        items: Items::Foreign(Foreign {
            name: "com.example.big",
            media_type: "",
            version: "1",
            schema: RefVec::Slice { slice: &hash },
        }),
        ..t
    });
    let mut defs = Definitions::new();
    hold(&mut defs, &[clock(1), track(1, 1), schema, foreign.clone()]).unwrap();
    let read = Arc::new(AtomicU64::new(0));
    let store = Counted {
        mem: MemPieces::new(),
        read: read.clone(),
    };
    let mut c = PieceChain::create_on(store, OWNER, 0, &[], options(PieceTapes::Each)).unwrap();
    chunks(&mut c, &defs, 0, 2);
    let id = c.writer().unwrap().next_id();
    c.write_block(&defs, &on_tracks(id, &[(2, 5, b"x")]))
        .unwrap();
    // track 2 again, relabeled: its newest definition in a block without the schema
    let Describe::Track(t) = &foreign else {
        unreachable!("a track")
    };
    let relabeled = TrackDef {
        label: "again",
        ..t.clone()
    };
    assert_eq!(defs.define_track(relabeled.spec()), Ok(TrackNum::new(2)));
    let id = c.writer().unwrap().next_id();
    c.write_block(&defs, &on_tracks(id, &[(2, 6, b"y")]))
        .unwrap();
    c.cut_now(&defs).unwrap();
    let mem = c.store().mem.clone();
    drop(c);
    read.store(0, Ordering::Relaxed);
    let (_, resumed) = PieceChain::resume_on(
        Counted {
            mem,
            read: read.clone(),
        },
        OWNER,
        0,
        options(PieceTapes::Each),
    )
    .unwrap();
    let got = read.load(Ordering::Relaxed);
    assert!(got < 60_000, "{got} bytes read from the closed piece");
    let Describe::Track(foreign) = foreign else {
        unreachable!("a track")
    };
    assert_eq!(
        resumed.numbers.track(foreign.id(), foreign.owner),
        Some(TrackNum::new(2))
    );
}
