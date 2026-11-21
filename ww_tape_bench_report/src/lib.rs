//! The report of a ww_tape bench run, as its JSON holds it: the bench, the build and commit it ran, the machine, and
//! its tables with the load average around each ([Report], [Table], [Cell], [Load], [Machine], [Build]).
//!
//! Types only: `ww_tape_bench` measures and writes a report, `ww_tape_figures` reads one back to draw the book's
//! charts and tables. A reader tolerates fields it doesn't know; [FORMAT] counts changes a reader must know.
//!
//! ```
//! use ww_tape_bench_report::{Cell, Table};
//!
//! let mut table = Table::new("cost", "What a chunk costs", &["What", "ns per chunk"]);
//! table.row(vec![Cell::text("a copy"), Cell::num(53_000.0, 0)]);
//! assert_eq!(table.cell("a copy", "ns per chunk"), Some(&Cell::num(53_000.0, 0)));
//! ```

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use std::io::Read as _;
use std::path::Path;

/// The shape of a report: a reader refuses another one.
pub const FORMAT: u32 = 1;

/// The largest report [Report::read] takes: the book's largest is under 64 KiB.
pub const REPORT_BYTES_MAX: u64 = 16 << 20;

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
    #[expect(
        clippy::as_conversions,
        clippy::cast_precision_loss,
        reason = "a count of a bench stays far below 2^53"
    )]
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

/// The load average: runnable tasks averaged over 1, 5 and 15 minutes (`/proc/loadavg`).
#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Load {
    pub one: f64,
    pub five: f64,
    pub fifteen: f64,
}

impl Load {
    /// The load average of `/proc/loadavg`'s text; None when it doesn't start with three numbers.
    ///
    /// ```
    /// let l = ww_tape_bench_report::Load::parse("5.62 4.10 3.99 2/1311 90210\n").unwrap();
    /// assert_eq!((l.one, l.five, l.fifteen), (5.62, 4.10, 3.99));
    /// ```
    pub fn parse(text: &str) -> Option<Self> {
        let mut v = text.split_whitespace().map(str::parse::<f64>);
        Some(Self {
            one: v.next()?.ok()?,
            five: v.next()?.ok()?,
            fifteen: v.next()?.ok()?,
        })
    }

    /// `5.62 before, 5.80 after` of the 1-minute averages, `unknown` where one is missing.
    pub fn span(before: Option<Self>, after: Option<Self>) -> String {
        let one =
            |l: Option<Self>| l.map_or_else(|| "unknown".to_owned(), |l| format!("{:.2}", l.one));
        format!("{} before, {} after", one(before), one(after))
    }
}

/// What a bench ran on.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Machine {
    /// The host name.
    pub host: String,
    /// Whether the CPU says it runs under a hypervisor.
    pub virtual_machine: bool,
    /// The CPU's model name, as the system reports it.
    pub cpu: String,
    /// Logical CPUs the bench could use.
    pub cpus: usize,
    /// `x86_64`, `aarch64`, ...
    pub arch: String,
    /// The vector and CRC extensions the kernels of ww_tape may pick.
    pub extensions: Vec<String>,
}

impl core::fmt::Display for Machine {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let kind = if self.virtual_machine {
            "virtual machine"
        } else {
            "machine"
        };
        write!(
            f,
            "{}, a {kind} with {} logical CPUs ({}, {}: {})",
            self.host,
            self.cpus,
            self.cpu,
            self.arch,
            self.extensions.join(", ")
        )
    }
}

/// How the bench was built.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Build {
    /// Cargo's profile kind: `release`, or `debug` (whose times mean nothing).
    pub profile: String,
    /// The optimization level the bench's code was built at.
    pub opt_level: String,
    /// The build's parallel jobs (`CARGO_BUILD_JOBS`, which `just jobs=N` sets); None where unset.
    pub jobs: Option<u32>,
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
}

impl Table {
    /// An empty table `id` with `columns`, no load average and no time.
    pub fn new(id: &str, title: &str, columns: &[&str]) -> Self {
        Self {
            id: id.to_owned(),
            title: title.to_owned(),
            columns: columns.iter().map(|&c| c.to_owned()).collect(),
            rows: Vec::new(),
            load_before: None,
            load_after: None,
            seconds: 0.0,
        }
    }

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

    /// The index of `column`.
    pub fn column(&self, column: &str) -> Option<usize> {
        self.columns.iter().position(|n| n == column)
    }

    /// The cell of `column` in the row whose first cell is `key`.
    pub fn cell(&self, key: &str, column: &str) -> Option<&Cell> {
        let c = self.column(column)?;
        self.rows
            .iter()
            .find(|r| r.first().and_then(Cell::as_text) == Some(key))?
            .get(c)
    }

