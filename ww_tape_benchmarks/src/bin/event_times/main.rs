//! What event times cost in ww_tape's events chunks against other codecs: bytes per event, coding, fetching item
//! `j`, and finding the first item at or after a tick, per workload, in chunks of `--chunk` items. ww_tape's linear
//! groups (`EncodedTimes::encode`, read in place by `Starts::get` and `Starts::lower_bound`) against raw 64-bit
//! ticks, delta-of-delta varints with a skip index, and Elias-Fano. Then what ends cost beside the starts, for spans
//! back to back and spans with gaps. Every codec's answers are checked against the raw ticks. Prints Markdown tables,
//! the best of `--rounds` rounds, with the load average before and after each.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::print_stderr,
    clippy::print_stdout,
    reason = "a bench reads no file of others: it computes sizes and rates freely, fails by panicking and prints its table"
)]

mod codecs;
mod workloads;

use clap::Parser;
use codecs::{Column, DodVarint, EliasFano, Lookup, Raw, Track};
use std::hint::black_box;
use std::path::PathBuf;
use std::time::Instant;
use vc_ui::Paint;
use workloads::Workload;
use ww_tape::chunk::{chunk_len, write_chunk};
use ww_tape::times::{EncodedTimes, EventTimes};
use ww_tape::{Block, BlockId, ColumnSpec, SessionTag};
use ww_tape_bench::{Cell, Common, Machine, Report, Rng, Table};

#[derive(Parser)]
#[command(
    name = "event_times",
    about = "ww_tape event times against other codecs: bytes, coding, item j and first at or after a tick",
    version = vc_build::version!(),
    after_long_help = "Examples:
  just bench-event-times
  just bench-event-times --events 1000000 --rounds 1
  event_times --journal journal.txt --json docs/bench/event_times.json"
)]
struct Args {
    #[command(flatten)]
    common: Common,
    /// Events of each generated workload.
    #[arg(long, default_value_t = 10_000_000)]
    events: usize,
    /// Video frames of the frame clock workloads.
    #[arg(long, default_value_t = 1_000_000)]
    frames: usize,
    /// Items per chunk.
    #[arg(long, default_value_t = 4096)]
    chunk: usize,
    /// Lookups timed per round.
    #[arg(long, default_value_t = 1 << 20)]
    lookups: usize,
    /// Rounds; the best one counts.
    #[arg(long, default_value_t = 3)]
    rounds: u32,
    /// A system journal, one entry per line with its time first as SECONDS.MICROS (`journalctl -o short-unix`);
    /// without it that workload is skipped.
    #[arg(long, value_name = "FILE", value_hint = clap::ValueHint::FilePath)]
    journal: Option<PathBuf>,
}

impl AsRef<Common> for Args {
    fn as_ref(&self) -> &Common {
        &self.common
    }
}

/// Best of `rounds`, ns per call of `f` over `iters` calls.
fn ns_per(rounds: u32, iters: usize, mut f: impl FnMut(usize) -> u64) -> f64 {
    let mut best = f64::MAX;
    for _ in 0..rounds {
        let t = Instant::now();
        let mut acc = 0u64;
        for i in 0..iters {
            acc = acc.wrapping_add(f(i));
        }
        black_box(acc);
        best = best.min(t.elapsed().as_nanos() as f64 / iters as f64);
    }
    best
}

/// The items and ticks a round looks up: items uniform, ticks half uniform over the time range (a view) and half
/// near an item (a click on data).
struct Lookups {
    items: Vec<usize>,
    ticks: Vec<i64>,
}

fn lookups(v: &[i64], n: usize) -> Lookups {
    let mut r = Rng(42);
    let len = v.len() as u64;
    let (lo, hi) = (v[0], v[v.len() - 1] + 1);
    let items = (0..n).map(|_| r.below(len) as usize).collect();
    let ticks = (0..n)
        .map(|k| {
            if k.is_multiple_of(2) {
                r.range(lo, hi)
            } else {
                v[r.below(len) as usize] + r.range(-1000, 1001)
            }
        })
        .collect();
    Lookups { items, ticks }
}

