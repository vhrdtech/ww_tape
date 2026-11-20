//! What every bench binary of ww_tape shares: its common command line ([Common]), the machine and the load it ran
//! at ([Machine], [Load]), and its report ([Report]): tables shown as Markdown and written as JSON.
//!
//! A bench parses its arguments with [args], which answers a shell completion request first. It starts a [Report]
//! with its settings, fills one [Table] per measurement and hands each to [Report::push], which takes the load
//! average after it and gives the Markdown back to print. [Report::finish] writes the JSON that `--json` names. The
//! book's charts and tables of measured numbers read those files (`docs/bench/`), so every number there names the
//! machine and the load it was measured at.
//!
//! ```
//! use ww_tape_bench::{Cell, Common, Machine, Report};
//!
//! let mut report = Report::new("demo", "0.1.0", Machine::this(), vec![("chunks", "256".to_owned())]);
//! let mut table = report.table("cost", "What a chunk costs", &["What", "ns per chunk"]);
//! table.row(vec![Cell::text("a copy"), Cell::num(53_000.0, 0)]);
//! let markdown = report.push(table, vc_ui::Paint::OFF);
//! assert!(markdown.contains("| a copy | 53000 |"));
//! assert_eq!(report.tables[0].rows.len(), 1);
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

pub use machine::{Load, Machine};
pub use rng::Rng;

use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;
use vc_ui::Paint;

/// The options every bench takes.
#[derive(Clone, Debug, Default, clap::Args)]
pub struct Common {
    #[command(flatten)]
    pub color: vc_ui::ColorFlag,
    /// Write the report as JSON to FILE as well: the settings, the machine, every table with its load average.
    #[arg(long, value_name = "FILE", value_hint = clap::ValueHint::FilePath)]
    pub json: Option<PathBuf>,
}

/// The command line of bench `A`: a shell completion request is answered first (`COMPLETE=bash BIN`), then the
/// arguments are parsed and the colour choice applied.
pub fn args<A: clap::Parser + AsRef<Common>>() -> A {
    vc_complete::complete(<A as clap::CommandFactory>::command);
    let a = A::parse();
    a.as_ref().color.apply();
    a
}

/// One value of a table: text, or a number shown with a fixed count of decimals.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Cell {
    Num { value: f64, digits: u8 },
    Text(String),
}

impl Cell {
    /// A text cell.
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text(s.into())
    }

    /// A number shown with `digits` decimals.
    pub fn num(value: f64, digits: u8) -> Self {
        Self::Num { value, digits }
    }

    /// A count, shown without decimals.
    pub fn count(value: u64) -> Self {
        Self::Num {
            value: value as f64,
            digits: 0,
        }
    }

    /// The number, None for text.
    pub fn value(&self) -> Option<f64> {
        match self {
            Self::Num { value, .. } => Some(*value),
            Self::Text(_) => None,
        }
    }

    /// The text, None for a number.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(s) => Some(s),
            Self::Num { .. } => None,
        }
    }
}

impl core::fmt::Display for Cell {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Num { value, digits } => write!(f, "{value:.*}", usize::from(*digits)),
            Self::Text(s) => f.write_str(s),
        }
    }
}

/// One measurement: its columns and rows, and the load average around it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Table {
    /// The name the book's figures look the table up by.
    pub id: String,
    /// What the table measures, with its settings.
    pub title: String,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Cell>>,
    /// The load average when the table started.
    pub load_before: Option<Load>,
    /// The load average when the table was done.
    pub load_after: Option<Load>,
    /// Seconds the table took.
    pub seconds: f64,
    #[serde(skip)]
    started: Option<Instant>,
}

impl Table {
    /// Add a row: one cell per column.
    ///
    /// # Panics
    ///
    /// When `cells` has another count than the columns: a bug of the bench.
    pub fn row(&mut self, cells: Vec<Cell>) {
        assert_eq!(
            cells.len(),
            self.columns.len(),
            "a row of table {} has a cell per column",
            self.id
        );
        self.rows.push(cells);
    }

