//! The thermal plate's figures:
//!
//! - `plate_tiles.svg`: the plate's tiles with their coordinates and tile numbers as the index packs them, and a box
//!   query over two of them. The tiles come from the layout of the plate's definition, and the tiles the box finds
//!   from `TapeReader::grid_tiles` on a recording of the plate at a sixth of its readings.
//! - `plate_levels.svg`: the plate's space overview levels to scale, each with its cells, from the same layout.
#![allow(
    clippy::indexing_slicing,
    reason = "the layout of the plate's two fixed axes has two entries"
)]

use anyhow::{Context, ensure};
use example_thermal::{BOX_COLUMNS, BOX_ROWS, Thermal};
use ww_tape::TileCoords;

use crate::svg::Svg;

/// Width and height of a tile as drawn.
const TILE: (f64, f64) = (72.0, 54.0);

pub fn plate_tiles() -> anyhow::Result<String> {
    let plate = Thermal {
        steps: 100,
        ..Thermal::plate()
    };
    let rec = plate.record().ok().context("the plate records")?;
    let layout = rec.layout;
    let found = plate
        .box_query(&rec, 0..plate.ticks(), BOX_ROWS, BOX_COLUMNS)
        .ok()
        .context("the box query")?;
    ensure!(!found.is_empty(), "the box meets tiles");
    let [rows, cols] = [layout.tiles()[0], layout.tiles()[1]];
    let bits = layout.widths();
    let (left, top) = (20.0, 56.0);
    let (w, h) = (f64::from(cols) * TILE.0, f64::from(rows) * TILE.1);
    let mut s = Svg::new(left + w + 20.0, top + h + 40.0, "the plate's tiles");
    s.text(
        left,
        24.0,
        "start",
        "strong",
        "Tiles: coordinates and tile number",
    );
    for r in 0..rows {
        for c in 0..cols {
            let coords = TileCoords::new(&[r, c]).context("two coordinates")?;
            let number = layout.pack(&coords).context("a tile of the layout")?;
            let hit = found.iter().any(|t| t.tile == coords);
            let at = (left + f64::from(c) * TILE.0, top + f64::from(r) * TILE.1);
            s.rect(at, TILE, 0.0, if hit { "tint-accent line" } else { "line" });
            s.text(
                at.0 + TILE.0 / 2.0,
                at.1 + 22.0,
                "middle",
                "",
                &format!("({r}, {c})"),
            );
            let number = format!(
                "{:0w0$b} {:0w1$b}",
                number >> bits[1],
                number & ((1 << bits[1]) - 1),
                w0 = usize::from(bits[0]),
                w1 = usize::from(bits[1])
            );
            s.text(
                at.0 + TILE.0 / 2.0,
                at.1 + 40.0,
                "middle",
                "soft small mono",
                &number,
            );
        }
    }
    // the box in cells, drawn over the tiles
    let (cw, ch) = (
        TILE.0 / f64::from(plate.tile),
        TILE.1 / f64::from(plate.tile),
    );
    s.rect(
        (
            left + f64::from(BOX_COLUMNS.start) * cw,
            top + f64::from(BOX_ROWS.start) * ch,
        ),
        (
            f64::from(BOX_COLUMNS.end - BOX_COLUMNS.start) * cw,
            f64::from(BOX_ROWS.end - BOX_ROWS.start) * ch,
        ),
        0.0,
        "hollow dash accent-line",
    );
    s.text(
        left + w / 2.0,
        top + h + 22.0,
        "middle",
        "small",
        &format!(
            "box: rows {}..{}, columns {}..{}: {} tiles",
            BOX_ROWS.start,
            BOX_ROWS.end,
            BOX_COLUMNS.start,
            BOX_COLUMNS.end,
            found
                .iter()
                .map(|t| t.tile)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
        ),
    );
    Ok(s.finish())
}

pub fn plate_levels() -> anyhow::Result<String> {
    let layout = Thermal::plate().layout().context("the plate's layout")?;
    let (left, top) = (20.0, 56.0);
    let scale = 1.6;
    let mut boxes = Vec::new();
    let mut x = left;
    for level in 0..=layout.overview_levels() {
        let at = layout.overview(level).context("a level of the layout")?;
        let (cells_y, cells_x) = (at.extents()[0].0, at.extents()[1].0);
        let size = (f64::from(cells_x) * scale, f64::from(cells_y) * scale);
        boxes.push((level, x, size, cells_y, cells_x));
        x += size.0.max(44.0) + 16.0;
    }
    let height = boxes.first().map_or(0.0, |b| b.2.1);
    let mut s = Svg::new(
        (x + 4.0).ceil(),
        top + height + 66.0,
        "the plate's overview levels",
    );
    s.text(
        left,
        24.0,
        "start",
        "strong",
        "Space overview levels of the plate, to scale: level, then rows × columns of cells",
    );
    for (level, x, size, cells_y, cells_x) in boxes {
        s.rect(
            (x, top),
            size,
            0.0,
            if level == 0 {
                "tint-blue line"
            } else {
                "tint-teal line"
            },
        );
        s.text(
            x,
            top + height + 20.0,
            "start",
            "small",
            &format!("{level}"),
        );
        s.text(
            x,
            top + height + 38.0,
            "start",
            "soft small",
            &format!("{cells_y} × {cells_x}"),
        );
    }
    s.text(
        left,
        top + height + 58.0,
        "start",
        "soft small",
        "level k: every downsampled axis halved k times; each cell holds the smallest, largest and mean value",
    );
    Ok(s.finish())
}
