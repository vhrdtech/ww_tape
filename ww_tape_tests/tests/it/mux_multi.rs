//! Two [TapeMux] chains in one process, as a radio writes its samples to one chain and what it decodes to another:
//! producers on several threads write both at once, a crash leaves both open at any byte, and both resume and go on.

use std::thread;

use exact_units::{Count, Magnitude};
use shrink_wrap::prelude::*;
use ww_tape::index::{IndexEntry, IndexPolicy};
use ww_tape::piece::check_chain;
use ww_tape::samples::Samples;
use ww_tape::summary::{SeriesStats, SeriesSummarySpec};
use ww_tape::times::EventTimes;
use ww_tape::{
    BlockKind, ChainOptions, ClockNum, ClockOrigin, ClockSpec, ColumnKind, CutPolicy, Element,
    ElementDesc, EventsDef, ItemNum, Items, MemTapes, OwnerId, PieceTapes, Ratio64, SampleShape,
    SeriesDef, SyncPolicy, TapeReader, TapeSink, Tapes, TrackId, TrackKind, TrackNum, TrackRepeat,
    TrackSpec, WriterOptions, clock_key, track_key,
};
use ww_tape_mux::{EventsOptions, MuxOptions, SeriesOptions, TapeMux};

/// One process writes both chains: one owner, two chain numbers.
const OWNER: OwnerId = OwnerId([0x6d; 32]);
/// The chain of the samples.
const SAMPLES: u64 = 0;
/// The chain of the events.
const EVENTS: u64 = 1;
/// Series tracks of [SAMPLES], each written from a thread of its own.
const SERIES_TRACKS: u32 = 2;
/// Events tracks of [EVENTS], each written from a thread of its own.
const EVENTS_TRACKS: u32 = 3;
/// Ticks from one sample to the next.
const PERIOD: i64 = 1000;
/// Samples of a series chunk, and items of an events chunk.
const CHUNK_SAMPLES: u32 = 1000;
const CHUNK_ITEMS: u32 = 200;
/// Samples of each series track and items of each events track written before the crash.
const SAMPLES_BEFORE: u64 = 20_000;
const ITEMS_BEFORE: u64 = 3_000;

/// A chain cut every `chunks` chunk blocks, with no sync and no checkpoint by time.
fn options(chunks: u64) -> ChainOptions {
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

fn clock() -> ClockSpec<'static> {
    ClockSpec {
        key: clock_key(0x6d, &[], 1),
        rate: Magnitude::pow10(9).into(),
        resolution: Magnitude::pow10(-9).into(),
        origin: ClockOrigin::Boot,
        label: "front end",
    }
}

/// Series track `k` of [SAMPLES]: i16 on two channels, a sample per [PERIOD] ticks, with extremes and sums per 256.
fn series_track(k: u32, clock: ClockNum) -> TrackSpec<'static> {
    let mut def = SeriesDef::of::<i16>(2, Ratio64::int(PERIOD as u64).unwrap());
    def.summaries = Some(SeriesSummarySpec {
        base_shift: 8,
        stats: SeriesStats {
            min: true,
            max: true,
            sum: true,
            ..SeriesStats::default()
        },
        clip: None,
    });
    TrackSpec {
        key: track_key(
            0x6d,
            &[ww_tape::Segment::Name("iq"), ww_tape::Segment::Index(k)],
        ),
        clock,
        dimensions: RefVec::Slice { slice: &[] },
        items: Items::numbers::<i16>(None),
        label: "iq",
        repeat: TrackRepeat::Stream,
        kind: TrackKind::Series(def),
        relations: RefVec::Slice { slice: &[] },
        owner: [0; 32],
    }
}

/// Events track `k` of [EVENTS]: instants with payloads.
fn events_track(k: u32, clock: ClockNum) -> TrackSpec<'static> {
    TrackSpec {
        key: track_key(
            0x6d,
            &[
                ww_tape::Segment::Name("decoded"),
                ww_tape::Segment::Index(k),
            ],
        ),
        clock,
        dimensions: RefVec::Slice { slice: &[] },
        items: Items::RawBytes,
        label: "decoded",
        repeat: TrackRepeat::Stream,
        kind: TrackKind::Events(EventsDef::INSTANTS),
        relations: RefVec::Slice { slice: &[] },
        owner: [0; 32],
    }
}

