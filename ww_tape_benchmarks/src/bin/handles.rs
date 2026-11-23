//! Typed track handles on a `TapeMux` against one thread writing a chain directly, by 1, 4 and 16 tracks.
//!
//! Two loads: small events (instants with a 16-byte payload and their item trees) and the RF capture's IQ (12-bit
//! packed, with summaries), a track per producer. The direct writer encodes every track's chunks and builds their
//! summaries on one thread, a chunk of each track in turn, as an application without the mux does. Through the mux
//! each producer thread encodes its own track's chunks, hashes their payload and computes the summaries' leaves;
//! the writing thread places the chunks and builds the levels above. An events producer pushes a chunk's worth of
//! events per call, and in a second row one event per call. Both write into a store that keeps no bytes,
//! so the encoding and the writer's work are measured, not a disk. Each table carries the load average before and
//! after it.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::print_stdout,
    reason = "a bench reads no file of others: it computes sizes and rates freely and prints its table"
)]

use clap::Parser;
use exact_units::{Count, Magnitude};
use example_capture::rf::{IQ_ELEMENT, Rf, iq_shape, summaries};
use shrink_wrap::prelude::*;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use ww_tape::chunk::ChunkHead;
use ww_tape::index::IndexPolicy;
use ww_tape::payloads::EncodedPayloads;
use ww_tape::samples::{encode, max_encoded_len};
use ww_tape::times::EncodedTimes;
use ww_tape::{
    BlockKind, ChainOptions, ClockNum, ClockOrigin, ClockSpec, ColumnKind, ColumnSpec, CutPolicy,
    Definitions, EndMode, EventsDef, GridDef, ItemNum, Items, OwnerId, PieceChain, PieceTapes,
    Segment, SyncPolicy, TrackId, TrackKind, TrackNum, TrackRepeat, TrackSpec, WriterOptions,
    clock_key, track_key,
};
use ww_tape_bench::{Bench, Cell, Common, Discard};
use ww_tape_mux::{Event, EventsOptions, MuxOptions, SeriesOptions, TapeMux};
use ww_tape_summary::{EventFields, EventTree, SeriesStart, SeriesSummaries};

#[derive(Parser)]
#[command(
    name = "handles",
    about = "Events and series handles on a TapeMux against one thread writing a chain directly",
    version = vc_build::version!(),
    after_long_help = "Examples:
  just bench-handles
  handles --producers 1,2,4,8 --events 2000000 --rf-seconds 4"
)]
struct Args {
    #[command(flatten)]
    common: Common,
    /// The numbers of tracks, each a producer thread on the mux.
    #[arg(long, value_delimiter = ',', default_values_t = [1, 4, 16])]
    producers: Vec<usize>,
    /// Events of a run, split evenly across its tracks.
    #[arg(long, default_value_t = 8_000_000)]
    events: u64,
    /// Items of an events chunk.
    #[arg(long, default_value_t = 4_096)]
    event_chunk: u32,
    /// Seconds of the RF capture (20 MS/s of IQ) a run writes, split evenly across its tracks.
    #[arg(long, default_value_t = 16)]
    rf_seconds: u32,
}

/// An error of the library, kept as its debug text.
fn err<E: core::fmt::Debug>(e: E) -> anyhow::Error {
    anyhow::anyhow!("{e:?}")
}

const OWNER: OwnerId = OwnerId([0x6b; 32]);

/// The payload of event `i` of a track: its number, then 8 bytes the same in every event.
fn payload(i: u64) -> [u8; 16] {
    let mut p = *b"........handles!";
    p[..8].copy_from_slice(&i.to_le_bytes());
    p
}

/// Ticks from one event of a track to the next.
const EVENT_STEP: i64 = 1_000;

/// A chain of a piece per 1000 chunks, with no sync and no checkpoint by time.
fn options() -> ChainOptions {
    ChainOptions {
        cut: CutPolicy {
            bytes: exact_units::Bytes::new(0),
            chunks: Count::new(1_000),
            interval: None,
        },
        writer: WriterOptions {
            producer: "handles bench".to_owned(),
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
        key: clock_key(0x6b, &[], 1),
        rate: Magnitude::pow10(9).into(),
        resolution: Magnitude::pow10(-9).into(),
        origin: ClockOrigin::Boot,
        label: "host",
    }
}

/// The two loads.
#[derive(Copy, Clone, PartialEq, Eq)]
enum Load {
    Events,
    Iq,
}

/// Track `k` of `load`.
fn track(load: Load, k: u32, clock: ClockNum, rf: &Rf) -> anyhow::Result<TrackSpec<'static>> {
    let (items, kind) = match load {
        Load::Events => (Items::RawBytes, TrackKind::Events(EventsDef::INSTANTS)),
        Load::Iq => {
            let def: GridDef<'static> = rf.def().ok_or_else(|| anyhow::anyhow!("the rate"))?;
            (
                Items::Numbers {
                    element: IQ_ELEMENT,
                    unit: None,
                },
                TrackKind::Series(def),
            )
        }
    };
    Ok(TrackSpec {
        key: track_key(0x6b, &[Segment::Index(k)]),
        clock,
        dimensions: RefVec::Slice { slice: &[] },
        items,
        label: "handles",
        repeat: TrackRepeat::Stream,
        kind,
        relations: RefVec::Slice { slice: &[] },
        owner: [0; 32],
    })
}

