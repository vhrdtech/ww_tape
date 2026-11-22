//! The charts and tables of "Sizes and speeds" and "The definitions a new piece carries", drawn from the bench reports in `docs/bench/` (`just bench-book`):
//!
//! - `sample_codecs.svg`: ratio and decode time per data set, fixed point against `ALP` and the best other float codec.
//! - `event_times.svg`: bytes per event and the time to find a tick, per workload and codec.
//! - `chunk_cost.svg`: what the writer spends on a 1 MiB series chunk, by part.
//! - `drive_sizes.svg`: where the bytes of the drive's hour go.
//! - `bench_*.txt`: the tables the pages include, each with the bench and the load average it ran at.
//! - `bench_machine.txt`: the machine, build and commit every report of "Sizes and speeds" ran, and the range of the
//!   load average; `bench_cases_machine.txt` the same of the reports of the two case pages, a run of their own.
//!
//! [says] gives the numbers the page's prose computes from the reports, and the numbers the reports name.

mod chart;
mod says;

pub use says::{Says, says};

use anyhow::{Context, ensure};
use chart::{Chart, Panel, Series};
use std::path::Path;
use ww_tape_bench_report::{Cell, Report, Table};

/// The bench reports, read from `dir`.
pub struct Reports {
    sample_codecs: Report,
    event_times: Report,
    summaries: Report,
    chunks: Report,
    tracks: Report,
    drive: Report,
    carried: Report,
    memory: Report,
    rf_core: Report,
    rf_chain: Report,
    ethernet: Report,
    wideband: Report,
}

/// The reports in `dir`, each a release build of a clean commit.
pub fn read(dir: &Path) -> anyhow::Result<Reports> {
    let r = |name: &str| {
        let report = Report::read(&dir.join(format!("{name}.json")))
            .with_context(|| format!("docs/bench/{name}.json: run `just bench-book`"))?;
        report.measured()?;
        Ok::<_, anyhow::Error>(report)
    };
    Ok(Reports {
        sample_codecs: r("sample_codecs")?,
        event_times: r("event_times")?,
        summaries: r("summaries")?,
        chunks: r("chunks")?,
        tracks: r("tracks")?,
        drive: r("drive")?,
        carried: r("carried")?,
        memory: r("memory")?,
        rf_core: r("rf_core")?,
        rf_chain: r("rf_chain")?,
        ethernet: r("ethernet")?,
        wideband: r("wideband")?,
    })
}

fn table<'r>(report: &'r Report, id: &str) -> anyhow::Result<&'r Table> {
    report
        .get(id)
        .with_context(|| format!("table {id} of bench {}", report.bench))
}

/// The value of `column` in `row`.
fn value(t: &Table, row: &[Cell], column: &str) -> anyhow::Result<f64> {
    let c = t
        .column(column)
        .with_context(|| format!("column {column} of table {}", t.id))?;
    row.get(c)
        .and_then(Cell::value)
        .with_context(|| format!("a number in column {column} of table {}", t.id))
}

/// The text of cell `at` of `row`.
fn text(row: &[Cell], at: usize) -> anyhow::Result<&str> {
    row.get(at)
        .and_then(Cell::as_text)
        .with_context(|| format!("text in cell {at} of a row {row:?}"))
}

/// The rows of `t` whose first cells are `keys`.
fn rows<'t>(t: &'t Table, keys: &[&str]) -> anyhow::Result<Vec<&'t Vec<Cell>>> {
    let mut out = Vec::new();
    for r in &t.rows {
        let mut hit = true;
        for (at, k) in keys.iter().enumerate() {
            hit &= text(r, at)? == *k;
        }
        if hit {
            out.push(r);
        }
    }
    Ok(out)
}

/// The one row of `t` whose first cells are `keys`.
fn row<'t>(t: &'t Table, keys: &[&str]) -> anyhow::Result<&'t Vec<Cell>> {
    let found = rows(t, keys)?;
    ensure!(found.len() == 1, "one row {keys:?} in table {}", t.id);
    found.into_iter().next().context("one row")
}

/// The distinct first cells of `t`'s rows, in order.
fn groups(t: &Table) -> anyhow::Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for r in &t.rows {
        let g = text(r, 0)?;
        if !out.iter().any(|o| o == g) {
            out.push(g.to_owned());
        }
    }
    Ok(out)
}

