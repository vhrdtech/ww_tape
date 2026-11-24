//! A summarized chunk sender (MUX-4): a caller that encodes its own chunks and sends their summary leaves gets the
//! summary tree an events handle writes, the same bytes, also after a crash at every block boundary and a resume;
//! a series' sample leaves give the tree a grid handle writes.

use exact_units::Count;
use ww_tape::chunk::{ChunkHead, ColumnKind, ColumnSpec};
use ww_tape::index::IndexPolicy;
use ww_tape::payloads::EncodedPayloads;
use ww_tape::samples::{encode, max_encoded_len};
use ww_tape::times::EncodedTimes;
use ww_tape::{
    BlockKind, ChainOptions, Codec, CutPolicy, EndMode, ItemNum, MemPieces, OwnerId, PieceTapes,
    SeriesGrid, SyncPolicy, TileCoords, TrackId, TrackNum, WriterOptions,
};
use ww_tape_mux::{
    CallError, Event, EventsOptions, GridOptions, Leaves, MuxError, MuxOptions, TapeMux,
};

use crate::mux_handles::{clock, events_track};
use crate::mux_multi::{next_item, pieces, plant};
use crate::resume::cut_points;

const OWNER: OwnerId = OwnerId([0x4c; 32]);
/// Items of the run: an events tree takes 2^11 items a node, so many items fill tree blocks mid-piece.
const ITEMS: u64 = 7 * 65_536;
const CHUNK_ITEMS: u64 = 16_384;
/// Chunks a piece holds: 6 steps of 4 chunks, then the last step in a piece of its own.
const CUT: u64 = 24;

/// Item `i`: a span with flags.
fn event(i: u64) -> Event<'static> {
    let start = i as i64 * 10;
    Event {
        start,
        end: start + (i % 7) as i64 * 3,
        flags: 1 << (i % 5),
        payload: &[],
        values: &[],
    }
}

