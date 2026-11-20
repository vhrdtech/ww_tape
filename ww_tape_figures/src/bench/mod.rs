//! The charts and tables of "Sizes and speeds", drawn from the bench reports in `docs/bench/` (`just bench-book`):
//!
//! - `sample_codecs.svg`: ratio and decode time per data set, fixed point against `ALP` and the best other float codec.
//! - `event_times.svg`: bytes per event and the time to find a tick, per workload and codec.
//! - `chunk_cost.svg`: what the writer spends on a 1 MiB series chunk, by part.
//! - `drive_sizes.svg`: where the bytes of the drive's hour go.
//! - `bench_*.txt`: the tables the page includes, each with the machine and the load average it ran at.

mod chart;

use anyhow::{Context, ensure};
use chart::{Chart, Panel, Series};
use std::path::Path;
use ww_tape_bench::{Cell, Report, Table};

/// The bench reports, read from `dir`.
struct Reports {
    sample_codecs: Report,
    event_times: Report,
    summaries: Report,
    chunks: Report,
    tracks: Report,
    drive: Report,
}

fn read(dir: &Path) -> anyhow::Result<Reports> {
    let r = |name: &str| {
        Report::read(&dir.join(format!("{name}.json")))
            .with_context(|| format!("docs/bench/{name}.json: run `just bench-book`"))
    };
    Ok(Reports {
        sample_codecs: r("sample_codecs")?,
        event_times: r("event_times")?,
        summaries: r("summaries")?,
        chunks: r("chunks")?,
        tracks: r("tracks")?,
        drive: r("drive")?,
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
        .columns
        .iter()
        .position(|n| n == column)
        .with_context(|| format!("column {column} of table {}", t.id))?;
    row.get(c)
        .and_then(Cell::value)
        .with_context(|| format!("a number in column {column} of table {}", t.id))
}

fn text(row: &[Cell], at: usize) -> &str {
    row.get(at).and_then(Cell::as_text).unwrap_or("")
}

/// The distinct first cells of `t`'s rows, in order.
fn groups(t: &Table) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in &t.rows {
        let g = text(r, 0);
        if !out.iter().any(|o| o == g) {
            out.push(g.to_owned());
        }
    }
    out
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

const FLOAT_CODECS: [&str; 4] = ["GORILLA", "CHIMP", "FLOAT_DELTA", "FLOAT_DELTA2"];

fn sample_codecs(r: &Reports) -> anyhow::Result<String> {
    let t = table(&r.sample_codecs, "codecs")?;
    let sets = groups(t);
    let mut ratio = Vec::new();
    let mut decode = Vec::new();
    for set in &sets {
        let rows: Vec<&Vec<Cell>> = t.rows.iter().filter(|r| text(r, 0) == set).collect();
        let fixed = rows
            .iter()
            .find(|r| text(r, 1).starts_with("DELTA of"))
            .context("the fixed-point row")?;
        let alp = rows
            .iter()
            .find(|r| text(r, 1) == "ALP")
            .context("the ALP row")?;
        let mut best = None::<(f64, f64)>;
        for row in rows.iter().filter(|r| FLOAT_CODECS.contains(&text(r, 1))) {
            let x = (
                value(t, row, "Ratio")?,
                value(t, row, "Decode ns per value")?,
            );
            if best.is_none_or(|b| x.0 > b.0) {
                best = Some(x);
            }
        }
        let best = best.context("the float codecs")?;
        ratio.push(vec![
            Some(value(t, fixed, "Ratio")?),
            Some(value(t, alp, "Ratio")?),
            Some(best.0),
        ]);
        decode.push(vec![
            Some(value(t, fixed, "Decode ns per value")?),
            Some(value(t, alp, "Decode ns per value")?),
            Some(best.1),
        ]);
    }
    Ok(Chart {
        label: "Sample codecs per data set: ratio to the raw floats and decode time per value, fixed point with DELTA against ALP and the best other float codec".to_owned(),
        rows: sets,
        series: series(&[
            ("fixed point, DELTA", "blue"),
            ("ALP", "accent"),
            ("best other float codec", "amber"),
        ]),
        panels: vec![
            Panel {
                axis: "Ratio to the raw floats (larger is smaller)".to_owned(),
                log: false,
                digits: 2,
                values: ratio,
            },
            Panel {
                axis: "Decode, ns per value".to_owned(),
                log: false,
                digits: 2,
                values: decode,
            },
        ],
    }
    .svg())
}

const EVENT_CODECS: [(&str, &str, &str); 4] = [
    ("ww_tape linear groups", "ww_tape linear groups", "blue"),
    ("delta-of-delta varints", "delta-of-delta varints", "amber"),
    ("Elias-Fano", "Elias-Fano", "green"),
    ("raw 64-bit ticks", "raw 64-bit ticks", "tint-ink"),
];

fn event_times(r: &Reports) -> anyhow::Result<String> {
    let t = table(&r.event_times, "codecs")?;
    let workloads = groups(t);
    let mut bytes = Vec::new();
    let mut find = Vec::new();
    for w in &workloads {
        let mut b = Vec::new();
        let mut f = Vec::new();
        for (codec, _, _) in EVENT_CODECS {
            let row = t
                .rows
                .iter()
                .find(|r| text(r, 0) == w && text(r, 1) == codec)
                .with_context(|| format!("{w}, {codec}"))?;
            b.push(Some(value(t, row, "Bytes per event")?));
            f.push(Some(value(t, row, "First at or after a tick ns")?));
        }
        bytes.push(b);
        find.push(f);
    }
    let names: Vec<(&str, &'static str)> = EVENT_CODECS
        .iter()
        .map(|&(_, name, class)| (name, class))
        .collect();
    Ok(Chart {
        label: "Event start ticks per workload: bytes per event on a log scale, and the time to find the first event at or after a tick, for ww_tape's linear groups, delta-of-delta varints, Elias-Fano and raw ticks".to_owned(),
        rows: workloads,
        series: series(&names),
        panels: vec![
            Panel {
                axis: "Bytes per event (log scale)".to_owned(),
                log: true,
                digits: 3,
                values: bytes,
            },
            Panel {
                axis: "First at or after a tick, ns".to_owned(),
                log: false,
                digits: 0,
                values: find,
            },
        ],
    }
    .svg())
}

fn chunk_cost(r: &Reports) -> anyhow::Result<String> {
    let t = table(&r.chunks, "writer")?;
    let mut rows = Vec::new();
    let mut us = Vec::new();
    for row in &t.rows {
        rows.push(plain(text(row, 0)));
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
        }],
    }
    .svg())
}