/// What a chart says under its legend: the bench and the load average of its table.
fn note(report: &Report, t: &Table) -> String {
    format!(
        "just bench-{} on {}, load average {}.",
        report.bench.replace('_', "-"),
        report.machine.host,
        t.load()
    )
}

/// `s` without the backticks of Markdown code spans.
fn plain(s: &str) -> String {
    s.replace('`', "")
}

fn series(names: &[(&str, &'static str)]) -> Vec<Series> {
    names
        .iter()
        .map(|&(name, class)| Series {
            name: name.to_owned(),
            class,
        })
        .collect()
}

/// The float codecs besides `ALP`; the bench keeps them to compare, the format has none of them.
const FLOAT_CODECS: [&str; 4] = ["GORILLA", "CHIMP", "FLOAT_DELTA", "FLOAT_DELTA2"];

/// Of the rows of one data set: the fixed-point row, `ALP`'s, and the other float codec of the best ratio.
struct SetRows<'t> {
    fixed: &'t Vec<Cell>,
    alp: &'t Vec<Cell>,
    best: &'t Vec<Cell>,
}

fn set_rows<'t>(t: &'t Table, set: &str) -> anyhow::Result<SetRows<'t>> {
    let all = rows(t, &[set])?;
    let mut fixed = None;
    let mut best: Option<&Vec<Cell>> = None;
    for r in &all {
        let codec = text(r, 1)?;
        if codec.starts_with("DELTA of") {
            fixed = Some(*r);
        } else if FLOAT_CODECS.contains(&codec) {
            let better = match best {
                Some(b) => value(t, r, "Ratio")? > value(t, b, "Ratio")?,
                None => true,
            };
            if better {
                best = Some(*r);
            }
        }
    }
    Ok(SetRows {
        fixed: fixed.with_context(|| format!("the fixed-point row of {set}"))?,
        alp: row(t, &[set, "ALP"])?,
        best: best.with_context(|| format!("the float codecs of {set}"))?,
    })
}

fn sample_codecs(r: &Reports) -> anyhow::Result<String> {
    let t = table(&r.sample_codecs, "codecs")?;
    let sets = groups(t)?;
    let (mut ratio, mut decode, mut tags) = (Vec::new(), Vec::new(), Vec::new());
    for set in &sets {
        let s = set_rows(t, set)?;
        let three = [s.fixed, s.alp, s.best];
        let mut rr = Vec::new();
        let mut dd = Vec::new();
        for row in three {
            rr.push(Some(value(t, row, "Ratio")?));
            dd.push(Some(value(t, row, "Decode ns per value")?));
        }
        ratio.push(rr);
        decode.push(dd);
        tags.push(vec![None, None, Some(text(s.best, 1)?.to_owned())]);
    }
    Ok(Chart {
        label: "Sample codecs per data set: ratio to the raw floats and decode time per value, fixed point with DELTA against ALP and the best other float codec, named at its bar".to_owned(),
        rows: sets,
        series: series(&[
            ("fixed point, DELTA", "blue"),
            ("ALP", "accent"),
            ("best other float codec", "amber"),
        ]),
        panels: vec![
            Panel {
                axis: "Ratio to the raw floats (larger: smaller file)".to_owned(),
                log: false,
                digits: 2,
                values: ratio,
                tags: tags.clone(),
            },
            Panel {
                axis: "Decode, ns per value".to_owned(),
                log: false,
                digits: 2,
                values: decode,
                tags,
            },
        ],
        note: note(&r.sample_codecs, t),
    }
    .svg())
}

/// The codecs of the event time bench, as its table names them, and their bars' class.
const EVENT_CODECS: [(&str, &str); 4] = [
    ("ww_tape linear groups", "blue"),
    ("delta-of-delta varints", "amber"),
    ("Elias-Fano", "green"),
    ("raw 64-bit ticks", "tint-ink"),
];

