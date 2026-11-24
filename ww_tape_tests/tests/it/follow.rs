//! Readers of a chain while it is written (READ-6): the mux's progress watch in the writer's process; a follower in
//! another process across cuts, a crash and a resume, which hands out only blocks that check; a track's items
//! decoded as they land; two chains merged by time.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use exact_units::Count;
use ww_tape::chunk::{ChunkHead, ColumnSpec};
use ww_tape::index::IndexPolicy;
use ww_tape::payloads::EncodedPayloads;
use ww_tape::times::EncodedTimes;
use ww_tape::{
    BlockKind, ChainOptions, Cut, CutPolicy, Describe, FileBlocks, ItemNum, Layout, OwnerId,
    PieceStore, PieceTapes, SyncPolicy, TrackId, WriterOptions,
};
use ww_tape_fs::follow::{
    ChainFollower, FollowOptions, FollowStart, Followed, TimeMerge, TrackItem, TrackItems,
};
use ww_tape_mux::{ChunkSender, MuxOptions, TapeMux};

use crate::definitions::base;
use crate::pieces::{chunk, chunk_id, scratch};

pub(crate) const OWNER: OwnerId = OwnerId([0x5f; 32]);
/// Tracks the tests write, `t` of [chunk].
const TRACKS: u64 = 3;
/// The directory [follow_child] writes, when the test binary runs as one.
const CHILD_DIR_ENV: &str = "WW_TAPE_FOLLOW_DIR";
/// `create` or `resume`.
const CHILD_OP_ENV: &str = "WW_TAPE_FOLLOW_OP";
/// A wait a test makes only to read on; never what decides it.
const POLL: Duration = Duration::from_millis(20);

/// A chain cut every `chunks` chunk blocks, with no sync and no checkpoint by time.
pub(crate) fn options(chunks: u64) -> ChainOptions {
    ChainOptions {
        cut: CutPolicy {
            chunks: Count::new(chunks),
            ..CutPolicy::default()
        },
        writer: WriterOptions {
            sync: SyncPolicy::None,
            index: IndexPolicy {
                interval: None,
                ..IndexPolicy::default()
            },
            ..WriterOptions::default()
        },
        tapes: PieceTapes::Each,
    }
}

/// Define [base]'s clock and tracks on `mux`; a sender for each of the first [TRACKS] tracks.
pub(crate) fn define<T>(mux: &TapeMux<T>) -> Vec<ChunkSender<T>>
where
    T: PieceStore + Send + 'static,
    T::Sink: Send,
    T::Key: Send,
    T::Error: Send + Sync + std::fmt::Debug,
{
    for d in &base(Layout::UNALIGNED)[2..] {
        match d {
            Describe::Clock(c) => assert_eq!(mux.define_clock(c.spec()).unwrap(), c.number),
            Describe::Track(t) => assert_eq!(mux.define_track(t.spec()).unwrap(), t.number),
            _ => unreachable!("base holds clocks and tracks after its first two"),
        }
    }
    (0..TRACKS)
        .map(|t| mux.chunks(ww_tape::TrackNum::new(t as u32 + 1)).unwrap())
        .collect()
}

/// Chunk `k` of track `t` with the items of [chunk], times encoded as a reader decodes them, each item's number
/// as its payload.
pub(crate) fn events_chunk(t: u64, k: u64) -> (ChunkHead, Vec<(ColumnSpec, Vec<u8>)>) {
    let (head, _) = chunk(t, k);
    let first = head.first_item.get();
    let starts: Vec<i64> = (first..first + 50)
        .map(|i| i as i64 * 10 + t as i64)
        .collect();
    let payloads: Vec<[u8; 8]> = (first..first + 50).map(u64::to_le_bytes).collect();
    let times = EncodedTimes::encode(&starts, None, None).unwrap();
    let payloads = EncodedPayloads::encode(payloads.iter().map(|p| &p[..])).unwrap();
    let columns = times
        .columns()
        .chain(payloads.columns())
        .map(|(s, b)| (s, b.to_vec()))
        .collect();
    let head = times
        .head
        .chunk_head(head.track_id, head.track_number, head.first_item);
    (head, columns)
}

