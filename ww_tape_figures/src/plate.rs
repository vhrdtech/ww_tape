//! `plate_tiles.svg`: the thermal plate's tiles with their coordinates and tile numbers as the index packs them, a box
//! query over two of them, and the plate's space overview levels. The tiles and levels come from the layout of the
//! plate's definition, and the tiles the box finds from `TapeReader::grid_tiles` on a recording of the plate at a
//! sixth of its readings.

use anyhow::{Context, ensure};
use example_thermal::Thermal;
use ww_tape::TileCoords;

use crate::svg::Svg;

/// Width and height of a tile as drawn.
const TILE: (f64, f64) = (72.0, 54.0);
/// The cells of the box the figure queries: rows and columns.
const BOX: ([u32; 2], [u32; 2]) = ([40, 56], [20, 60]);

pub fn plate_tiles() -> anyhow::Result<String> {
    let plate = Thermal {
        steps: 100,
        ..Thermal::plate()
    };
    let rec = plate.record().ok().context("the plate records")?;
    let layout = rec.layout;
    let found = plate
        .box_query(
            &rec,
            0..plate.ticks(),
            BOX.0[0]..BOX.0[1],
            BOX.1[0]..BOX.1[1],
        )
        .ok()
        .context("the box query")?;
    ensure!(!found.is_empty(), "the box meets tiles");
    let [rows, cols] = [layout.tiles()[0], layout.tiles()[1]];
    let bits = layout.widths();
    let (left, top) = (20.0, 56.0);
    let (w, h) = (f64::from(cols) * TILE.0, f64::from(rows) * TILE.1);
    let mut s = Svg::new(
        left + w + 60.0 + 290.0,
        top + h + 70.0,
        "the plate's tiles and overview levels",
    );
    s.text(
        left,
        24.0,
        "start",
        "strong",
        "Tiles: coordinates, and the number the index packs",
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
            left + f64::from(BOX.1[0]) * cw,
            top + f64::from(BOX.0[0]) * ch,
        ),
        (
            f64::from(BOX.1[1] - BOX.1[0]) * cw,
            f64::from(BOX.0[1] - BOX.0[0]) * ch,
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
            BOX.0[0],
            BOX.0[1],
            BOX.1[0],
            BOX.1[1],
            found.iter().map(|t| t.tile).collect::<std::collections::BTreeSet<_>>().len()
        ),
    );
    // the levels, each half the size of the one before
    let x0 = left + w + 60.0;
    s.text(
        x0,
        24.0,
        "start",
        "strong",
        "Overview levels: cells of each",
    );
    let scale = 1.2;
    let mut x = x0;
    for level in 0..=layout.overview_levels() {
        let at = layout.overview(level).context("a level of the layout")?;
        let (cells_y, cells_x) = (at.extents()[0].0, at.extents()[1].0);
        let size = (f64::from(cells_x) * scale, f64::from(cells_y) * scale);
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
            x + size.0 / 2.0,
            top + size.1 + 18.0,
            "middle",
            "small",
            &format!("{level}"),
        );
        x += size.0 + 14.0;
    }
    s.text(
        x0,
        top + 140.0 + 22.0,
        "start",
        "soft small",
        "level k: every downsampled axis halved k times",
    );
    Ok(s.finish())
}