/// One events chunk of `starts` (and `ends`) as a block.
fn block(starts: &[i64], ends: Option<&[i64]>) -> anyhow::Result<Vec<u8>> {
    let enc = EncodedTimes::encode(starts, ends, None)?;
    let cols: Vec<(ColumnSpec, &[u8])> = enc.columns().collect();
    let specs: Vec<ColumnSpec> = cols.iter().map(|c| c.0).collect();
    let head = enc.head.chunk_head(9, 1, 0);
    let mut buf = vec![0u8; chunk_len(&head, &specs, 0)];
    let id = BlockId::new(SessionTag::from_u32(1), 0);
    let len = write_chunk(&mut buf, id, &head, &cols, 0).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    buf.truncate(len);
    Ok(buf)
}

/// Bytes of the time columns of `starts` (and `ends`) in chunks of `chunk` items.
fn time_bytes(starts: &[i64], ends: Option<&[i64]>, chunk: usize) -> anyhow::Result<usize> {
    let mut bytes = 0;
    for (k, s) in starts.chunks(chunk).enumerate() {
        let e = ends.map(|e| &e[k * chunk..k * chunk + s.len()]);
        let enc = EncodedTimes::encode(s, e, None)?;
        bytes += enc.columns().map(|(_, b)| b.len()).sum::<usize>();
    }
    Ok(bytes)
}

/// ww_tape's events chunks of a track, their starts read in place.
struct Tape<'b> {
    chunk: usize,
    times: Vec<EventTimes<'b>>,
    firsts: Vec<i64>,
}

impl Lookup for Tape<'_> {
    fn get(&self, i: usize) -> i64 {
        self.times[i / self.chunk].starts().get(i % self.chunk)
    }

    fn lower_bound(&self, t: i64) -> usize {
        let idx = self.firsts.partition_point(|&f| f < t);
        if idx == 0 {
            return 0;
        }
        let c = idx - 1;
        c * self.chunk + self.times[c].starts().lower_bound(t)
    }
}

/// Check `track` against the raw ticks, then time item `j` and the first at or after a tick.
fn lookup_ns(a: &Args, v: &[i64], q: &Lookups, track: &impl Lookup, name: &str) -> (f64, f64) {
    for k in 0..q.items.len().min(4096) {
        let i = q.items[k];
        assert_eq!(track.get(i), v[i], "{name}: item {i}");
        let t = q.ticks[k];
        assert_eq!(
            track.lower_bound(t),
            v.partition_point(|&x| x < t),
            "{name}: first at or after {t}"
        );
    }
    let n = q.items.len();
    let get = ns_per(a.rounds, n, |k| track.get(q.items[k]) as u64);
    let lower = ns_per(a.rounds, n, |k| track.lower_bound(q.ticks[k]) as u64);
    (get, lower)
}

fn row(t: &mut Table, w: &Workload, codec: &str, bytes: usize, encode: f64, look: (f64, f64)) {
    let n = w.starts.len() as f64;
    t.row(vec![
        Cell::text(&w.name),
        Cell::text(codec),
        Cell::num(bytes as f64 / n, 3),
        Cell::num(encode / n, 1),
        Cell::num(look.0, 1),
        Cell::num(look.1, 1),
    ]);
}

fn other<C: Column>(a: &Args, t: &mut Table, w: &Workload, q: &Lookups) {
    let mut encode = f64::MAX;
    let mut track = None;
    for _ in 0..a.rounds {
        let at = Instant::now();
        let built = Track::<C>::build(&w.starts, a.chunk);
        encode = encode.min(at.elapsed().as_nanos() as f64);
        track = Some(built);
    }
    let track = track.expect("one round or more");
    let look = lookup_ns(a, &w.starts, q, &track, C::NAME);
    row(t, w, C::NAME, track.bytes(), encode, look);
}