    /// `Load::span` of this table.
    pub fn load(&self) -> String {
        Load::span(self.load_before, self.load_after)
    }
}

/// A bench run: its build, its settings, its machine and the tables it measured.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Report {
    /// [FORMAT] when written.
    pub format: u32,
    /// The bench's binary name.
    pub bench: String,
    /// Its `--version` line.
    pub version: String,
    /// The commit it was built from, short; `unknown` without git.
    pub commit: String,
    /// Whether the tree had changes the commit doesn't hold.
    pub dirty: bool,
    pub build: Build,
    /// Its settings, as named on the command line.
    pub settings: Vec<(String, String)>,
    pub machine: Machine,
    pub tables: Vec<Table>,
}

impl Report {
    /// The table `id`.
    pub fn get(&self, id: &str) -> Option<&Table> {
        self.tables.iter().find(|t| t.id == id)
    }

    /// The value of setting `name`.
    pub fn setting(&self, name: &str) -> Option<&str> {
        self.settings
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// Whether its times mean what they say: a release build of a commit, with no changes beside it.
    pub fn measured(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.build.profile == "release",
            "bench {} is a {} build: its times mean nothing",
            self.bench,
            self.build.profile
        );
        anyhow::ensure!(
            !self.dirty && self.commit != "unknown",
            "bench {} ran {} with changes beside it: no commit holds its code",
            self.bench,
            self.commit
        );
        Ok(())
    }

    /// The report as JSON, ending in a newline.
    pub fn to_json(&self) -> anyhow::Result<String> {
        let mut json = serde_json::to_string_pretty(self)?;
        json.push('\n');
        Ok(json)
    }

    /// The report a bench wrote to `path`, of [FORMAT] and at most [REPORT_BYTES_MAX] bytes.
    pub fn read(path: &Path) -> anyhow::Result<Self> {
        let at = || format!("bench report {}", path.display());
        let mut text = String::new();
        std::fs::File::open(path)
            .with_context(at)?
            .take(REPORT_BYTES_MAX.saturating_add(1))
            .read_to_string(&mut text)
            .with_context(at)?;
        anyhow::ensure!(
            u64::try_from(text.len()).is_ok_and(|n| n <= REPORT_BYTES_MAX),
            "{}: over {REPORT_BYTES_MAX} bytes",
            at()
        );
        let report: Self = serde_json::from_str(&text).with_context(at)?;
        anyhow::ensure!(
            report.format == FORMAT,
            "{}: format {}, this reader takes {FORMAT}",
            at(),
            report.format
        );
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[should_panic(expected = "a row of table t has a cell per column")]
    fn a_row_has_a_cell_per_column() {
        let mut t = Table::new("t", "T", &["a", "b"]);
        t.row(vec![Cell::text("x")]);
    }

    #[test]
    fn load_parse_refuses_what_is_not_loadavg() {
        assert_eq!(Load::parse(""), None);
        assert_eq!(Load::parse("1.0 x 2.0"), None);
        assert_eq!(
            Load::span(Load::parse("0.5 1 2"), None),
            "0.50 before, unknown after"
        );
    }

    #[test]
    fn json_round_trip() {
        let mut t = Table::new("t", "T", &["What", "ns"]);
        t.row(vec![Cell::text("a"), Cell::num(1.25, 2)]);
        t.load_before = Load::parse("1 2 3");
        let r = Report {
            format: FORMAT,
            bench: "demo".to_owned(),
            version: "0.1.0 (abc1234, commit 1 Jan 2026 00:00 UTC)".to_owned(),
            commit: "abc1234".to_owned(),
            dirty: false,
            build: Build {
                profile: "release".to_owned(),
                opt_level: "3".to_owned(),
                jobs: Some(4),
            },
            settings: vec![("rounds".to_owned(), "3".to_owned())],
            machine: Machine {
                host: "h".to_owned(),
                virtual_machine: true,
                cpu: "c".to_owned(),
                cpus: 2,
                arch: "x86_64".to_owned(),
                extensions: vec!["avx2".to_owned()],
            },
            tables: vec![t],
        };
        let dir = std::env::temp_dir().join(format!("ww_tape_bench_report-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("demo.json");
        std::fs::write(&path, r.to_json().unwrap()).unwrap();
        assert_eq!(Report::read(&path).unwrap(), r);
        r.measured().unwrap();
        let mut dirty = r.clone();
        dirty.dirty = true;
        assert!(dirty.measured().is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
