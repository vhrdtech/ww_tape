//! What every bench binary of ww_tape shares: its common command line ([Common], [args]), its run ([Bench]): the
//! report of `ww_tape_bench_report` with the build, the machine and the load average of each table, and the timing
//! loop ([ns_per], [ns_per_try]).
//!
//! A bench parses its arguments with [args], which answers a shell completion request first. [Bench::start] takes
//! its settings and records the commit, the build and the machine. The bench fills one [Measure] per table and
//! hands each to [Bench::push], which takes the load average after it and gives the Markdown back to print.
//! [Bench::finish] writes the JSON that `--json` names. The book's charts and tables of measured numbers read those
//! files (`docs/bench/`), so every number there names the machine, the build and the load it was measured at.
//!
//! ```
//! use ww_tape_bench::{Bench, Cell, Common};
//!
//! let info = vc_build::Info { version: "0.1.0", sha: "abc1234", time: "1 Jan 2026 00:00 UTC" };
//! let common = Common { color: Default::default(), json: None };
//! let mut bench = Bench::start("demo", info, &common, vec![("chunks", "256".to_owned())]).unwrap();
//! let mut table = bench.table("cost", "What a chunk costs", &["What", "ns per chunk"]);
//! table.row(vec![Cell::text("a copy"), Cell::num(53_000.0, 0)]);
//! let markdown = bench.push(table);
//! assert!(markdown.contains("| a copy | 53000 |"));
//! assert_eq!(bench.report().tables[0].rows.len(), 1);
//! ```
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    reason = "benches compute times, sizes and rates of data they make themselves, far from any limit"
)]

mod machine;
mod rng;

pub use machine::{build, load, machine};
pub use rng::Rng;
pub use ww_tape_bench_report::{Build, Cell, FORMAT, Load, Machine, Report, Table};

use anyhow::Context as _;
use std::fmt::Write as _;
use std::hint::black_box;
use std::path::PathBuf;
use std::time::Instant;
use vc_ui::Paint;

/// The options every bench takes.
#[derive(Clone, Debug, Default, clap::Args)]
pub struct Common {
    #[command(flatten)]
    pub color: vc_ui::ColorFlag,
    /// Write the report as JSON to FILE as well: the build, the machine, the settings, every table with its load
    /// average.
    #[arg(long, value_name = "FILE", value_hint = clap::ValueHint::FilePath)]
    pub json: Option<PathBuf>,
}

/// The command line of bench `A`: a shell completion request is answered first (`COMPLETE=bash BIN`), and a closed
/// pipe ends the bench quietly (`bench | head`).
pub fn args<A: clap::Parser>() -> A {
    vc_ui::restore_sigpipe();
    vc_complete::complete(<A as clap::CommandFactory>::command);
    A::parse()
}