/// Send chunk `k` of track `t` ([events_chunk]).
pub(crate) fn send<T: PieceStore>(s: &ChunkSender<T>, t: u64, k: u64)
where
    T::Error: std::fmt::Debug,
{
    let (head, columns) = events_chunk(t, k);
    s.send(head, columns).unwrap();
}

/// Not a test of its own: the body of the writer process [Writer] starts. It creates or resumes a chain in
/// [CHILD_DIR_ENV], cut every 4 chunks, and takes commands, one per line on stdin, answering `done` on stdout:
///
/// - `send K0 K1 K2 N`: chunks `K_t .. K_t + N` of each track `t`, a round of the three at a time, then a flush;
/// - `stream`: chunks from 0 on, a flush after each round, until killed (no answer);
/// - `tear`: a flush, then the last block of the newest piece again, its last 10 bytes left out, as a writer killed
///   mid-write leaves it;
/// - `close`: close the chain and end.
#[test]
fn follow_child() {
    let Some(dir) = std::env::var_os(CHILD_DIR_ENV) else {
        return;
    };
    let dir = PathBuf::from(dir);
    let mux = match std::env::var(CHILD_OP_ENV).as_deref() {
        Ok("resume") => {
            ww_tape_fs::mux::resume(&dir, OWNER, 0, options(4), MuxOptions::default())
                .unwrap()
                .0
        }
        _ => {
            ww_tape_fs::mux::create(&dir, OWNER, 0, &[], options(4), MuxOptions::default()).unwrap()
        }
    };
    let senders = define(&mux);
    for line in std::io::stdin().lines() {
        let line = line.unwrap();
        let words: Vec<&str> = line.split_whitespace().collect();
        match words.as_slice() {
            ["send", from @ .., n] => {
                let from: Vec<u64> = from.iter().map(|w| w.parse().unwrap()).collect();
                for r in 0..n.parse::<u64>().unwrap() {
                    for (t, s) in senders.iter().enumerate() {
                        send(s, t as u64, from[t] + r);
                    }
                }
                mux.flush().unwrap();
            }
            ["stream"] => {
                for k in 0..=u64::MAX {
                    for (t, s) in senders.iter().enumerate() {
                        send(s, t as u64, k);
                    }
                    mux.flush().unwrap();
                }
            }
            ["tear"] => {
                mux.flush().unwrap();
                let path = newest_piece(&dir);
                let bytes = std::fs::read(&path).unwrap();
                let (offset, len) = walk(&bytes).last().map(|(o, b)| (*o, b.len())).unwrap();
                let mut f = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap();
                f.write_all(&bytes[offset as usize..offset as usize + len - 10])
                    .unwrap();
            }
            ["close"] => {
                drop(senders);
                mux.close().unwrap();
                println!("follow-child: done");
                return;
            }
            _ => panic!("no such command: {line}"),
        }
        println!("follow-child: done");
    }
}

/// The newest piece file in `dir`.
fn newest_piece(dir: &Path) -> PathBuf {
    let mut names: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    names.sort();
    names.pop().unwrap()
}

/// The blocks of `bytes` a Prefix walk reads, with their offsets.
pub(crate) fn walk(bytes: &[u8]) -> Vec<(u64, Vec<u8>)> {
    let mut out = Vec::new();
    let mut w = FileBlocks::new(bytes);
    while let Some(b) = w.next_block() {
        if let Ok(b) = b {
            let bytes = b.bytes().to_vec();
            out.push((w.offset(), bytes));
        }
    }
    out
}

/// The blocks of every piece file of `dir`, as fresh walks read them once the writer is done: (piece, offset, bytes).
fn walk_chain(dir: &Path) -> Vec<(u64, u64, Vec<u8>)> {
    (0..)
        .map_while(|seq| {
            std::fs::read(dir.join(ww_tape_fs::piece_name(seq)))
                .ok()
                .map(|b| (seq, b))
        })
        .flat_map(|(seq, bytes)| walk(&bytes).into_iter().map(move |(o, b)| (seq, o, b)))
        .collect()
}