fn event_times(r: &Reports) -> anyhow::Result<String> {
    let t = table(&r.event_times, "codecs")?;
    let workloads = groups(t)?;
    let mut bytes = Vec::new();
    let mut find = Vec::new();
    for w in &workloads {
        let mut b = Vec::new();
        let mut f = Vec::new();
        for (codec, _) in EVENT_CODECS {
            let row = row(t, &[w, codec])?;
            b.push(Some(value(t, row, "Bytes per event")?));
            f.push(Some(value(t, row, "First at or after a tick ns")?));
        }
        bytes.push(b);
        find.push(f);
    }
    Ok(Chart {
        label: "Event start ticks per workload: bytes per event on a log scale, and the time to find the first event at or after a tick, for ww_tape's linear groups, delta-of-delta varints, Elias-Fano and raw ticks".to_owned(),
        rows: workloads,
        series: series(&EVENT_CODECS),
        panels: vec![
            Panel {
                axis: "Bytes per event (log scale)".to_owned(),
                log: true,
                digits: 3,
                values: bytes,
                tags: Vec::new(),
            },
            Panel {
                axis: "First at or after a tick, ns".to_owned(),
                log: false,
                digits: 0,
                values: find,
                tags: Vec::new(),
            },
        ],
        note: note(&r.event_times, t),
    }
    .svg())
}

fn chunk_cost(r: &Reports) -> anyhow::Result<String> {
    let t = table(&r.chunks, "writer")?;
    let mut rows = Vec::new();
    let mut us = Vec::new();
    for row in &t.rows {
        rows.push(plain(text(row, 0)?));
        us.push(vec![Some(value(t, row, "ns per chunk")? / 1000.0)]);
    }
    Ok(Chart {
        label: "What a 1 MiB series chunk costs the writer and its parts, µs per chunk".to_owned(),
        rows,
        series: series(&[("µs per 1 MiB chunk", "blue")]),
        panels: vec![Panel {
            axis: "µs per 1 MiB series chunk".to_owned(),
            log: false,
            digits: 0,
            values: us,
            tags: Vec::new(),
        }],
        note: note(&r.chunks, t),
    }
    .svg())
}

fn drive_sizes(r: &Reports) -> anyhow::Result<String> {
    let t = table(&r.drive, "sizes")?;
    let mut rows = Vec::new();
    let mut mib = Vec::new();
    for row in &t.rows {
        let part = text(row, 0)?;
        let bytes = value(t, row, "Bytes")?;
        if part == "the recording" || bytes == 0.0 {
            continue;
        }
        rows.push(plain(part));
        mib.push(vec![Some(bytes / f64::from(1 << 20))]);
    }
    Ok(Chart {
        label: "Where the bytes of the drive's hour go, MiB by part of the recording, log scale"
            .to_owned(),
        rows,
        series: series(&[("MiB of the hour", "blue")]),
        panels: vec![Panel {
            axis: "MiB of the drive's hour (log scale)".to_owned(),
            log: true,
            digits: 3,
            values: mib,
            tags: Vec::new(),
        }],
        note: format!(
            "just bench-drive on {}: sizes don't change with the load.",
            r.drive.machine.host
        ),
    }
    .svg())
}

/// A number as a page shows it: whole numbers of five digits or more in groups of three, split by spaces.
pub(crate) fn shown(c: &Cell) -> String {
    match c {
        Cell::Num { value, digits: 0 } if value.abs() >= 10_000.0 => {
            grouped(&format!("{value:.0}"))
        }
        c => c.to_string(),
    }
}

/// A whole number's digits in groups of three, split by spaces.
pub(crate) fn grouped(s: &str) -> String {
    let (sign, digits) = s.split_at(usize::from(s.starts_with('-')));
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(' ');
        }
        out.push(ch);
    }
    format!("{sign}{out}")
}

/// Table `id` of `report` as Markdown a page includes, then the line saying which bench it is and its load.
fn markdown(report: &Report, id: &str) -> anyhow::Result<String> {
    let t = table(report, id)?;
    let mut out = format!("| {} |\n", t.columns.join(" | "));
    let align: Vec<&str> = t
        .rows
        .first()
        .context("a table with rows")?
        .iter()
        .map(|c| if c.value().is_some() { "---:" } else { "---" })
        .collect();
    out.push_str(&format!("|{}|\n", align.join("|")));
    for r in &t.rows {
        let cells: Vec<String> = r.iter().map(shown).collect();
        out.push_str(&format!("| {} |\n", cells.join(" | ")));
    }
    out.push_str(&format!(
        "\n`just bench-{}` on {}, load average {}.\n",
        report.bench.replace('_', "-"),
        report.machine.host,
        t.load()
    ));
    Ok(out)
}