/// What a run took.
struct Run {
    wall: Duration,
    written: u64,
    waits: u64,
}

/// The work of a run: `tracks` tracks of `load`, `chunks` chunks each.
#[derive(Copy, Clone)]
struct Work<'a> {
    load: Load,
    tracks: usize,
    chunks: u64,
    event_chunk: u32,
    rf: &'a Rf,
    /// One chunk of the capture's IQ, pushed again and again.
    iq: &'a [i16],
}

impl Work<'_> {
    /// Items of a chunk.
    fn chunk_items(&self) -> u64 {
        match self.load {
            Load::Events => u64::from(self.event_chunk),
            Load::Iq => u64::from(self.rf.chunk_samples),
        }
    }

    /// Items of the run, all tracks.
    fn items(&self) -> u64 {
        self.tracks as u64 * self.chunks * self.chunk_items()
    }
}

/// A track's summary builder on the direct path.
enum Builder {
    Events(EventTree),
    Iq(Box<SeriesSummaries<i16>>),
}

impl Builder {
    fn new(w: &Work<'_>, id: TrackId, track: TrackNum, first: u64) -> anyhow::Result<Self> {
        Ok(match w.load {
            Load::Events => Self::Events(EventTree::new(
                id,
                track,
                EventFields {
                    flags: false,
                    latest_end: false,
                },
                ItemNum::new(first),
            )),
            Load::Iq => {
                let def = w.rf.def().ok_or_else(|| anyhow::anyhow!("the rate"))?;
                let start = SeriesStart {
                    track_id: id,
                    track_number: track,
                    shape: iq_shape(),
                    first_item: ItemNum::new(first),
                    grid: def.first_grid(0),
                    period: def.period,
                };
                Self::Iq(Box::new(
                    SeriesSummaries::new(start, summaries()).map_err(err)?,
                ))
            }
        })
    }

    fn finish(&mut self) -> anyhow::Result<Vec<Vec<u8>>> {
        Ok(match self {
            Self::Events(t) => {
                t.finish().map_err(err)?;
                t.take_blocks()
            }
            Self::Iq(s) => {
                s.finish().map_err(err)?;
                s.take_blocks()
            }
        })
    }
}

/// Encode chunk `first..` of track `track` into `columns`: its head.
fn encode_chunk(
    w: &Work<'_>,
    id: TrackId,
    track: TrackNum,
    first: u64,
    columns: &mut Vec<(ColumnSpec, Vec<u8>)>,
) -> anyhow::Result<ChunkHead> {
    columns.clear();
    match w.load {
        Load::Events => {
            let starts: Vec<i64> = (first..first + u64::from(w.event_chunk))
                .map(|i| i.cast_signed() * EVENT_STEP)
                .collect();
            let times = EncodedTimes::encode(&starts, None, None).map_err(err)?;
            let items: Vec<[u8; 16]> = (first..first + u64::from(w.event_chunk))
                .map(payload)
                .collect();
            let payloads = EncodedPayloads::encode(items.iter().map(|p| &p[..])).map_err(err)?;
            columns.extend(
                times
                    .columns()
                    .chain(payloads.columns())
                    .map(|(s, b)| (s, b.to_vec())),
            );
            Ok(times.head.chunk_head(id, track, ItemNum::new(first)))
        }
        Load::Iq => {
            let def = w.rf.def().ok_or_else(|| anyhow::anyhow!("the rate"))?;
            let shape = iq_shape();
            let count = w.rf.chunk_samples;
            let mut bytes =
                vec![0; max_encoded_len(w.rf.codec, shape, count as usize).map_err(err)?];
            let len = encode(w.rf.codec, shape, w.iq, &mut bytes).map_err(err)?;
            bytes.truncate(len);
            let grid = def.first_grid(0);
            let (first_tick, last_tick) = grid
                .chunk_ticks(def.period, ItemNum::new(first), count)
                .map_err(err)?;
            columns.push((
                ColumnSpec {
                    kind: ColumnKind::Samples,
                    codec: w.rf.codec,
                    param: 0,
                    len: bytes.len(),
                },
                bytes,
            ));
            Ok(ChunkHead {
                kind: BlockKind::Series,
                track_id: id,
                track_number: track,
                first_item: ItemNum::new(first),
                count,
                first_tick,
                last_tick,
                latest_end: last_tick,
                time_mode: 0,
                end_mode: EndMode::Absent,
                grid: Some(grid),
            })
        }
    }
}