/// A writer process ([follow_child]).
struct Writer {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Writer {
    fn start(dir: &Path, op: &str) -> Self {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["follow::follow_child", "--exact", "--nocapture"])
            .env_clear()
            .env(CHILD_DIR_ENV, dir)
            .env(CHILD_OP_ENV, op)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin,
            stdout,
        }
    }

    /// Send `command` and wait for its answer.
    fn run(&mut self, command: &str) {
        self.send(command);
        let mut line = String::new();
        while !line.contains("follow-child: done") {
            line.clear();
            assert_ne!(
                self.stdout.read_line(&mut line).unwrap(),
                0,
                "the writer ended at {command}"
            );
        }
    }

    fn send(&mut self, command: &str) {
        writeln!(self.stdin, "{command}").unwrap();
        self.stdin.flush().unwrap();
    }

    /// Kill the writer at once, as a crash does (SIGKILL: no core dump).
    fn kill(mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }

    fn wait(mut self) {
        drop(self.stdin);
        assert!(self.child.wait().unwrap().success());
    }
}

/// What a test saw a follower hand out.
#[derive(Default)]
struct Seen {
    blocks: Vec<Followed>,
    /// Per track, the (first item, count) of each chunk, in the order handed out.
    chunks: BTreeMap<TrackId, Vec<(u64, u64)>>,
}

impl Seen {
    fn take(&mut self, f: Followed) {
        if let Some(c) = f.block.block().chunk() {
            let h = c.head();
            self.chunks
                .entry(h.track_id)
                .or_default()
                .push((h.first_item.get(), u64::from(h.count)));
        }
        self.blocks.push(f);
    }

    /// Take every block there now.
    fn drain(&mut self, f: &mut ChainFollower) {
        while let Some(b) = f.try_next().unwrap() {
            self.take(b);
        }
    }

    /// The item after the last chunk of track `t`, each chunk right after the one before from item 0.
    fn end_of(&self, t: u64) -> u64 {
        let mut end = 0;
        for (first, count) in self.chunks.get(&chunk_id(t)).into_iter().flatten() {
            assert_eq!(*first, end, "track {t}: {:?}", self.chunks);
            end += count;
        }
        end
    }

    /// What was handed out is what fresh walks of the finished files read, block for block.
    fn is_chain_of(&self, dir: &Path) {
        let seen: Vec<(u64, u64, &[u8])> = self
            .blocks
            .iter()
            .map(|f| (f.piece, f.offset, f.block.bytes()))
            .collect();
        let files = walk_chain(dir);
        let files: Vec<(u64, u64, &[u8])> = files
            .iter()
            .map(|(p, o, b)| (*p, *o, b.as_slice()))
            .collect();
        assert_eq!(
            seen.len(),
            files.len(),
            "blocks handed out and in the files"
        );
        assert!(
            seen == files,
            "the blocks handed out are those of the files"
        );
    }
}

/// A follower in another process than the writer reads the chain while it is written: across cuts, at a torn end
/// a crash left (handed out never, read again once a resume cut it back and wrote on), and after the resume to the
/// close. Each step waits for the writer's answer, so the follower reads exactly what the step wrote.
#[test]
fn a_follower_reads_a_chain_another_process_writes_across_a_crash_and_a_resume() {
    let dir = scratch("follow-crash");
    let mut f = ChainFollower::new(&dir, FollowOptions::default()).unwrap();
    let mut seen = Seen::default();
    assert!(f.try_next().unwrap().is_none(), "nothing there yet");

    let mut w = Writer::start(&dir, "create");
    w.run("send 0 0 0 5");
    seen.drain(&mut f);
    assert!(
        f.piece().unwrap() >= 2,
        "cut every 4 chunks: 15 chunks span pieces"
    );
    assert_eq!(
        (0..TRACKS).map(|t| seen.end_of(t)).collect::<Vec<_>>(),
        [250; 3]
    );
    assert_eq!(f.stopped(), None);

    let before = seen.blocks.len();
    w.run("tear");
    seen.drain(&mut f);
    assert_eq!(
        seen.blocks.len(),
        before,
        "a torn block is never handed out"
    );
    assert!(matches!(f.stopped(), Some(Cut::Truncated { .. })));
    w.kill();
    seen.drain(&mut f);
    assert_eq!(seen.blocks.len(), before);

    let mut w = Writer::start(&dir, "resume");
    w.run("send 5 5 5 5");
    seen.drain(&mut f);
    assert_eq!(
        (0..TRACKS).map(|t| seen.end_of(t)).collect::<Vec<_>>(),
        [500; 3]
    );
    w.run("close");
    w.wait();
    seen.drain(&mut f);
    assert!(f.is_closed());
    assert_eq!(f.stopped(), None);
    seen.is_chain_of(&dir);
    let defs = f.definitions();
    assert_eq!(
        defs.tracks().count(),
        TRACKS as usize,
        "the tracks written on"
    );
}