fn drive_sizes(r: &Reports) -> anyhow::Result<String> {
    let t = table(&r.drive, "sizes")?;
    let mut rows = Vec::new();
    let mut mib = Vec::new();
    for row in t.rows.iter().filter(|r| text(r, 0) != "the recording") {
        let bytes = value(t, row, "Bytes")?;
        if bytes == 0.0 {
            continue;
        }
        rows.push(plain(text(row, 0)));
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
        }],
    }
    .svg())
}

/// A number as a page shows it: whole numbers of five digits or more in groups of three, split by spaces.
fn shown(c: &Cell) -> String {
    match c {
        Cell::Num { value, digits: 0 } if value.abs() >= 10_000.0 => {
            let s = format!("{value:.0}");
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
        c => c.to_string(),
    }
}

/// Table `id` of `report` as Markdown a page includes, then the line saying where and at what load it ran.
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
    let load = |l: Option<ww_tape_bench::Load>| {
        l.map_or_else(|| "unknown".to_owned(), |l| format!("{:.2}", l.one))
    };
    let m = &report.machine;
    out.push_str(&format!(
        "\n`just bench-{}` on an {} ({} logical CPUs), load average {} before and {} after.\n",
        report.bench.replace('_', "-"),
        m.cpu,
        m.cpus,
        load(t.load_before),
        load(t.load_after)
    ));
    Ok(out)
}

/// Every chart and table, drawn from the reports in `dir`.
pub fn figures(dir: &Path) -> anyhow::Result<Vec<(String, String)>> {
    let r = read(dir)?;
    let mut out = vec![
        ("sample_codecs.svg".to_owned(), sample_codecs(&r)?),
        ("event_times.svg".to_owned(), event_times(&r)?),
        ("chunk_cost.svg".to_owned(), chunk_cost(&r)?),
        ("drive_sizes.svg".to_owned(), drive_sizes(&r)?),
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
    ] {
        let name = format!("bench_{}_{id}.txt", report.bench);
        out.push((name, markdown(report, id)?));
    }
    ensure!(!out.is_empty(), "figures drawn");
    Ok(out)
}