/// Every codec on workload `w`.
fn workload(a: &Args, t: &mut Table, w: &Workload) -> anyhow::Result<()> {
    eprintln!("{}: {} events", w.name, w.starts.len());
    let q = lookups(&w.starts, a.lookups);
    let mut encode = f64::MAX;
    for _ in 0..a.rounds {
        let at = Instant::now();
        for s in w.starts.chunks(a.chunk) {
            black_box(EncodedTimes::encode(black_box(s), None, None)?);
        }
        encode = encode.min(at.elapsed().as_nanos() as f64);
    }
    let blocks: Vec<Vec<u8>> = w
        .starts
        .chunks(a.chunk)
        .map(|s| block(s, None))
        .collect::<anyhow::Result<_>>()?;
    let read: Vec<Block<'_>> = blocks
        .iter()
        .map(|b| Block::read(b).map_err(|e| anyhow::anyhow!("{e:?}")))
        .collect::<anyhow::Result<_>>()?;
    let chunks: Vec<_> = read
        .iter()
        .map(|b| b.chunk().ok_or_else(|| anyhow::anyhow!("not a chunk")))
        .collect::<anyhow::Result<_>>()?;
    let tape = Tape {
        chunk: a.chunk,
        times: chunks
            .iter()
            .map(EventTimes::read)
            .collect::<Result<_, _>>()?,
        firsts: w.starts.chunks(a.chunk).map(|c| c[0]).collect(),
    };
    let look = lookup_ns(a, &w.starts, &q, &tape, "ww_tape");
    let bytes = time_bytes(&w.starts, None, a.chunk)?;
    row(t, w, "ww_tape linear groups", bytes, encode, look);
    other::<Raw>(a, t, w, &q);
    other::<DodVarint>(a, t, w, &q);
    other::<EliasFano>(a, t, w, &q);
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let a: Args = ww_tape_bench::args();
    anyhow::ensure!(
        a.events > 0 && a.frames > 0 && a.chunk > 0 && a.lookups > 0 && a.rounds > 0,
        "events, frames, chunk, lookups and rounds must be positive"
    );
    let paint = Paint::stdout();
    let mut report = Report::new(
        "event_times",
        vc_build::version!(),
        Machine::this(),
        vec![
            ("events", a.events.to_string()),
            ("frames", a.frames.to_string()),
            ("chunk", a.chunk.to_string()),
            ("lookups", a.lookups.to_string()),
            ("rounds", a.rounds.to_string()),
        ],
    );
    println!("{}", report.header(paint));
    let mut sets = vec![
        workloads::exact_gaps(a.events),
        workloads::jitter(a.events),
        workloads::bursty(a.events),
    ];
    match &a.journal {
        Some(path) => sets.push(workloads::journal(&std::fs::read_to_string(path)?)),
        None => eprintln!("journal: skipped, no --journal file"),
    }
    sets.push(workloads::frames(a.frames, 60_000));
    sets.push(workloads::frames(a.frames, 1_000_000_000));
    let mut t = report.table(
        "codecs",
        &format!(
            "Event start ticks in chunks of {} items, the time columns alone; lookups of uniform items and ticks, best of {}.",
            a.chunk, a.rounds
        ),
        &[
            "Workload",
            "Codec",
            "Bytes per event",
            "Coding ns per event",
            "Item j ns",
            "First at or after a tick ns",
        ],
    );
    for w in sets.iter().filter(|w| !w.starts.is_empty()) {
        workload(&a, &mut t, w)?;
    }
    println!("{}", report.push(t, paint));

    let mut e = report.table(
        "ends",
        &format!(
            "Spans: starts and ends in chunks of {} items, the time columns alone.",
            a.chunk
        ),
        &[
            "Workload",
            "Raw starts and ends bytes per event",
            "Starts bytes per event",
            "Starts and ends bytes per event",
        ],
    );
    for w in [
        workloads::spans_back_to_back(a.events),
        workloads::spans_gaps(a.events),
    ] {
        let n = w.starts.len() as f64;
        let starts = time_bytes(&w.starts, None, a.chunk)?;
        let both = time_bytes(&w.starts, w.ends.as_deref(), a.chunk)?;
        e.row(vec![
            Cell::text(&w.name),
            Cell::num(16.0, 3),
            Cell::num(starts as f64 / n, 3),
            Cell::num(both as f64 / n, 3),
        ]);
    }
    println!("{}", report.push(e, paint));
    report.finish(a.common.json.as_deref())
}