/// A writer killed at no chosen byte while it streams: the follower hands out the valid prefix, the resume goes on
/// from it, and the follower reads on to the close with no item lost or twice.
#[test]
fn a_follower_reads_on_after_a_writer_killed_anywhere() {
    let dir = scratch("follow-kill");
    let mut f = ChainFollower::new(&dir, FollowOptions::default()).unwrap();
    let mut seen = Seen::default();
    let mut w = Writer::start(&dir, "stream");
    w.send("stream");
    while seen.chunks.values().map(Vec::len).sum::<usize>() < 40 {
        if let Some(b) = f.wait_next_for(POLL).unwrap() {
            seen.take(b);
        }
    }
    w.kill();
    seen.drain(&mut f);

    let from: Vec<u64> = (0..TRACKS).map(|t| seen.end_of(t) / 50).collect();
    let mut w = Writer::start(&dir, "resume");
    w.run(&format!("send {} {} {} 3", from[0], from[1], from[2]));
    w.run("close");
    w.wait();
    seen.drain(&mut f);
    assert!(f.is_closed());
    for (t, k) in from.iter().enumerate() {
        assert_eq!(seen.end_of(t as u64), (k + 3) * 50);
    }
    seen.is_chain_of(&dir);
}

/// A writer crashed at any byte, a cut's included: the follower hands out what the crash left, the resume writes on
/// from what it keeps, and the follower reads on to the close, every block once and none the resume took back.
///
/// The crash points are each block's start and end in every piece of a finished chain, and bytes inside each block;
/// each is a prefix of that chain, as the writer leaves it when killed there.
#[test]
fn a_follower_reads_on_after_a_crash_at_any_byte() {
    let whole = scratch("follow-any-whole");
    let mux =
        ww_tape_fs::mux::create(&whole, OWNER, 0, &[], options(4), MuxOptions::default()).unwrap();
    let senders = define(&mux);
    for k in 0..3 {
        for (t, s) in senders.iter().enumerate() {
            send(s, t as u64, k);
        }
    }
    drop(senders);
    mux.close().unwrap();
    // written on after its close: an append head and a second close in the last piece
    let (mux, _) =
        ww_tape_fs::mux::resume(&whole, OWNER, 0, options(4), MuxOptions::default()).unwrap();
    let senders = define(&mux);
    for (t, s) in senders.iter().enumerate() {
        send(s, t as u64, 3);
    }
    drop(senders);
    mux.close().unwrap();
    let pieces: Vec<Vec<u8>> = (0..)
        .map_while(|seq| std::fs::read(whole.join(ww_tape_fs::piece_name(seq))).ok())
        .collect();
    assert!(
        pieces.len() >= 3,
        "a cut every 4 chunks: 9 chunks span pieces"
    );

    let mut points = Vec::new();
    for (seq, bytes) in pieces.iter().enumerate() {
        points.push((seq, 0));
        for (offset, block) in walk(bytes) {
            let len = block.len() as u64;
            for at in [
                offset,
                offset + 1,
                offset + len / 2,
                offset + len - 1,
                offset + len,
            ] {
                points.push((seq, at as usize));
            }
        }
    }
    points.sort_unstable();
    points.dedup();
    let dir = scratch("follow-any");
    for (seq, at) in points {
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        for (s, bytes) in pieces.iter().enumerate().take(seq + 1) {
            let end = if s == seq { at } else { bytes.len() };
            std::fs::write(dir.join(ww_tape_fs::piece_name(s as u64)), &bytes[..end]).unwrap();
        }
        let mut f = ChainFollower::new(&dir, FollowOptions::default()).unwrap();
        let mut seen = Seen::default();
        seen.drain(&mut f);
        let from: Vec<u64> = (0..TRACKS).map(|t| seen.end_of(t) / 50).collect();
        let Ok((mux, _)) =
            ww_tape_fs::mux::resume(&dir, OWNER, 0, options(4), MuxOptions::default())
        else {
            // a crash inside the first piece's head leaves nothing to resume
            assert_eq!((seq, seen.blocks.len()), (0, 0), "crash at {seq}:{at}");
            continue;
        };
        let senders = define(&mux);
        for r in 0..2 {
            for (t, s) in senders.iter().enumerate() {
                send(s, t as u64, from[t] + r);
            }
        }
        drop(senders);
        mux.close().unwrap();
        seen.drain(&mut f);
        assert!(
            f.is_closed(),
            "crash at {seq}:{at}: the follower reads to the close, stopped {:?} in piece {:?}",
            f.stopped(),
            f.piece()
        );
        for (t, k) in from.iter().enumerate() {
            assert_eq!(seen.end_of(t as u64), (k + 2) * 50, "crash at {seq}:{at}");
        }
        seen.is_chain_of(&dir);
    }
}