/// One thread writes every track's chunks into a chain directly, a chunk of each in turn, with the summaries of
/// each piece ended before its cut.
fn direct(w: &Work<'_>) -> anyhow::Result<Run> {
    let store = Discard::default();
    let start = Instant::now();
    let mut defs = Definitions::new();
    let c = defs.define_clock(clock()).map_err(err)?;
    let mut tracks = Vec::new();
    for k in 0..w.tracks as u32 {
        let spec = track(w.load, k, c, w.rf)?;
        let num = defs.define_track(spec.clone()).map_err(err)?;
        tracks.push((spec.numbered(num).id(), num));
    }
    let mut chain = PieceChain::create_on(store.clone(), OWNER, 0, &[], options()).map_err(err)?;
    let mut builders: Vec<Option<Builder>> = (0..w.tracks).map(|_| None).collect();
    let mut columns = Vec::new();
    let n = w.chunk_items();
    for chunk in 0..w.chunks {
        let first = chunk * n;
        for (k, &(id, num)) in tracks.iter().enumerate() {
            if chain.cut_due() {
                for mut b in builders_mut(&mut builders) {
                    for block in b.finish()? {
                        chain.defer_block(&defs, block).map_err(err)?;
                    }
                }
            }
            let head = encode_chunk(w, id, num, first, &mut columns)?;
            let cols: Vec<_> = columns.iter().map(|(s, b)| (*s, &b[..])).collect();
            chain.write_chunk(&defs, &head, &cols).map_err(err)?;
            let builder = builders
                .get_mut(k)
                .ok_or_else(|| anyhow::anyhow!("a builder per track"))?;
            let b = match builder {
                Some(b) => b,
                None => builder.insert(Builder::new(w, id, num, first)?),
            };
            let blocks = match b {
                Builder::Events(t) => {
                    for i in first..first + n {
                        let tick = i.cast_signed() * EVENT_STEP;
                        t.push(tick, tick, 0).map_err(err)?;
                    }
                    t.take_blocks()
                }
                Builder::Iq(s) => {
                    s.push(w.iq, 0).map_err(err)?;
                    s.take_blocks()
                }
            };
            for block in blocks {
                chain.defer_block(&defs, block).map_err(err)?;
            }
        }
    }
    for mut b in builders_mut(&mut builders) {
        for block in b.finish()? {
            chain.defer_block(&defs, block).map_err(err)?;
        }
    }
    chain.close().map_err(err)?;
    Ok(Run {
        wall: start.elapsed(),
        written: store.written.load(Ordering::Relaxed),
        waits: 0,
    })
}

/// The builders of `all` that hold a piece's summaries, taken out: the next chunk starts new ones.
fn builders_mut(all: &mut [Option<Builder>]) -> impl Iterator<Item = Builder> + '_ {
    all.iter_mut().filter_map(Option::take)
}