fn options() -> ChainOptions {
    ChainOptions {
        cut: CutPolicy {
            chunks: Count::new(CUT),
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

/// Define the clock and the events track on `mux`, the same way each time; its number and id.
fn define(mux: &TapeMux<MemPieces>) -> (TrackNum, TrackId) {
    let c = mux.define_clock(clock()).unwrap();
    let t = events_track(1, c);
    let id = t.key.id();
    (mux.define_track(t).unwrap(), id)
}

/// The items from `from` on through an events handle, chunks of [CHUNK_ITEMS] from item 0 on, then the close.
fn by_handle(mux: TapeMux<MemPieces>, track: TrackNum, from: u64) {
    let mut events = mux
        .events(
            track,
            EventsOptions {
                chunk_items: CHUNK_ITEMS as u32,
                latency: None,
                first_item: ItemNum::new(from),
                ..EventsOptions::default()
            },
        )
        .unwrap();
    let items: Vec<Event<'_>> = (from..ITEMS).map(event).collect();
    for c in items.chunks(CHUNK_ITEMS as usize) {
        events.push_all(c).unwrap();
    }
    events.finish().unwrap();
    mux.close().unwrap();
}

/// The same items through a summarized chunk sender: each chunk encoded as an events handle encodes it, its leaves
/// from the sender's runs.
fn by_sender(mux: TapeMux<MemPieces>, track: (TrackNum, TrackId), from: u64) {
    let sender = mux.summarized_chunks(track.0).unwrap();
    let mut runs = sender.event_runs(ItemNum::new(from)).unwrap();
    let mut first = from;
    while first < ITEMS {
        let end = (first / CHUNK_ITEMS + 1) * CHUNK_ITEMS;
        let items: Vec<Event<'_>> = (first..end.min(ITEMS)).map(event).collect();
        let starts: Vec<i64> = items.iter().map(|e| e.start).collect();
        let ends: Vec<i64> = items.iter().map(|e| e.end).collect();
        let flags: Vec<u32> = items.iter().map(|e| e.flags).collect();
        let times = EncodedTimes::encode(&starts, Some(&ends), Some(&flags)).unwrap();
        let lens = vec![0u32; items.len()];
        let payloads = EncodedPayloads::from_parts(
            EventsOptions::default().payload_codec,
            &lens,
            &[],
            Vec::new(),
        )
        .unwrap();
        let head = times.head.chunk_head(track.1, track.0, ItemNum::new(first));
        let columns = times
            .columns()
            .map(|(s, b)| (s, b.to_vec()))
            .chain(payloads.into_columns())
            .collect();
        // --8<-- [start:send_leaves]
        for e in &items {
            runs.push(e.start, e.end, e.flags).unwrap();
        }
        let leaves = Leaves::events(&mut runs);
        let placing = sender.send_placed(head, columns, Some(leaves)).unwrap();
        // --8<-- [end:send_leaves]
        placing.expect("a waiting sender drops nothing");
        first = end;
    }
    sender.flush().unwrap();
    drop(sender);
    mux.close().unwrap();
}

/// The pieces of the whole run, through a handle or a sender.
fn whole(sender: bool) -> Vec<Vec<u8>> {
    let mem = MemPieces::new();
    let mux =
        TapeMux::create(mem.clone(), OWNER, 0, &[], options(), MuxOptions::default()).unwrap();
    let t = define(&mux);
    if sender {
        by_sender(mux, t, 0);
    } else {
        by_handle(mux, t.0, 0);
    }
    pieces(&mem)
}

#[test]
fn a_senders_leaves_give_the_bytes_of_an_events_handle() {
    let handle = whole(false);
    assert_eq!(handle.len(), 2, "a piece ended by a cut and the last");
    assert!(
        whole(true) == handle,
        "the sender's run is the handle's, summaries included"
    );
}

/// Crash the sender's run at every block boundary and at bytes between; resumed, a sender gives every item after
/// the last chunk kept, and the chain is the handle's whole run.
#[test]
fn a_senders_tree_resumes_to_the_bytes_of_an_events_handle_after_a_crash_anywhere() {
    let want = whole(false);
    let mut crashes = 0;
    for (p, piece) in want.iter().enumerate() {
        let (done, _) = want.split_at(p);
        for at in cut_points(piece, 20) {
            if p + 1 == want.len() && at == piece.len() {
                continue;
            }
            let mem = plant(done, &piece[..at]);
            let (mux, resumed) =
                TapeMux::resume(mem.clone(), OWNER, 0, options(), MuxOptions::default()).unwrap();
            let t = define(&mux);
            let from = next_item(done, &resumed.entries, t.1);
            by_sender(mux, t, from);
            assert!(
                pieces(&mem) == want,
                "a crash in piece {p} at {at} of {}",
                piece.len()
            );
            crashes += 1;
        }
    }
    assert!(crashes > 40, "{crashes} crashes");
}

/// Samples of the series run: [SAMPLES] samples of two channels, chunks of [CHUNK_STEPS] time steps.
const SAMPLES: u64 = 7 * 4096;
const CHUNK_STEPS: u64 = 1024;

/// The series track of the run, defined on `mux` the same way each time: its number, id, shape and period.
fn series_define(
    mux: &TapeMux<MemPieces>,
) -> (
    TrackNum,
    TrackId,
    ww_tape::samples::SampleShape,
    ww_tape::Ratio64,
) {
    let c = mux.define_clock(clock()).unwrap();
    let spec = crate::mux_handles::series_track(c);
    let id = spec.key.id();
    let shape = spec
        .clone()
        .numbered(TrackNum::new(1))
        .series_shape()
        .unwrap();
    let track = mux.define_track(spec).unwrap();
    let period = mux
        .definitions()
        .unwrap()
        .track(track)
        .unwrap()
        .period
        .unwrap();
    (track, id, shape, period)
}

/// The run's samples `from..to`.
fn series_samples(from: u64, to: u64) -> Vec<i16> {
    (from..to).flat_map(crate::mux_resume::sample).collect()
}

/// The run's grid.
fn series_grid() -> SeriesGrid {
    SeriesGrid::start(0, 0, exact_units::Magnitude::ONE)
}

/// The series run from time step `from` on through a summarized sender whose chunks hold the raw samples and whose
/// leaves come from the sender's sample leaves, then the close.
fn series_by_sender(mux: TapeMux<MemPieces>, from: u64) {
    let (track, id, shape, period) = series_define(&mux);
    let grid = series_grid();
    let s = mux.summarized_chunks(track).unwrap();
    for k in from / CHUNK_STEPS..SAMPLES / CHUNK_STEPS {
        let first = ItemNum::new(k * CHUNK_STEPS);
        let values = series_samples(k * CHUNK_STEPS, (k + 1) * CHUNK_STEPS);
        let mut bytes = vec![0; max_encoded_len(Codec::RAW, shape, values.len()).unwrap()];
        let len = encode(Codec::RAW, shape, &values, &mut bytes).unwrap();
        bytes.truncate(len);
        let (first_tick, last_tick) = grid.chunk_ticks(period, first, CHUNK_STEPS as u32).unwrap();
        let head = ChunkHead {
            kind: BlockKind::Series,
            track_id: id,
            track_number: track,
            first_item: first,
            count: CHUNK_STEPS as u32,
            first_tick,
            last_tick,
            latest_end: last_tick,
            time_mode: 0,
            end_mode: EndMode::Absent,
            grid: Some(grid),
            tile: TileCoords::NONE,
            level: 0,
        };
        let column = ColumnSpec {
            kind: ColumnKind::Samples,
            codec: Codec::RAW,
            param: 0,
            len: bytes.len(),
        };
        let mut leaves = s.sample_leaves::<i16>(first, grid).unwrap().unwrap();
        leaves.push(&values, 0).unwrap();
        let leaves = Leaves::series(leaves).unwrap();
        s.send_placed(head, vec![(column, bytes)], Some(leaves))
            .unwrap()
            .unwrap();
    }
    s.flush().unwrap();
    drop(s);
    mux.close().unwrap();
}

/// The series run through a grid handle, or through a summarized sender ([series_by_sender]); the pieces.
fn series_run(sender: bool) -> Vec<Vec<u8>> {
    let mem = MemPieces::new();
    let mux =
        TapeMux::create(mem.clone(), OWNER, 0, &[], options(), MuxOptions::default()).unwrap();
    if sender {
        series_by_sender(mux, 0);
        return pieces(&mem);
    }
    let (track, ..) = series_define(&mux);
    let mut g = mux
        .grid::<i16>(
            track,
            GridOptions {
                chunk_steps: CHUNK_STEPS as u32,
                latency: None,
                ..GridOptions::default()
            },
        )
        .unwrap();
    g.regrid(series_grid()).unwrap();
    g.push(&series_samples(0, SAMPLES)).unwrap();
    g.finish().unwrap();
    mux.close().unwrap();
    pieces(&mem)
}

#[test]
fn a_senders_sample_leaves_give_the_bytes_of_a_grid_handle() {
    let handle = series_run(false);
    assert!(
        series_run(true) == handle,
        "the sender's series is the handle's, summaries included"
    );
}

/// A summarized sender takes its chunks with leaves only; a sender without summaries takes none.
#[test]
fn a_senders_series_tree_resumes_to_the_bytes_of_a_grid_handle_after_a_crash_anywhere() {
    let want = series_run(false);
    let mut crashes = 0;
    for (p, piece) in want.iter().enumerate() {
        let (done, _) = want.split_at(p);
        for at in cut_points(piece, 20) {
            if p + 1 == want.len() && at == piece.len() {
                continue;
            }
            let mem = plant(done, &piece[..at]);
            let (mux, resumed) =
                TapeMux::resume(mem.clone(), OWNER, 0, options(), MuxOptions::default()).unwrap();
            let id = series_define(&mux).1;
            let from = next_item(done, &resumed.entries, id);
            series_by_sender(mux, from);
            assert!(
                pieces(&mem) == want,
                "a crash in piece {p} at {at} of {}",
                piece.len()
            );
            crashes += 1;
        }
    }
    assert!(crashes > 20, "{crashes} crashes");
}

#[test]
fn leaves_go_only_with_a_summarized_sender() {
    let mem = MemPieces::new();
    let mux =
        TapeMux::create(mem.clone(), OWNER, 0, &[], options(), MuxOptions::default()).unwrap();
    let t = define(&mux);
    let summarized = mux.summarized_chunks(t.0).unwrap();
    let times = EncodedTimes::encode(&[0], Some(&[0]), Some(&[0])).unwrap();
    let head = times.head.chunk_head(t.1, t.0, ItemNum::new(0));
    let columns = || -> Vec<_> { times.columns().map(|(s, b)| (s, b.to_vec())).collect() };
    let refused =
        |r: Result<_, MuxError<_>>| matches!(r, Err(MuxError::Call(CallError::WrongLeaves)));
    assert!(refused(summarized.send(head, columns()).map(|_| ())));
    assert!(refused(
        summarized.send_placed(head, columns(), None).map(|_| ())
    ));
    drop(summarized);
    let plain = mux.chunks(t.0).unwrap();
    assert!(plain.event_runs(ItemNum::new(0)).is_none());
    let mut runs = ww_tape_summary::EventRuns::new(
        ww_tape_summary::EventFields {
            flags: true,
            latest_end: true,
        },
        ItemNum::new(0),
    );
    runs.push(0, 0, 0).unwrap();
    let leaves = Leaves::events(&mut runs);
    assert!(refused(
        plain.send_placed(head, columns(), Some(leaves)).map(|_| ())
    ));
    drop(plain);
    mux.close().unwrap();
}

/// Leaves of fewer or more items than their chunk holds are refused at the call, before anything reaches the tree;
/// the next chunk with its own leaves goes on in the same tree.
#[test]
fn leaves_of_other_items_than_the_chunk_are_refused() {
    let mem = MemPieces::new();
    let mux =
        TapeMux::create(mem.clone(), OWNER, 0, &[], options(), MuxOptions::default()).unwrap();
    let t = define(&mux);
    let sender = mux.summarized_chunks(t.0).unwrap();
    let chunk = |first: u64, n: u64| {
        let items: Vec<Event<'_>> = (first..first + n).map(event).collect();
        let starts: Vec<i64> = items.iter().map(|e| e.start).collect();
        let ends: Vec<i64> = items.iter().map(|e| e.end).collect();
        let flags: Vec<u32> = items.iter().map(|e| e.flags).collect();
        let times = EncodedTimes::encode(&starts, Some(&ends), Some(&flags)).unwrap();
        let head = times.head.chunk_head(t.1, t.0, ItemNum::new(first));
        let columns: Vec<_> = times.columns().map(|(s, b)| (s, b.to_vec())).collect();
        (head, columns, items)
    };
    let leaves = |items: &[Event<'_>], first: u64| {
        let mut runs = sender.event_runs(ItemNum::new(first)).unwrap();
        for e in items {
            runs.push(e.start, e.end, e.flags).unwrap();
        }
        Leaves::events(&mut runs)
    };
    let refused =
        |r: Result<_, MuxError<_>>| matches!(r, Err(MuxError::Call(CallError::WrongLeaves)));
    let (head, columns, items) = chunk(0, 100);
    let fewer = leaves(&items[..50], 0);
    assert!(
        refused(
            sender
                .send_placed(head, columns.clone(), Some(fewer))
                .map(|_| ())
        ),
        "50 items of leaves for 100"
    );
    let (_, _, more) = chunk(0, 150);
    let more = leaves(&more, 0);
    assert!(
        refused(
            sender
                .send_placed(head, columns.clone(), Some(more))
                .map(|_| ())
        ),
        "150 items of leaves for 100"
    );
    let late = leaves(&items, 1);
    assert!(
        refused(
            sender
                .send_placed(head, columns.clone(), Some(late))
                .map(|_| ())
        ),
        "leaves from another first item"
    );
    let whole = leaves(&items, 0);
    sender
        .send_placed(head, columns, Some(whole))
        .unwrap()
        .unwrap()
        .wait()
        .unwrap();
    drop(sender);
    mux.close().unwrap();
}