/// A follower waiting on another thread wakes as the writer writes, and its wait ends at the close.
#[test]
fn a_waiting_follower_wakes_for_each_write_and_ends_at_the_close() {
    let dir = scratch("follow-wake");
    let mut f = ChainFollower::new(&dir, FollowOptions::default()).unwrap();
    let reader = std::thread::spawn(move || {
        let mut seen = Seen::default();
        while let Some(b) = f.wait_next().unwrap() {
            seen.take(b);
        }
        seen
    });
    let mux =
        ww_tape_fs::mux::create(&dir, OWNER, 0, &[], options(4), MuxOptions::default()).unwrap();
    let senders = define(&mux);
    for k in 0..6 {
        for (t, s) in senders.iter().enumerate() {
            send(s, t as u64, k);
        }
        mux.flush().unwrap();
    }
    drop(senders);
    mux.close().unwrap();
    let seen = reader.join().unwrap();
    assert_eq!(
        (0..TRACKS).map(|t| seen.end_of(t)).collect::<Vec<_>>(),
        [300; 3]
    );
    seen.is_chain_of(&dir);
}

/// A follower started at the end hands out only what is written later, with the definitions already held; one
/// started at the newest piece begins with that piece's head.
#[test]
fn a_follower_starts_at_the_end_or_the_newest_piece() {
    let dir = scratch("follow-start");
    let mux =
        ww_tape_fs::mux::create(&dir, OWNER, 0, &[], options(4), MuxOptions::default()).unwrap();
    let senders = define(&mux);
    for k in 0..3 {
        for (t, s) in senders.iter().enumerate() {
            send(s, t as u64, k);
        }
    }
    mux.flush().unwrap();

    let at_end = FollowOptions {
        start: FollowStart::End,
        ..FollowOptions::default()
    };
    let mut end = ChainFollower::new(&dir, at_end).unwrap();
    assert!(end.try_next().unwrap().is_none());
    assert_eq!(end.definitions().tracks().count(), TRACKS as usize);
    let newest = FollowOptions {
        start: FollowStart::Newest,
        ..FollowOptions::default()
    };
    let mut new = ChainFollower::new(&dir, newest).unwrap();
    let first = new.try_next().unwrap().unwrap();
    assert_eq!(first.block.block().kind, BlockKind::PieceHead);
    assert!(first.piece >= 2);

    send(&senders[0], 0, 3);
    mux.flush().unwrap();
    let mut seen = Seen::default();
    seen.drain(&mut end);
    assert_eq!(&seen.chunks[&chunk_id(0)], &[(150, 50)]);
    assert_eq!(seen.chunks.len(), 1);
    drop(senders);
    mux.close().unwrap();
}