/// A producer thread per track pushes into its handle on a mux: events a chunk's worth per call with `batch`, else
/// one by one.
fn mux(w: &Work<'_>, batch: bool) -> anyhow::Result<Run> {
    let store = Discard::default();
    let start = Instant::now();
    let mux = TapeMux::create(
        store.clone(),
        OWNER,
        0,
        &[],
        options(),
        MuxOptions::default(),
    )
    .map_err(err)?;
    let c = mux.define_clock(clock()).map_err(err)?;
    let n = w.chunk_items();
    std::thread::scope(|s| -> anyhow::Result<()> {
        let mut producers = Vec::new();
        for k in 0..w.tracks as u32 {
            let num = mux.define_track(track(w.load, k, c, w.rf)?).map_err(err)?;
            let chunks = w.chunks;
            producers.push(match w.load {
                Load::Events => {
                    let mut h = mux
                        .events(
                            num,
                            EventsOptions {
                                chunk_items: w.event_chunk,
                                ..EventsOptions::default()
                            },
                        )
                        .map_err(err)?;
                    s.spawn(move || -> anyhow::Result<()> {
                        if !batch {
                            for i in 0..chunks * n {
                                let tick = i.cast_signed() * EVENT_STEP;
                                h.push(tick, tick, 0, &payload(i)).map_err(err)?;
                            }
                            return h.finish().map_err(err);
                        }
                        let mut payloads = Vec::with_capacity(n as usize);
                        for chunk in 0..chunks {
                            payloads.clear();
                            payloads.extend((chunk * n..(chunk + 1) * n).map(payload));
                            let items: Vec<Event<'_>> = payloads
                                .iter()
                                .enumerate()
                                .map(|(j, p)| {
                                    let tick = (chunk * n + j as u64).cast_signed() * EVENT_STEP;
                                    Event {
                                        start: tick,
                                        end: tick,
                                        payload: &p[..],
                                        ..Event::default()
                                    }
                                })
                                .collect();
                            h.push_all(&items).map_err(err)?;
                        }
                        h.finish().map_err(err)
                    })
                }
                Load::Iq => {
                    let mut h = mux
                        .series::<i16>(
                            num,
                            SeriesOptions {
                                chunk_samples: w.rf.chunk_samples,
                                codec: w.rf.codec,
                                ..SeriesOptions::default()
                            },
                        )
                        .map_err(err)?;
                    let iq = w.iq;
                    s.spawn(move || -> anyhow::Result<()> {
                        for _ in 0..chunks {
                            h.push(iq).map_err(err)?;
                        }
                        h.finish().map_err(err)
                    })
                }
            });
        }
        for p in producers {
            p.join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))?;
        }
        Ok(())
    })?;
    let stats = mux.stats();
    mux.close().map_err(err)?;
    anyhow::ensure!(
        stats.drops == 0 && stats.refused == 0 && stats.lost == 0,
        "the mux lost units: {stats:?}"
    );
    Ok(Run {
        wall: start.elapsed(),
        written: store.written.load(Ordering::Relaxed),
        waits: stats.waits,
    })
}

/// A table of `load`: per number of tracks the direct writer and the mux.
fn table(bench: &mut Bench, args: &Args, load: Load, rf: &Rf, iq: &[i16]) -> anyhow::Result<()> {
    let (id, title, unit, per_track) = match load {
        Load::Events => (
            "events",
            format!(
                "{} instants with a 16-byte payload and their item trees, {} to a chunk, split across the tracks.",
                args.events, args.event_chunk
            ),
            "M events/s",
            args.events,
        ),
        Load::Iq => (
            "iq",
            format!(
                "{} s of 20 MS/s IQ, 12-bit packed with summaries, chunks of 10 ms, split across the tracks.",
                args.rf_seconds
            ),
            "MS/s",
            u64::from(args.rf_seconds) * u64::from(rf.rate_hz),
        ),
    };
    let mut t = bench.table(
        id,
        &title,
        &[
            "Writer",
            "Tracks",
            "ms",
            unit,
            "MB/s written",
            "vs direct",
            "Waits",
        ],
    );
    for &tracks in &args.producers {
        anyhow::ensure!(tracks > 0, "--producers holds 0");
        let chunk_items = match load {
            Load::Events => u64::from(args.event_chunk),
            Load::Iq => u64::from(rf.chunk_samples),
        };
        let w = Work {
            load,
            tracks,
            chunks: (per_track / chunk_items / tracks as u64).max(1),
            event_chunk: args.event_chunk,
            rf,
            iq,
        };
        let d = direct(&w)?;
        let direct_secs = d.wall.as_secs_f64();
        let mut runs = vec![("direct", d), ("mux", mux(&w, true)?)];
        if load == Load::Events {
            runs.push(("mux, one by one", mux(&w, false)?));
        }
        for (writer, run) in &runs {
            let secs = run.wall.as_secs_f64();
            t.row(vec![
                Cell::text(*writer),
                Cell::count(tracks as u64),
                Cell::num(secs * 1e3, 0),
                Cell::num(w.items() as f64 / secs / 1e6, 1),
                Cell::num(run.written as f64 / secs / 1e6, 0),
                Cell::num(direct_secs / secs, 2),
                Cell::count(run.waits),
            ]);
        }
    }
    println!("{}", bench.push(t));
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let args: Args = ww_tape_bench::args();
    let rf = Rf::capture();
    let mut iq = vec![0i16; rf.chunk_samples as usize * 2];
    rf.signal().fill(0, &mut iq);
    let producers: Vec<String> = args.producers.iter().map(ToString::to_string).collect();
    let mut bench = Bench::start(
        "handles",
        vc_build::build_info!(),
        &args.common,
        vec![
            ("tracks", producers.join(",")),
            ("events", args.events.to_string()),
            ("RF s", args.rf_seconds.to_string()),
        ],
    )?;
    println!("{}", bench.header());
    table(&mut bench, &args, Load::Events, &rf, &iq)?;
    table(&mut bench, &args, Load::Iq, &rf, &iq)?;
    bench.finish()
}
