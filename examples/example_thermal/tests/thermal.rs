//! The plate through the three reads of the chapter: a box finds the tiles it meets, a region's timeline holds the
//! values of its cells, a view at a level holds the min, max and mean of the cells it covers, and the page says the
//! numbers the run prints.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "tests fail by panicking and compute expected values freely"
)]

use example_capture::about;
use example_thermal::Thermal;
use ww_tape_summary::{Value, overview::div_even};

/// The text of the book page `name`, its line breaks as spaces.
fn page(name: &str) -> String {
    let path = format!("{}/../../docs/{name}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).expect("the page is there");
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `n` with its digits in groups of three, as the book writes an exact count: "12 288".
fn grouped(n: u64) -> String {
    let mut out = String::new();
    for (i, d) in n.to_string().chars().rev().enumerate() {
        if i != 0 && i % 3 == 0 {
            out.push(' ');
        }
        out.push(d);
    }
    out.chars().rev().collect()
}

/// The same recording twice gives the same bytes.
#[test]
fn the_same_plate_gives_the_same_bytes() {
    let plate = Thermal {
        steps: 200,
        ..Thermal::plate()
    };
    assert_eq!(
        plate.record().expect("records").pieces,
        plate.record().expect("records").pieces
    );
}

/// A box finds the chunks of exactly the tiles it meets, in the times it meets.
#[test]
fn a_box_finds_the_tiles_it_meets() {
    let plate = Thermal::plate();
    let rec = plate.record().expect("records");
    let first_ten_s = 0..plate.tick(100);
    let found = plate
        .box_query(&rec, first_ten_s.clone(), 40..56, 20..60)
        .expect("queries");
    let mut tiles: Vec<_> = found.iter().map(|t| t.tile.as_slice().to_vec()).collect();
    tiles.sort();
    assert_eq!(tiles, [vec![1, 0], vec![1, 1]]);
    // a chunk of 100 readings: one chunk a tile in the first 10 s, and no chunk of another time
    assert!(
        found
            .iter()
            .all(|t| t.entry.count == 100 && t.entry.first_item.get() == 0)
    );
    // the box's cell edges select the tiles: one row less and one tile row is left
    let tiles = plate
        .box_query(&rec, first_ten_s, 31..32, 0..1)
        .expect("queries");
    assert_eq!(tiles.len(), 1);
    assert_eq!(tiles[0].tile.as_slice(), [0, 0]);
}

/// A region's nodes hold the largest value of the cells of their tiles, node by node.
#[test]
fn a_region_holds_the_values_of_its_tiles() {
    let plate = Thermal::plate();
    let rec = plate.record().expect("records");
    for (ys, xs) in [(32..64, 0..32), (64..96, 96..128)] {
        let nodes = plate
            .region(&rec, ys.clone(), xs.clone(), 0)
            .expect("region");
        assert_eq!(nodes.len() as u64, plate.steps.div_ceil(16));
        for node in nodes {
            assert_eq!(node.tiles, 1);
            let from = node.node * 16;
            let to = (from + 16).min(plate.steps);
            let mut peak = i64::MIN;
            for t in from..to {
                for y in ys.clone() {
                    for x in xs.clone() {
                        peak = peak.max(i64::from(plate.value(t, y, x)));
                    }
                }
            }
            let Value::I64(got) = node.stats.channels[0].max[0] else {
                panic!("an integer maximum")
            };
            assert_eq!(got, peak, "node {}", node.node);
            assert_eq!(node.samples, (to - from) * 32 * 32);
        }
    }
}

/// A cell of level `k` holds the min, max and mean of its `2^k × 2^k` cells as written.
#[test]
fn a_view_holds_the_naive_downsample() {
    let plate = Thermal::plate();
    let rec = plate.record().expect("records");
    let step = 300;
    for (cells_per_pixel, level) in [(1.0, 0), (2.0, 1), (4.0, 2), (16.0, 4), (32.0, 5)] {
        let view = plate.view(&rec, step, cells_per_pixel).expect("view");
        assert_eq!(view.level, level);
        assert_eq!(view.chunks, 12);
        let side = 1u32 << level;
        for r in 0..view.height {
            for c in 0..view.width {
                let cells: Vec<i64> = (r * side..((r + 1) * side).min(plate.height))
                    .flat_map(|y| {
                        (c * side..((c + 1) * side).min(plate.width))
                            .map(move |x| i64::from(plate.value(step, y, x)))
                    })
                    .collect();
                let sum: i64 = cells.iter().sum();
                let want = [
                    *cells.iter().min().expect("cells") as i16,
                    *cells.iter().max().expect("cells") as i16,
                    div_even(i128::from(sum), cells.len() as u32) as i16,
                ];
                assert_eq!(
                    view.cells[(r * view.width + c) as usize],
                    want,
                    "level {level} cell {r}, {c}"
                );
            }
        }
    }
}

/// Every number of the page comes from the run.
#[test]
fn the_page_says_what_the_plate_measures() {
    let plate = Thermal::plate();
    let story = plate.story().expect("story");
    let page = page("grids.md");
    let cells = u64::from(plate.width) * u64::from(plate.height);
    let mut phrases = vec![
        format!("A plate of {} temperature sensors", grouped(cells)),
        format!(
            "The plate has {} sensors along `x` and {} along `y`.",
            plate.width, plate.height
        ),
        format!(
            "The run is {} readings of {} cells, and the cells take {} as written.",
            plate.steps,
            grouped(cells),
            about(story.raw_bytes)
        ),
        format!(
            "so the plate has {} tiles along `y` and {} along `x`.",
            story.tiles.0, story.tiles.1
        ),
        format!(
            "the plate's {} tiles take {} bits.",
            story.tiles.0 * story.tiles.1,
            story.tile_bits
        ),
        format!(
            "A chunk holds {} readings, so each tile has {} chunks and the run has {} in all.",
            plate.chunk_steps, story.one_tile, story.whole
        ),
        format!(
            "It finds {} chunks in the first 10 s, where the whole plate has {} of them.",
            story.box_chunks, story.whole_first
        ),
        format!(
            "The tile in the hot row's first column holds {} chunks over the whole run.",
            story.one_tile
        ),
        format!(
            "the largest value of node 0 is {}, which is {:.2} °C.",
            story.sweep_peaks[0],
            story.sweep_peaks[0] as f64 / 100.0 - 0.0
        ),
        format!(
            "from node {}, when the spot has left the tile",
            story
                .sweep_peaks
                .iter()
                .position(|p| *p < 3_000)
                .expect("it cools")
        ),
        format!(
            "stays between {} and {} for the whole run",
            story.heater_peaks.iter().min().expect("peaks"),
            story.heater_peaks.iter().max().expect("peaks")
        ),
        format!(
            "The chain with its overviews takes {}, twice the cells as written.",
            about(story.chain_bytes)
        ),
    ];
    // a tile of 32 halves 5 times
    assert_eq!(story.levels, 5);
    assert_eq!(story.chain_bytes / story.raw_bytes, 2);
    for r in &story.level_rows {
        phrases.push(format!(
            "| {} | {} × {} | {} | {} |",
            r.level,
            r.cells.1,
            r.cells.0,
            r.chunks,
            about(r.bytes)
        ));
    }
    for v in &story.views {
        phrases.push(format!(
            "| {} | {} | {} × {} | {} | {} |",
            v.cells_per_pixel,
            v.level,
            v.cells.1,
            v.cells.0,
            v.chunks,
            about(v.bytes)
        ));
    }
    for phrase in phrases {
        assert!(page.contains(&phrase), "docs/grids.md says: {phrase}");
    }
}
