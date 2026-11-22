//! A [TapeMux] resumed after a crash rebuilds the summary trees of the piece the crash tore: crashed anywhere and
//! given the items it lost, it writes the bytes of the run without the crash, in both layouts (WRITE-13).

use exact_units::Count;
use ww_tape::index::IndexPolicy;
use ww_tape::{
    BlockKind, ChainOptions, CutPolicy, ItemNum, Layout, MemTapes, OwnerId, PieceTapes, SeriesGrid,
    SyncPolicy, TapeReader, TrackId, TrackNum, WriterOptions,
};
use ww_tape_mux::{Event, EventsOptions, MuxOptions, SeriesOptions, TapeMux};

use crate::mux_handles::{clock, events_track, series_track};
use crate::mux_multi::{next_item, pieces, plant};
use crate::resume::cut_points;

const OWNER: OwnerId = OwnerId([0x52; 32]);
/// Ticks from one sample of the series to the next, as [series_track] declares.
const PERIOD: i64 = 1000;

/// What a run writes: `steps` steps, each `events` items of the events track and `samples` samples of the series,
/// in chunks of `chunk_items` and `chunk_samples`, the chain cut every `cut` chunks; the events with their tree when
/// `event_tree`.
#[derive(Copy, Clone, Debug)]
struct Run {
    aligned: bool,
    steps: u64,
    events: u64,
    samples: u64,
    chunk_items: u32,
    chunk_samples: u32,
    cut: u64,
    event_tree: bool,
}

impl Run {
    fn options(&self) -> ChainOptions {
        ChainOptions {
            cut: CutPolicy {
                chunks: Count::new(self.cut),
                ..CutPolicy::default()
            },
            writer: WriterOptions {
                sync: SyncPolicy::None,
                index: IndexPolicy {
                    interval: None,
                    ..IndexPolicy::default()
                },
                layout: if self.aligned {
                    Layout::ALIGNED
                } else {
                    Layout::UNALIGNED
                },
                ..WriterOptions::default()
            },
            tapes: PieceTapes::Each,
        }
    }
}

/// Item `i` of the events track: a span with flags.
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

/// Sample `i` of the series: two channels.
fn sample(i: u64) -> [i16; 2] {
    let x = (i * 37 % 4001) as i16 - 2000;
    [x, (i % 300) as i16]
}

/// The tracks of a run: the events track and the series, with their numbers.
struct Tracks {
    events: (TrackNum, TrackId),
    series: (TrackNum, TrackId),
}

/// Define the clock and the two tracks on `mux`, in the same order each time, so a resumed mux gives the numbers
/// back.
fn define(mux: &TapeMux<MemTapes>) -> Tracks {
    let c = mux.define_clock(clock()).unwrap();
    let e = events_track(1, c);
    let s = series_track(c);
    let ids = (e.key.id(), s.key.id());
    Tracks {
        events: (mux.define_track(e).unwrap(), ids.0),
        series: (mux.define_track(s).unwrap(), ids.1),
    }
}

/// Write `run` on `mux` from item `events_from` of the events track and sample `samples_from` of the series on,
/// then close it.
fn write(mux: TapeMux<MemTapes>, run: Run, t: &Tracks, events_from: u64, samples_from: u64) {
    let mut events = mux
        .events(
            t.events.0,
            EventsOptions {
                chunk_items: run.chunk_items,
                latency: None,
                summaries: run.event_tree,
                first_item: ItemNum::new(events_from),
                ..EventsOptions::default()
            },
        )
        .unwrap();
    let mut series = mux
        .series::<i16>(
            t.series.0,
            SeriesOptions {
                chunk_samples: run.chunk_samples,
                latency: None,
                first_item: ItemNum::new(samples_from),
                first_tick: samples_from as i64 * PERIOD,
                ..SeriesOptions::default()
            },
        )
        .unwrap();
    // the grid the run started with, from sample 0 on
    series
        .regrid(SeriesGrid::start(0, 0, exact_units::Magnitude::ONE))
        .unwrap();
    for step in 0..run.steps {
        let items: Vec<Event<'_>> = (step * run.events..(step + 1) * run.events)
            .filter(|i| *i >= events_from)
            .map(event)
            .collect();
        events.push_all(&items).unwrap();
        let samples: Vec<i16> = (step * run.samples..(step + 1) * run.samples)
            .filter(|i| *i >= samples_from)
            .flat_map(sample)
            .collect();
        series.push(&samples).unwrap();
    }
    events.finish().unwrap();
    series.finish().unwrap();
    mux.close().unwrap();
}