/// Sample `i` of series track `k`, both channels.
fn sample(k: u32, i: u64) -> [i16; 2] {
    let v = |c: u64| ((i * 31 + c * 7 + u64::from(k) * 1000) % 4001) as i16 - 2000;
    [v(0), v(1)]
}

/// The start of item `i` of events track `k`.
fn start(k: u32, i: u64) -> i64 {
    i as i64 * 10 + i64::from(k)
}

/// The tracks of both chains, defined on their muxes: the numbers the writer gave, and the ids.
struct Tracks {
    series: Vec<(TrackNum, TrackId)>,
    events: Vec<(TrackNum, TrackId)>,
}

fn define<T>(samples: &TapeMux<T>, events: &TapeMux<T>) -> Tracks
where
    T: Tapes + Send + 'static,
    T::Sink: Send,
    T::Key: Send,
    T::Error: Send + Sync + std::fmt::Debug,
{
    let define_all = |mux: &TapeMux<T>, n: u32, spec: fn(u32, ClockNum) -> TrackSpec<'static>| {
        let c = mux.define_clock(clock()).unwrap();
        (0..n)
            .map(|k| {
                let number = mux.define_track(spec(k, c)).unwrap();
                (number, spec(k, c).numbered(number).id())
            })
            .collect()
    };
    Tracks {
        series: define_all(samples, SERIES_TRACKS, series_track),
        events: define_all(events, EVENTS_TRACKS, events_track),
    }
}

/// Write samples `series.0..series.1` of every series track and items `events.0..events.1` of every events track,
/// each track from a thread of its own, both muxes at once. Each handle is dropped at the end: its open chunk is
/// sent.
fn write<T>(
    muxes: (&TapeMux<T>, &TapeMux<T>),
    tracks: &Tracks,
    series: &[(u64, u64)],
    events: &[(u64, u64)],
) where
    T: Tapes + Send + Sync + 'static,
    T::Sink: Send,
    T::Key: Send,
    T::Error: Send + Sync + std::fmt::Debug,
{
    thread::scope(|s| {
        for (k, ((number, _), (from, to))) in tracks.series.iter().zip(series).enumerate() {
            let mut h = muxes
                .0
                .series::<i16>(
                    *number,
                    SeriesOptions {
                        chunk_samples: CHUNK_SAMPLES,
                        first_item: ItemNum::new(*from),
                        first_tick: *from as i64 * PERIOD,
                        ..SeriesOptions::default()
                    },
                )
                .unwrap();
            let (from, to) = (*from, *to);
            s.spawn(move || {
                let mut at = from;
                while at < to {
                    // uneven batches, so chunks and pushes don't line up
                    let n = (1 + (at * 7919) % 1700).min(to - at);
                    let batch: Vec<i16> = (at..at + n).flat_map(|i| sample(k as u32, i)).collect();
                    h.push(&batch).unwrap();
                    at += n;
                }
            });
        }
        for (k, ((number, _), (from, to))) in tracks.events.iter().zip(events).enumerate() {
            let mut h = muxes
                .1
                .events(
                    *number,
                    EventsOptions {
                        chunk_items: CHUNK_ITEMS,
                        latency: None,
                        first_item: ItemNum::new(*from),
                        ..EventsOptions::default()
                    },
                )
                .unwrap();
            let (from, to) = (*from, *to);
            s.spawn(move || {
                for i in from..to {
                    let t = start(k as u32, i);
                    h.push(t, t, 0, &[k as u8; 3]).unwrap();
                }
            });
        }
    });
}

/// The bytes of pieces 0, 1, ... of `mem`, while there are any.
fn pieces(mem: &MemTapes) -> Vec<Vec<u8>> {
    (0..)
        .map_while(|seq| mem.get(&seq).unwrap())
        .map(|t| t.to_vec().unwrap())
        .collect()
}