/// The paragraph saying where every report ran: the machine, the build, the commit and the range of the load.
/// The reports the tables of "Sizes and speeds" draw from.
fn sizes_reports(r: &Reports) -> [&Report; 10] {
    [
        &r.drive,
        &r.sample_codecs,
        &r.event_times,
        &r.summaries,
        &r.chunks,
        &r.tracks,
        &r.carried,
        &r.memory,
        &r.rf_core,
        &r.rf_chain,
    ]
}

/// The reports the case pages "Every frame of a 10 Gbps link" and "A billion samples a second" draw from.
fn cases_reports(r: &Reports) -> [&Report; 2] {
    [&r.ethernet, &r.wideband]
}

/// The machine, build and commit the reports `all` ran, one run: the same for each, against the first.
fn machine(all: &[&Report]) -> anyhow::Result<String> {
    let first = all.first().context("a report")?;
    for x in all {
        ensure!(
            x.machine == first.machine && x.build == first.build && x.commit == first.commit,
            "bench {} ran on another machine, build or commit than bench {}: run `just bench-book` again",
            x.bench,
            first.bench
        );
    }
    let loads = all
        .iter()
        .flat_map(|x| &x.tables)
        .flat_map(|t| [t.load_before, t.load_after])
        .flatten()
        .map(|l| l.one);
    let lo = loads.clone().fold(f64::MAX, f64::min);
    let hi = loads.fold(0.0f64, f64::max);
    let m = &first.machine;
    let jobs = first
        .build
        .jobs
        .map_or_else(|| "the default jobs".to_owned(), |j| format!("{j} jobs"));
    Ok(format!(
        "The benches ran on {}, a {} with {} logical CPUs ({}, {}). Each was a {} build at opt-level {} of commit `{}`, \
         built with {jobs}. The load average ran from {lo:.2} to {hi:.2} across the tables.\n",
        m.host,
        if m.virtual_machine {
            "virtual machine"
        } else {
            "machine"
        },
        m.cpus,
        m.cpu,
        m.arch,
        first.build.profile,
        first.build.opt_level,
        first.commit,
    ))
}

/// Every chart and table, drawn from the reports in `dir`.
pub fn figures(dir: &Path) -> anyhow::Result<Vec<(String, String)>> {
    let r = read(dir)?;
    let mut out = vec![
        ("sample_codecs.svg".to_owned(), sample_codecs(&r)?),
        ("event_times.svg".to_owned(), event_times(&r)?),
        ("chunk_cost.svg".to_owned(), chunk_cost(&r)?),
        ("drive_sizes.svg".to_owned(), drive_sizes(&r)?),
        ("bench_machine.txt".to_owned(), machine(&sizes_reports(&r))?),
        (
            "bench_cases_machine.txt".to_owned(),
            machine(&cases_reports(&r))?,
        ),
    ];
    for (report, id) in [
        (&r.drive, "sizes"),
        (&r.drive, "items"),
        (&r.drive, "write"),
        (&r.sample_codecs, "codecs"),
        (&r.event_times, "codecs"),
        (&r.event_times, "ends"),
        (&r.summaries, "summaries"),
        (&r.summaries, "kernels"),
        (&r.chunks, "writer"),
        (&r.chunks, "timeline-0"),
        (&r.chunks, "timeline-4096"),
        (&r.tracks, "add"),
        (&r.tracks, "puts"),
        (&r.tracks, "idle"),
        (&r.carried, "pieces"),
        (&r.carried, "caps"),
        (&r.carried, "room"),
        (&r.carried, "state"),
        (&r.memory, "heap"),
        (&r.rf_core, "writer"),
        (&r.rf_core, "reader"),
        (&r.rf_chain, "chain"),
        (&r.rf_chain, "read"),
        (&r.ethernet, "writer"),
        (&r.wideband, "writer"),
        (&r.wideband, "overview"),
    ] {
        let name = format!("bench_{}_{id}.txt", report.bench);
        out.push((name, markdown(report, id)?));
    }
    Ok(out)
}