    /// The cell of `column` in the row whose first cell is `key`.
    pub fn cell(&self, key: &str, column: &str) -> Option<&Cell> {
        let c = self.columns.iter().position(|n| n == column)?;
        self.rows
            .iter()
            .find(|r| r.first().and_then(Cell::as_text) == Some(key))?
            .get(c)
    }

    /// The table as Markdown: its title, its load average, then the rows.
    pub fn markdown(&self, paint: Paint) -> String {
        let mut out = String::new();
        _ = writeln!(out, "{}", paint.paint(vc_ui::HEADING, &self.title));
        _ = writeln!(
            out,
            "{}",
            paint.dim(&format!(
                "Load average {}.",
                Load::span(self.load_before, self.load_after)
            ))
        );
        out.push('\n');
        _ = writeln!(out, "| {} |", self.columns.join(" | "));
        let align: Vec<&str> = self
            .rows
            .first()
            .map(|r| {
                r.iter()
                    .map(|c| if c.value().is_some() { "---:" } else { "---" })
                    .collect()
            })
            .unwrap_or_default();
        _ = writeln!(out, "|{}|", align.join("|"));
        for r in &self.rows {
            let cells: Vec<String> = r.iter().map(ToString::to_string).collect();
            _ = writeln!(out, "| {} |", cells.join(" | "));
        }
        out
    }
}

/// A bench run: its settings, its machine and the tables it measured.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Report {
    /// The bench's binary name.
    pub bench: String,
    /// Its `--version` line.
    pub version: String,
    /// Its settings, as named on the command line.
    pub settings: Vec<(String, String)>,
    pub machine: Machine,
    pub tables: Vec<Table>,
}

impl Report {
    /// A report of `bench` at `version`, run on `machine` with `settings`.
    pub fn new(
        bench: &str,
        version: &str,
        machine: Machine,
        settings: Vec<(&str, String)>,
    ) -> Self {
        Self {
            bench: bench.to_owned(),
            version: version.to_owned(),
            settings: settings
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v))
                .collect(),
            machine,
            tables: Vec::new(),
        }
    }

    /// The report's first lines as Markdown: the bench, its settings and the machine.
    pub fn header(&self, paint: Paint) -> String {
        let settings: Vec<String> = self
            .settings
            .iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect();
        format!(
            "{} {}: {}.\n{}\n",
            paint.paint(vc_ui::HEADING, &self.bench),
            self.version,
            settings.join(", "),
            paint.dim(&self.machine.to_string())
        )
    }

    /// A new table `id` with `columns`; the load average is taken now.
    pub fn table(&self, id: &str, title: &str, columns: &[&str]) -> Table {
        Table {
            id: id.to_owned(),
            title: title.to_owned(),
            columns: columns.iter().map(|&c| c.to_owned()).collect(),
            rows: Vec::new(),
            load_before: Load::now(),
            load_after: None,
            seconds: 0.0,
            started: Some(Instant::now()),
        }
    }

    /// Keep `table`, taking the load average after it; its Markdown, to print.
    pub fn push(&mut self, mut table: Table, paint: Paint) -> String {
        table.load_after = Load::now();
        table.seconds = table
            .started
            .take()
            .map_or(0.0, |t| t.elapsed().as_secs_f64());
        let md = table.markdown(paint);
        self.tables.push(table);
        md
    }

    /// The table `id`.
    pub fn get(&self, id: &str) -> Option<&Table> {
        self.tables.iter().find(|t| t.id == id)
    }

    /// Write the report as JSON to `path`, when given.
    pub fn finish(&self, path: Option<&Path>) -> anyhow::Result<()> {
        if let Some(path) = path {
            let mut json = serde_json::to_string_pretty(self)?;
            json.push('\n');
            std::fs::write(path, json)?;
        }
        Ok(())
    }

    /// The report a bench wrote to `path`.
    pub fn read(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&text)?)
    }
}