/// A store holding the closed pieces `done` and the start `open` of the next, unclosed.
fn plant(done: &[Vec<u8>], open: &[u8]) -> MemTapes {
    let mem = MemTapes::new();
    for (j, w) in done.iter().enumerate() {
        let mut s = mem.create(&(j as u64)).unwrap();
        s.write_all(w).unwrap();
        s.close().unwrap();
    }
    let mut s = mem.create(&(done.len() as u64)).unwrap();
    s.write_all(open).unwrap();
    mem
}

/// Every piece of `pieces` checks, and they form one whole chain.
fn whole(pieces: &[Vec<u8>], what: &str) {
    let mut summaries = Vec::new();
    for (i, p) in pieces.iter().enumerate() {
        let r = TapeReader::open(&p[..]).unwrap();
        assert!(r.verify(0).unwrap().holds, "{what}: piece {i}");
        summaries.extend(r.pieces().iter().filter_map(|p| p.summary()));
    }
    assert_eq!(summaries.len(), pieces.len(), "{what}: every piece closed");
    assert!(check_chain(&summaries).is_whole(), "{what}: one chain");
}

/// The chunks of track `id` in `pieces` as (first item, values): a series chunk's samples, an events chunk's
/// starts; in item order. Asserts the chain holds no track of `foreign`.
fn chunks(pieces: &[Vec<u8>], id: TrackId, foreign: &[TrackId]) -> Vec<(u64, Vec<i64>)> {
    let shape = SampleShape::new(ElementDesc::of(Element::I16), 2);
    let mut out = Vec::new();
    let mut buf = Vec::new();
    for p in pieces {
        let r = TapeReader::open(&p[..]).unwrap();
        for f in foreign {
            assert!(
                r.track_entries(0, *f).unwrap().is_empty(),
                "a track of the other chain"
            );
        }
        for e in r.track_entries(0, id).unwrap() {
            let b = r.read_block(0, &e, &mut buf).unwrap();
            let values: Vec<i64> = match e.kind {
                BlockKind::Series => {
                    let c = b.chunk().unwrap();
                    let col = c.find(ColumnKind::Samples, 0).unwrap();
                    let s = Samples::of(&col, shape, e.count as usize).unwrap();
                    let mut v = vec![0i16; e.count as usize * 2];
                    s.decode(0, &mut v).unwrap();
                    v.into_iter().map(i64::from).collect()
                }
                BlockKind::Events => {
                    let t = EventTimes::read(&b.chunk().unwrap()).unwrap();
                    (0..t.len()).map(|i| t.start(i)).collect()
                }
                _ => continue,
            };
            out.push((e.first_item.get(), values));
        }
    }
    out.sort_by_key(|c| c.0);
    out
}

/// The values of `chunks` joined, each chunk right after the one before from item 0, `per_item` values an item.
fn joined(chunks: &[(u64, Vec<i64>)], per_item: usize, what: &str) -> Vec<i64> {
    let mut out = Vec::new();
    for (first, values) in chunks {
        assert_eq!(
            *first as usize * per_item,
            out.len(),
            "{what}: the chunk after the one before"
        );
        out.extend_from_slice(values);
    }
    out
}

/// The item after the last chunk of track `id` that the closed pieces `done` and the resumed piece's `entries`
/// keep: chunks follow each other, so the last ends where the next goes on.
fn next_item(done: &[Vec<u8>], entries: &[IndexEntry], id: TrackId) -> u64 {
    let closed = done.iter().flat_map(|p| {
        TapeReader::open(&p[..])
            .unwrap()
            .track_entries(0, id)
            .unwrap()
    });
    closed
        .chain(entries.iter().copied())
        .filter(|e| e.track_id == id && matches!(e.kind, BlockKind::Series | BlockKind::Events))
        .map(|e| e.end_item().get())
        .max()
        .unwrap_or(0)
}

/// Write the first part of both chains and crash: both muxes dropped unclosed, as the process ends. The pieces of
/// each chain, the last unclosed.
fn crashed() -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let (a, b) = (MemTapes::new(), MemTapes::new());
    let samples = TapeMux::create(
        a.clone(),
        OWNER,
        SAMPLES,
        &[],
        options(8),
        MuxOptions::default(),
    )
    .unwrap();
    let events = TapeMux::create(
        b.clone(),
        OWNER,
        EVENTS,
        &[],
        options(6),
        MuxOptions::default(),
    )
    .unwrap();
    let tracks = define(&samples, &events);
    write(
        (&samples, &events),
        &tracks,
        &[(0, SAMPLES_BEFORE); SERIES_TRACKS as usize],
        &[(0, ITEMS_BEFORE); EVENTS_TRACKS as usize],
    );
    drop((samples, events));
    (pieces(&a), pieces(&b))
}