/// A track follower gives the track's events decoded, in item order, and a jump in item numbers as items lost.
#[test]
fn a_track_follower_gives_events_and_lost_items() {
    let dir = scratch("follow-track");
    let mux =
        ww_tape_fs::mux::create(&dir, OWNER, 0, &[], options(0), MuxOptions::default()).unwrap();
    let senders = define(&mux);
    for k in [0, 1, 3] {
        send(&senders[1], 1, k);
        send(&senders[0], 0, k);
    }
    drop(senders);
    mux.close().unwrap();

    let chain = ChainFollower::new(&dir, FollowOptions::default()).unwrap();
    let mut track = chain.track(chunk_id(1));
    let mut items = Vec::new();
    while let Some(i) = track.wait_next().unwrap() {
        items.push(i);
    }
    let lost = TrackItem::Lost {
        first: ItemNum::new(100),
        count: 50,
    };
    assert_eq!(items.len(), 151);
    assert_eq!(items[100], lost);
    for (n, i) in items.iter().filter(|i| **i != lost).enumerate() {
        let item = if n < 100 { n } else { n + 50 } as u64;
        let start = item as i64 * 10 + 1;
        let want = TrackItem::Event {
            item: ItemNum::new(item),
            start,
            end: start,
            flags: 0,
            payload: item.to_le_bytes().to_vec(),
        };
        assert_eq!(*i, want);
    }
}

/// In a chain two owners write, a track follower of one owner passes over the chunks of the other owner's track of
/// the same id; one of the piece's own owner takes the tracks defined with an owner of all zeros; one of any owner
/// takes chunks of both.
#[test]
fn a_track_follower_of_an_owner_skips_another_owners_chunks() {
    const RUN: OwnerId = OwnerId([0xa1; 32]);
    let dir = scratch("follow-owner");
    let mux =
        ww_tape_fs::mux::create(&dir, OWNER, 0, &[], options(2), MuxOptions::default()).unwrap();
    let senders = define(&mux);
    let Describe::Track(t) = crate::definitions::track(2, crate::definitions::CLOCK) else {
        unreachable!("track() gives a track")
    };
    let mut spec = t.spec();
    spec.owner = RUN.0;
    let run_track = mux.define_track(spec).unwrap();
    assert_ne!(
        run_track, t.number,
        "another owner's track of the same id has a number of its own"
    );
    let run = mux.chunks(run_track).unwrap();
    for k in 0..4 {
        send(&senders[1], 1, k);
        let (mut head, columns) = events_chunk(1, k + 10);
        head.track_number = run_track;
        run.send(head, columns).unwrap();
    }
    drop((senders, run));
    mux.close().unwrap();

    let firsts = |mut track: ww_tape_fs::follow::TrackFollower| {
        let mut out = Vec::new();
        while let Some(i) = track.wait_next().unwrap() {
            if let TrackItem::Event { item, .. } = i {
                out.push(item.get());
            }
        }
        out
    };
    let follow = || ChainFollower::new(&dir, FollowOptions::default()).unwrap();
    let own = firsts(follow().track_of(chunk_id(1), OWNER));
    assert_eq!(own, (0..200).collect::<Vec<u64>>());
    let theirs = firsts(follow().track_of(chunk_id(1), RUN));
    assert_eq!(theirs, (500..700).collect::<Vec<u64>>());
    let none = firsts(follow().track_of(chunk_id(1), OwnerId([0xb2; 32])));
    assert!(none.is_empty(), "no chunk of a third owner");
    let any = firsts(follow().track(chunk_id(1)));
    // one numbering over both owners' chunks: those behind the items given are passed over
    assert!(
        any.contains(&0),
        "a follower of any owner takes the piece owner's chunks"
    );
    assert!(
        any.contains(&500),
        "a follower of any owner takes the other owner's chunks"
    );
}

/// A track follower of a series track gives its chunks' samples and flag runs as a reader of the closed file decodes
/// them; a chain of one file, aligned for its payloads.
#[test]
fn a_track_follower_gives_series_samples() {
    let dir = scratch("follow-series");
    let path = dir.join("series.wwt");
    crate::series::write(&path);
    let want = crate::series::read(&std::fs::read(&path).unwrap());

    let one = FollowOptions {
        tapes: PieceTapes::One,
        ..FollowOptions::default()
    };
    let chain = ChainFollower::new(&path, one).unwrap();
    let mut track = chain.track(crate::series::series_id());
    let mut got = Vec::new();
    while let Some(i) = track.wait_next().unwrap() {
        let TrackItem::Samples(s) = i else {
            panic!("a series track gives samples: {i:?}");
        };
        let runs = s.flag_runs().unwrap();
        let flags: Vec<u32> = (0..s.count).map(|i| runs.flags_at(i)).collect();
        got.push((s.first.get(), s.decode::<i32>().unwrap(), flags));
    }
    let want: Vec<(u64, Vec<i32>, Vec<u32>)> = want
        .chunks
        .into_iter()
        .zip(want.flags)
        .map(|((h, s), f)| (h.first_item.get(), s, f))
        .collect();
    assert!(
        want.iter().any(|(_, _, f)| f.iter().any(|w| *w != 0)),
        "flagged samples"
    );
    assert_eq!(got, want);
}

