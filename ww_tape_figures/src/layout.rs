//! Byte layouts drawn as rows of byte cells: each byte its hex value, each field a tint, a bracket under its bytes,
//! its name and what it holds. `block.svg` and the layout figures of the format reference draw on it.

use anyhow::{Context, ensure};

use crate::svg::Svg;

/// A field: its first byte, its length, its name, what it holds, its colour class.
pub struct Field {
    pub at: usize,
    pub len: usize,
    pub name: String,
    pub value: String,
    pub class: &'static str,
}

impl Field {
    pub fn new(at: usize, len: usize, name: &str, value: String, class: &'static str) -> Self {
        Self {
            at,
            len,
            name: name.to_owned(),
            value,
            class,
        }
    }

    pub fn end(&self) -> usize {
        self.at + self.len
    }
}

/// A row: its first byte, one past its last, a title left of it (empty for none).
pub type Row<'a> = (usize, usize, &'a str);

/// The geometry of a drawing: cell width and height, the left margin, the top of the first row, the row pitch.
pub struct Grid {
    pub cell: f64,
    pub ch: f64,
    pub left: f64,
    pub top: f64,
    pub pitch: f64,
}

/// Width of a character of the small label text, for telling whether a label fits over its field.
const CHAR_W: f64 = 6.6;

impl Grid {
    /// The geometry of `block.svg`: 16 bytes a row, room left of the rows for a title.
    pub const BYTES: Self = Self {
        cell: 34.0,
        ch: 24.0,
        left: 92.0,
        top: 30.0,
        pitch: 88.0,
    };

    /// The drawing's width for rows of 16 bytes.
    pub fn width(&self) -> f64 {
        self.left + 16.0 * self.cell + 16.0
    }

    /// The top of row `r`.
    pub fn y(&self, r: usize) -> f64 {
        self.top + self.pitch * r as f64
    }

    /// The drawing's height for `rows` rows.
    pub fn height(&self, rows: usize) -> f64 {
        self.top + self.pitch * rows as f64 - 12.0
    }

    /// Draw `rows` of `bytes` into `s`, every byte coloured by the field holding it. `base` is the offset the labels
    /// left of the rows count from: the offset of `bytes[0]` in its block. Every byte of a row must lie in a field.
    ///
    /// A field over several rows has its labels in the row that holds most of its bytes. A field whose name is wider
    /// than its bytes, after one that is too, sets its labels one line lower, so two narrow fields side by side stay
    /// apart.
    pub fn draw(
        &self,
        s: &mut Svg,
        bytes: &[u8],
        base: usize,
        fields: &[Field],
        rows: &[Row<'_>],
    ) -> anyhow::Result<()> {
        for (r, &(from, to, title)) in rows.iter().enumerate() {
            ensure!(to - from <= 16, "a row holds at most 16 bytes");
            let y = self.y(r);
            s.text(
                self.left - 10.0,
                y + 16.5,
                "end",
                "soft mono",
                &(base + from).to_string(),
            );
            if !title.is_empty() {
                s.text(self.left - 40.0, y + 16.5, "end", "strong", title);
            }
            for i in from..to {
                let field = fields
                    .iter()
                    .find(|f| (f.at..f.end()).contains(&i))
                    .with_context(|| format!("byte {i} in a field"))?;
                let x = self.left + self.cell * (i - from) as f64;
                s.rect(
                    (x + 1.0, y),
                    (self.cell - 2.0, self.ch),
                    3.0,
                    &format!("tint-{}", field.class),
                );
                s.text(
                    x + self.cell / 2.0,
                    y + 16.5,
                    "middle",
                    "mono",
                    &format!("{:02x}", bytes.get(i).context("byte")?),
                );
            }
            let mut lowered = false;
            // a field over several rows has a bracket in each, its labels in the row holding most of its bytes
            for field in fields.iter().filter(|f| f.at < to && f.end() > from) {
                let start = field.at.max(from);
                let shown = field.end().min(to) - start;
                let x0 = self.left + self.cell * (start - from) as f64 + 2.0;
                let x1 = x0 + self.cell * shown as f64 - 4.0;
                let cx = (x0 + x1) / 2.0;
                if label_row(field, rows) != Some(r) {
                    s.bracket_under(x0, x1, y + self.ch + 6.0, field.class);
                    continue;
                }
                let label = field.name.chars().count().max(field.value.chars().count());
                let narrow = label as f64 * CHAR_W > x1 - x0 + 6.0;
                lowered = narrow && !lowered;
                let dy = if lowered { 28.0 } else { 0.0 };
                s.bracket_under(x0, x1, y + self.ch + 6.0, field.class);
                if lowered {
                    s.line(
                        (cx, y + self.ch + 6.0),
                        (cx, y + self.ch + 9.0 + dy),
                        "thin",
                    );
                }
                s.text(
                    cx,
                    y + self.ch + 21.0 + dy,
                    "middle",
                    field.class,
                    &field.name,
                );
                if !field.value.is_empty() {
                    s.text(
                        cx,
                        y + self.ch + 36.0 + dy,
                        "middle",
                        "soft small",
                        &field.value,
                    );
                }
            }
        }
        Ok(())
    }
}

/// The row whose part of `field` is the longest, the first of equals.
fn label_row(field: &Field, rows: &[Row<'_>]) -> Option<usize> {
    let part = |&(from, to, _): &Row<'_>| field.end().min(to).saturating_sub(field.at.max(from));
    let longest = rows.iter().map(part).max()?;
    rows.iter().position(|row| part(row) == longest)
}

/// `n` bytes from `at` in `b` as a little-endian number.
pub fn le(b: &[u8], at: usize, n: usize) -> anyhow::Result<u64> {
    let bytes = b.get(at..at + n).context("a number inside the block")?;
    Ok(bytes
        .iter()
        .rev()
        .fold(0u64, |v, byte| (v << 8) | u64::from(*byte)))
}

/// The first bytes of `b` as hex and an ellipsis: a hash or an id, shortened to fit its field.
pub fn hex_start(b: &[u8], n: usize) -> String {
    let mut s: String = b.iter().take(n).map(|x| format!("{x:02x}")).collect();
    if b.len() > n {
        s.push('…');
    }
    s
}