/// Best of `rounds`, ns per call of `f` over `iters` calls `f(0..iters)`; what `f` returns is summed, so the
/// compiler keeps the work.
pub fn ns_per(rounds: u32, iters: usize, mut f: impl FnMut(usize) -> u64) -> f64 {
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

/// [ns_per] of a call that may fail: the first error ends the timing.
pub fn ns_per_try(
    rounds: u32,
    iters: usize,
    mut f: impl FnMut(usize) -> anyhow::Result<()>,
) -> anyhow::Result<f64> {
    let mut best = f64::MAX;
    for _ in 0..rounds {
        let t = Instant::now();
        for i in 0..iters {
            f(i)?;
        }
        best = best.min(t.elapsed().as_nanos() as f64 / iters as f64);
    }
    Ok(best)
}

/// A table being measured: its rows, and the time it started.
pub struct Measure {
    pub table: Table,
    started: Instant,
}

impl Measure {
    /// Add a row: one cell per column ([Table::row]).
    ///
    /// # Panics
    ///
    /// When `cells` has another count than the columns: a bug of the bench.
    pub fn row(&mut self, cells: Vec<Cell>) {
        self.table.row(cells);
    }
}

/// A bench run: its report so far, how it prints, and where its JSON goes.
pub struct Bench {
    report: Report,
    paint: Paint,
    json: Option<PathBuf>,
}

impl Bench {
    /// Start the report of bench `name`, built as `info` says (`vc_build::build_info!()`), with `settings`.
    ///
    /// Applies `--color`. Fails when `--json`'s directory doesn't exist, before any measuring.
    pub fn start(
        name: &str,
        info: vc_build::Info,
        common: &Common,
        settings: Vec<(&str, String)>,
    ) -> anyhow::Result<Self> {
        common.color.apply();
        if let Some(path) = &common.json {
            let dir = path
                .parent()
                .filter(|d| !d.as_os_str().is_empty())
                .unwrap_or_else(|| std::path::Path::new("."));
            anyhow::ensure!(
                dir.is_dir(),
                "--json {}: no directory {}",
                path.display(),
                dir.display()
            );
        }
        let report = Report {
            format: FORMAT,
            bench: name.to_owned(),
            version: info.line(),
            commit: info.sha.trim_end_matches("-dirty").to_owned(),
            dirty: info.sha.ends_with("-dirty"),
            build: build(),
            settings: settings
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v))
                .collect(),
            machine: machine(),
            tables: Vec::new(),
        };
        Ok(Self {
            report,
            paint: Paint::stdout(),
            json: common.json.clone(),
        })
    }

    /// The report so far.
    pub fn report(&self) -> &Report {
        &self.report
    }

    /// The report's first lines: the bench, its settings, the machine and the build; a warning where its times mean
    /// nothing.
    pub fn header(&self) -> String {
        let r = &self.report;
        let p = self.paint;
        let settings: Vec<String> = r.settings.iter().map(|(k, v)| format!("{k} {v}")).collect();
        let jobs = r
            .build
            .jobs
            .map_or_else(|| "jobs unset".to_owned(), |j| format!("{j} jobs"));
        let mut out = format!(
            "{} {}: {}.\n{}\n{}\n",
            p.paint(vc_ui::HEADING, &r.bench),
            r.version,
            settings.join(", "),
            p.dim(&r.machine.to_string()),
            p.dim(&format!(
                "{} build at opt-level {}, {jobs}.",
                r.build.profile, r.build.opt_level
            )),
        );
        if let Err(e) = r.measured() {
            _ = writeln!(out, "{}", p.warn(&format!("{e}.")));
        }
        out
    }

    /// A line saying a part of the bench was skipped, and why.
    pub fn skipped(&self, what: &str, why: &str) -> String {
        self.paint.warn(&format!("{what}: skipped, {why}."))
    }

    /// A new table `id` with `columns`; the load average is taken now.
    pub fn table(&self, id: &str, title: &str, columns: &[&str]) -> Measure {
        let mut table = Table::new(id, title, columns);
        table.load_before = load();
        Measure {
            table,
            started: Instant::now(),
        }
    }

    /// Keep `m`'s table, taking the load average after it; its Markdown, to print.
    pub fn push(&mut self, m: Measure) -> String {
        let mut table = m.table;
        table.load_after = load();
        table.seconds = m.started.elapsed().as_secs_f64();
        let md = markdown(&table, self.paint);
        self.report.tables.push(table);
        md
    }

    /// Write the report as JSON to the `--json` file, when given.
    pub fn finish(self) -> anyhow::Result<()> {
        if let Some(path) = &self.json {
            std::fs::write(path, self.report.to_json()?)
                .with_context(|| format!("--json {}", path.display()))?;
        }
        Ok(())
    }
}

/// `table` as Markdown: its title, its load average, then the rows.
fn markdown(table: &Table, paint: Paint) -> String {
    let mut out = String::new();
    _ = writeln!(out, "{}", paint.paint(vc_ui::HEADING, &table.title));
    _ = writeln!(
        out,
        "{}",
        paint.dim(&format!("Load average {}.", table.load()))
    );
    out.push('\n');
    _ = writeln!(out, "| {} |", table.columns.join(" | "));
    let align: Vec<&str> = table
        .rows
        .first()
        .map(|r| {
            r.iter()
                .map(|c| if c.value().is_some() { "---:" } else { "---" })
                .collect()
        })
        .unwrap_or_default();
    _ = writeln!(out, "|{}|", align.join("|"));
    for r in &table.rows {
        let cells: Vec<String> = r.iter().map(ToString::to_string).collect();
        _ = writeln!(out, "| {} |", cells.join(" | "));
    }
    out
}
