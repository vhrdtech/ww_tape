//! A small SVG writer: boxes, lines and text styled by the book's classes (`ww-svg`, `line`, `thin`, `faint`,
//! `accent`, `blue`, ... in the WireWeaver book's `figures.css`), so a figure inlined into a page follows the light or
//! dark scheme.

use core::fmt::Write;

/// An SVG document being written.
pub struct Svg {
    out: String,
}

/// `s` with the characters XML gives a meaning escaped.
pub fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

impl Svg {
    /// A drawing `width` by `height` units, described by `label` for screen readers.
    pub fn new(width: f64, height: f64, label: &str) -> Self {
        let mut out = String::new();
        _ = write!(
            out,
            r#"<svg class="ww-svg" viewBox="0 0 {width} {height}" width="{width}" height="{height}" role="img" aria-label="{}" xmlns="http://www.w3.org/2000/svg">"#,
            escape(label)
        );
        out.push('\n');
        Svg { out }
    }

    /// The finished document.
    pub fn finish(mut self) -> String {
        self.out.push_str("</svg>\n");
        self.out
    }

    /// Text at `(x, y)`, anchored `start`, `middle` or `end`.
    pub fn text(&mut self, x: f64, y: f64, anchor: &str, class: &str, s: &str) {
        let class = if class.is_empty() {
            String::new()
        } else {
            format!(r#" class="{class}""#)
        };
        _ = writeln!(
            self.out,
            r#"<text x="{x:.1}" y="{y:.1}" text-anchor="{anchor}"{class}>{}</text>"#,
            escape(s)
        );
    }

    pub fn line(&mut self, a: (f64, f64), b: (f64, f64), class: &str) {
        _ = writeln!(
            self.out,
            r#"<line x1="{:.1}" y1="{:.1}" x2="{:.1}" y2="{:.1}" class="{class}"/>"#,
            a.0, a.1, b.0, b.1
        );
    }

    pub fn rect(&mut self, at: (f64, f64), size: (f64, f64), rx: f64, class: &str) {
        _ = writeln!(
            self.out,
            r#"<rect x="{:.1}" y="{:.1}" width="{:.1}" height="{:.1}" rx="{rx}" class="{class}"/>"#,
            at.0, at.1, size.0, size.1
        );
    }

    pub fn circle(&mut self, c: (f64, f64), r: f64, class: &str) {
        _ = writeln!(
            self.out,
            r#"<circle cx="{:.1}" cy="{:.1}" r="{r}" class="{class}"/>"#,
            c.0, c.1
        );
    }

    /// A path from SVG path data.
    pub fn path(&mut self, d: &str, class: &str) {
        _ = writeln!(self.out, r#"<path d="{d}" class="{class}"/>"#);
    }

    /// A bracket under `x0..x1` at `y`, ticks pointing up: what a label below it covers.
    pub fn bracket_under(&mut self, x0: f64, x1: f64, y: f64, class: &str) {
        self.path(&format!("M{x0:.1} {:.1} v4 H{x1:.1} v-4", y - 4.0), class);
    }

    /// A bracket over `x0..x1` at `y`, ticks pointing down.
    pub fn bracket_over(&mut self, x0: f64, x1: f64, y: f64, class: &str) {
        self.path(&format!("M{x0:.1} {:.1} v-4 H{x1:.1} v4", y + 4.0), class);
    }
}
