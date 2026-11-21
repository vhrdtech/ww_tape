//! Horizontal bar charts of bench numbers: one or more panels side by side over the same rows, each its own scale (a
//! small multiple, never two scales on one axis). A bar carries its value at its end and as a tooltip.

use crate::svg::Svg;

/// One series: its name in the legend and the class its bars take.
pub struct Series {
    pub name: String,
    pub class: &'static str,
}

/// A panel: what its axis measures and each row's value per series (None: no bar), with an optional word after a
/// bar's value (`tags`, by row and series; empty: none).
pub struct Panel {
    pub axis: String,
    pub log: bool,
    pub digits: usize,
    pub values: Vec<Vec<Option<f64>>>,
    pub tags: Vec<Vec<Option<String>>>,
}

/// A chart: its rows, its series and its panels, and a note under the legend (where and at what load it ran).
pub struct Chart {
    pub label: String,
    pub rows: Vec<String>,
    pub series: Vec<Series>,
    pub panels: Vec<Panel>,
    pub note: String,
}

const BAR: f64 = 12.0;
const BAR_GAP: f64 = 2.0;
const ROW_GAP: f64 = 14.0;
const PANEL: f64 = 230.0;
const PANEL_GAP: f64 = 60.0;
const TOP: f64 = 40.0;
/// The width of a character of a row label, at the figures' 13 px.
const CHAR: f64 = 6.6;
/// The width of a character of small text.
const SMALL_CHAR: f64 = 6.0;

/// `v` with `digits` decimals, or with 3 significant digits where `digits` would round it to 0.
pub fn fmt(v: f64, digits: usize) -> String {
    let unit = libm::pow(10.0, -(digits as f64));
    if v != 0.0 && v.abs() < unit / 2.0 {
        let d = (2.0 - libm::floor(libm::log10(v.abs()))) as usize;
        format!("{v:.d$}")
    } else {
        format!("{v:.digits$}")
    }
}

/// The range a panel's axis spans: from 0, or from a power of ten below its smallest value on a log axis.
fn range(p: &Panel) -> (f64, f64) {
    let vals = p.values.iter().flatten().flatten().copied();
    let max = vals.clone().fold(0.0f64, f64::max);
    if p.log {
        let min = vals.filter(|v| *v > 0.0).fold(f64::MAX, f64::min);
        let lo = libm::pow(10.0, libm::floor(libm::log10(min)));
        let hi = libm::pow(10.0, libm::ceil(libm::log10(max)));
        (lo, hi.max(lo * 10.0))
    } else {
        (0.0, nice(max))
    }
}

/// The smallest of 1, 2 or 5 times a power of ten at or above `v`.
fn nice(v: f64) -> f64 {
    if v <= 0.0 {
        return 1.0;
    }
    let p = libm::pow(10.0, libm::floor(libm::log10(v)));
    [1.0, 2.0, 5.0, 10.0]
        .into_iter()
        .map(|m| m * p)
        .find(|n| *n >= v)
        .unwrap_or(10.0 * p)
}

/// Where value `v` falls on a panel `PANEL` wide over `(lo, hi)`.
fn at(v: f64, (lo, hi): (f64, f64), log: bool) -> f64 {
    let f = if log {
        if v <= lo {
            0.0
        } else {
            libm::log10(v / lo) / libm::log10(hi / lo)
        }
    } else {
        v / hi
    };
    f.clamp(0.0, 1.0) * PANEL
}

/// The ticks of an axis over `(lo, hi)`: each power of ten on a log axis, five steps on a linear one.
fn ticks((lo, hi): (f64, f64), log: bool) -> Vec<f64> {
    if log {
        let mut t = vec![lo];
        while t.last().is_some_and(|x| *x < hi * 0.999) {
            t.push(t.last().map_or(hi, |x| x * 10.0));
        }
        t
    } else {
        (0..=5).map(|i| hi * f64::from(i) / 5.0).collect()
    }
}

fn tick_label(v: f64) -> String {
    if v >= 1.0 || v == 0.0 {
        format!("{v}")
    } else {
        let d = (-libm::floor(libm::log10(v))) as usize;
        format!("{v:.d$}")
    }
}

impl Chart {
    /// The chart as SVG.
    pub fn svg(&self) -> String {
        let n = self.series.len() as f64;
        let row_h = n * BAR + (n - 1.0) * BAR_GAP + ROW_GAP;
        let plot_h = row_h * self.rows.len() as f64;
        let panels = self.panels.len() as f64;
        let longest = self
            .rows
            .iter()
            .map(|r| r.chars().count())
            .max()
            .unwrap_or(0);
        let labels = CHAR * longest as f64 + 14.0;
        let width = (labels + panels * PANEL + (panels - 1.0) * PANEL_GAP + 50.0)
            .max(labels + SMALL_CHAR * self.note.chars().count() as f64 + 10.0);
        let height = TOP + plot_h + 84.0;
        let mut s = Svg::new(width, height, &self.label);
        for (k, p) in self.panels.iter().enumerate() {
            let x0 = labels + k as f64 * (PANEL + PANEL_GAP);
            let r = range(p);
            s.text(x0, 16.0, "start", "strong", &p.axis);
            let base = TOP + plot_h;
            for t in ticks(r, p.log) {
                let x = x0 + at(t, r, p.log);
                s.line((x, TOP - 6.0), (x, base), "thin");
                s.text(x, base + 14.0, "middle", "small soft", &tick_label(t));
            }
            for (i, vals) in p.values.iter().enumerate() {
                let y_row = TOP + i as f64 * row_h;
                for (j, v) in vals.iter().enumerate() {
                    let (Some(v), Some(series)) = (v, self.series.get(j)) else {
                        continue;
                    };
                    let y = y_row + j as f64 * (BAR + BAR_GAP);
                    let w = at(*v, r, p.log).max(1.0);
                    let label = match p.tags.get(i).and_then(|t| t.get(j)).cloned().flatten() {
                        Some(tag) => format!("{} {tag}", fmt(*v, p.digits)),
                        None => fmt(*v, p.digits),
                    };
                    let row = self.rows.get(i).map_or("", String::as_str);
                    s.rect_titled(
                        (x0, y),
                        (w, BAR),
                        series.class,
                        &format!("{row}, {}: {label}", series.name),
                    );
                    s.text(x0 + w + 4.0, y + BAR - 2.0, "start", "small", &label);
                }
            }
            s.line((x0, TOP - 6.0), (x0, base), "line");
        }
        for (i, row) in self.rows.iter().enumerate() {
            let y = TOP + i as f64 * row_h + (row_h - ROW_GAP) / 2.0 + 4.0;
            s.text(labels - 10.0, y, "end", "", row);
        }
        // the legend, below the axes
        let mut x = labels;
        let y = TOP + plot_h + 36.0;
        for series in &self.series {
            s.rect((x, y - 9.0), (12.0, 10.0), 2.0, series.class);
            s.text(x + 17.0, y, "start", "small", &series.name);
            x += 30.0 + 7.0 * series.name.chars().count() as f64;
        }
        s.text(labels, y + 22.0, "start", "small soft", &self.note);
        s.finish()
    }
}