/// The pieces of `run` written whole.
fn whole_run(run: Run) -> Vec<Vec<u8>> {
    let mem = MemTapes::new();
    let mux = TapeMux::create(
        mem.clone(),
        OWNER,
        0,
        &[],
        run.options(),
        MuxOptions::default(),
    )
    .unwrap();
    let t = define(&mux);
    write(mux, run, &t, 0, 0);
    pieces(&mem)
}

/// Summary blocks of the events tree and of the series in `pieces` that lie before a chunk of their piece: written
/// while the piece went on, not only by the end of its trees.
fn summaries_mid_piece(pieces: &[Vec<u8>]) -> (usize, usize) {
    let (mut events, mut series) = (0, 0);
    for p in pieces {
        let r = TapeReader::open(&p[..]).unwrap();
        let entries = r.entries(0).unwrap();
        let last_chunk = entries
            .iter()
            .filter(|e| matches!(e.kind, BlockKind::Events | BlockKind::Series))
            .map(|e| e.offset)
            .max()
            .unwrap_or(0);
        for e in entries
            .iter()
            .filter(|e| e.kind == BlockKind::Summary && e.offset < last_chunk)
        {
            match e.tree {
                1 => events += 1,
                _ => series += 1,
            }
        }
    }
    (events, series)
}

/// The end of the last events chunk of `piece`.
fn last_events_end(piece: &[u8]) -> usize {
    let r = TapeReader::open(piece).unwrap();
    r.entries(0)
        .unwrap()
        .iter()
        .filter(|e| e.kind == BlockKind::Events)
        .map(|e| (e.offset + u64::from(e.len)) as usize)
        .max()
        .unwrap()
}

/// Crash `run` at many bytes: at every block boundary of every piece and at bytes in between. Each crash keeps the
/// pieces before and the torn piece up to the crash; a mux resumes the chain, is given every item after the last
/// chunk kept of each track, and closes. The chain holds the bytes of the run without the crash.
///
/// Without the events' tree, the crashes are those after which the piece gets another events chunk: a rebuilt tree
/// is dropped by it, as the mux built none.
fn crash_anywhere(run: Run, extra: usize) {
    let want = whole_run(run);
    assert!(want.len() > 2, "the run is cut into pieces: {}", want.len());
    let (events_mid, series_mid) = summaries_mid_piece(&want);
    assert!(
        (events_mid > 0) == run.event_tree && series_mid > 0,
        "the trees write blocks mid-piece: {events_mid} and {series_mid}"
    );
    let mut crashes = 0;
    for (p, piece) in want.iter().enumerate() {
        let (done, _) = want.split_at(p);
        let last_events = last_events_end(piece);
        for at in cut_points(piece, extra) {
            if (p + 1 == want.len() && at == piece.len()) || (!run.event_tree && at >= last_events)
            {
                continue;
            }
            let mem = plant(done, &piece[..at]);
            let (mux, resumed) =
                TapeMux::resume(mem.clone(), OWNER, 0, run.options(), MuxOptions::default())
                    .unwrap();
            let t = define(&mux);
            let events_from = next_item(done, &resumed.entries, t.events.1);
            let samples_from = next_item(done, &resumed.entries, t.series.1);
            write(mux, run, &t, events_from, samples_from);
            let got = pieces(&mem);
            assert!(
                got == want,
                "{run:?}: a crash in piece {p} at {at} of {}, kept {}: {} pieces of {}; piece lengths {:?}, want {:?}",
                piece.len(),
                resumed.kept_bytes,
                got.len(),
                want.len(),
                got.iter().map(Vec::len).collect::<Vec<_>>(),
                want.iter().map(Vec::len).collect::<Vec<_>>(),
            );
            crashes += 1;
        }
    }
    assert!(crashes > 100, "{crashes} crashes");
}

/// The runs of the tests: an events tree that fills a block mid-piece takes 2^11 items a node, so many items.
fn run(aligned: bool) -> Run {
    Run {
        aligned,
        steps: 18,
        events: 65_536,
        samples: 4096,
        chunk_items: 16_384,
        chunk_samples: 1024,
        cut: 48,
        event_tree: true,
    }
}

#[test]
fn a_mux_crashed_anywhere_resumes_to_the_bytes_of_the_whole_run() {
    crash_anywhere(run(false), 40);
}

#[test]
fn an_aligned_mux_crashed_anywhere_resumes_to_the_bytes_of_the_whole_run() {
    crash_anywhere(run(true), 40);
}

/// An events track written without its tree beside a series with summaries: the tree a resume rebuilds of the kept
/// events is dropped by the track's next chunk.
#[test]
fn a_track_written_without_its_tree_resumes_to_the_same_bytes() {
    for aligned in [false, true] {
        let run = Run {
            events: 4096,
            chunk_items: 1024,
            event_tree: false,
            ..run(aligned)
        };
        crash_anywhere(run, 20);
    }
}