/// Two chains, one per writer, merged by time: every item once, in time order, and none before both chains show
/// they hold nothing older (the bound: the slower chain's newest time).
#[test]
fn two_chains_merge_by_time() {
    let dirs = [scratch("follow-merge-a"), scratch("follow-merge-b")];
    for (i, dir) in dirs.iter().enumerate() {
        let mux =
            ww_tape_fs::mux::create(dir, OWNER, 0, &[], options(4), MuxOptions::default()).unwrap();
        let senders = define(&mux);
        for k in 0..5 {
            send(&senders[i], i as u64, k);
        }
        drop(senders);
        mux.close().unwrap();
    }
    let now = Instant::now();
    let mut merge = TimeMerge::new(None);
    let mut tracks: Vec<_> = dirs
        .iter()
        .enumerate()
        .map(|(i, d)| {
            merge.add_source(now);
            ChainFollower::new(d, FollowOptions::default())
                .unwrap()
                .track(chunk_id(i as u64))
        })
        .collect();

    // chain a alone: nothing is given while b holds back the bound
    let first = tracks[0].wait_next().unwrap().unwrap();
    let TrackItem::Event { start, .. } = first else {
        panic!("{first:?}")
    };
    merge.push(0, i128::from(start), first, now);
    assert!(merge.pop(now).is_none());
    assert_eq!(merge.bound(now), None);

    let mut out = Vec::new();
    let mut open = [true, true];
    while open.iter().any(|o| *o) {
        for (s, t) in tracks.iter_mut().enumerate() {
            match t.try_next().unwrap() {
                Some(i @ TrackItem::Event { start, .. }) => {
                    merge.push(s, i128::from(start), i, now)
                }
                Some(i) => panic!("{i:?}"),
                None if t.chain().is_closed() && open[s] => {
                    open[s] = false;
                    merge.mark(s, i128::MAX, now);
                }
                None => {}
            }
        }
        while let Some(m) = merge.pop(now) {
            assert!(!m.late);
            out.push((m.source, m.time));
        }
    }
    let want: Vec<(usize, i128)> = (0..250)
        .flat_map(|i| [(0, i * 10), (1, i * 10 + 1)])
        .collect();
    assert_eq!(out, want);
    assert_eq!(merge.waiting(), 0);
}

/// [TrackItems] picks its track out of blocks a caller has, apart from a follower, from a tick on.
#[test]
fn track_items_pick_their_track_from_a_tick() {
    let dir = scratch("follow-items");
    let mux =
        ww_tape_fs::mux::create(&dir, OWNER, 0, &[], options(0), MuxOptions::default()).unwrap();
    let senders = define(&mux);
    for k in [0, 1] {
        send(&senders[2], 2, k);
        send(&senders[0], 0, k);
    }
    drop(senders);
    mux.close().unwrap();
    let mut f = ChainFollower::new(&dir, FollowOptions::default()).unwrap();
    let mut items = TrackItems::new(chunk_id(2)).from_tick(52);
    let mut out = VecDeque::new();
    while let Some(b) = f.try_next().unwrap() {
        items.take(&b.block, f.definitions(), &mut out).unwrap();
    }
    assert_eq!(items.next_item(), Some(ItemNum::new(100)));
    let starts: Vec<i64> = out
        .iter()
        .map(|i| match i {
            TrackItem::Event { start, .. } => *start,
            i => panic!("{i:?}"),
        })
        .collect();
    let want: Vec<i64> = (5..100).map(|i| i * 10 + 2).collect();
    assert_eq!(starts, want);
}