/// Check chain `pieces`: whole, every track's chunks following each other from item 0 to `ends`, every value as
/// written, no track of the other chain.
fn check_chain_of(
    pieces: &[Vec<u8>],
    ids: &[TrackId],
    foreign: &[TrackId],
    ends: &[u64],
    series: bool,
    what: &str,
) {
    whole(pieces, what);
    for (k, (id, end)) in ids.iter().zip(ends).enumerate() {
        let k = k as u32;
        let what = format!("{what}, track {k}");
        let (per_item, want): (usize, Vec<i64>) = if series {
            (
                2,
                (0..*end)
                    .flat_map(|i| sample(k, i).map(i64::from))
                    .collect(),
            )
        } else {
            (1, (0..*end).map(|i| start(k, i)).collect())
        };
        let got = joined(&chunks(pieces, *id, foreign), per_item, &what);
        assert!(
            got == want,
            "{what}: every value as written, {} of {}",
            got.len(),
            want.len()
        );
    }
}

/// Two muxes write two chains at once from five threads, and a crash leaves both open. Cut off at many bytes, each
/// at its own, both resume in the same process at once: each track gets its number back and goes on right after
/// its last chunk kept, both chains close whole, and neither holds a block of the other.
#[test]
fn two_muxes_write_two_chains_at_once_and_both_resume_after_a_crash() {
    const CUTS: usize = 200;
    let (a, b) = crashed();
    assert!(
        a.len() > 2 && b.len() > 2,
        "both chains are cut into pieces: {} and {}",
        a.len(),
        b.len()
    );
    for j in 0..=CUTS {
        // the open piece of each chain cut at its own fraction: the samples' from its start up, the events' down
        let (a_done, a_open) = a.split_at(a.len() - 1);
        let (b_done, b_open) = b.split_at(b.len() - 1);
        let a_at = a_open[0].len() * j / CUTS;
        let b_at = b_open[0].len() * (CUTS - j) / CUTS;
        let what = format!("cut at {a_at} and {b_at}");
        let (am, bm) = (
            plant(a_done, &a_open[0][..a_at]),
            plant(b_done, &b_open[0][..b_at]),
        );
        let (samples, a_resumed) = TapeMux::resume(
            am.clone(),
            OWNER,
            SAMPLES,
            options(8),
            MuxOptions::default(),
        )
        .unwrap();
        let (events, b_resumed) =
            TapeMux::resume(bm.clone(), OWNER, EVENTS, options(6), MuxOptions::default()).unwrap();
        let tracks = define(&samples, &events);
        let series: Vec<(u64, u64)> = tracks
            .series
            .iter()
            .map(|(_, id)| next_item(a_done, &a_resumed.entries, *id))
            .map(|next| (next, next.max(SAMPLES_BEFORE) + 3_500))
            .collect();
        let items: Vec<(u64, u64)> = tracks
            .events
            .iter()
            .map(|(_, id)| next_item(b_done, &b_resumed.entries, *id))
            .map(|next| (next, next.max(ITEMS_BEFORE) + 700))
            .collect();
        write((&samples, &events), &tracks, &series, &items);
        samples.close().unwrap();
        events.close().unwrap();

        let ids = |t: &[(TrackNum, TrackId)]| t.iter().map(|(_, id)| *id).collect::<Vec<_>>();
        let (series_ids, events_ids) = (ids(&tracks.series), ids(&tracks.events));
        let ends = |r: &[(u64, u64)]| r.iter().map(|(_, end)| *end).collect::<Vec<_>>();
        check_chain_of(
            &pieces(&am),
            &series_ids,
            &events_ids,
            &ends(&series),
            true,
            &format!("samples, {what}"),
        );
        check_chain_of(
            &pieces(&bm),
            &events_ids,
            &series_ids,
            &ends(&items),
            false,
            &format!("events, {what}"),
        );
    }
}
