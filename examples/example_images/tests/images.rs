//! The frames through the two reads of the chapter: a thumbnail holds the means of the cells it covers, a box of
//! pixels reads only the tiles it meets, and the page says the numbers the run prints.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "tests fail by panicking and compute expected values freely"
)]

use example_capture::about;
use example_images::{
    CHANNELS, Images, SQUARE_COLUMNS, SQUARE_ROWS, STORY_FRAME, THUMB_CELLS_PER_PIXEL,
};
use ww_tape_summary::overview::div_even;

/// The text of the book page `name`, its line breaks as spaces.
fn page(name: &str) -> String {
    let path = format!("{}/../../docs/{name}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).expect("the page is there");
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `n` with its digits in groups of three, as the book writes an exact count: "307 200".
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
fn the_same_frames_give_the_same_bytes() {
    let images = Images::frames();
    assert_eq!(
        images.record().expect("records").pieces,
        images.record().expect("records").pieces
    );
}

/// A pixel read at level 0 is the pixel written, and a cell of level `k` holds the mean of the `2^k × 2^k` pixels it
/// covers, in each channel.
#[test]
fn a_view_holds_the_pixels_and_their_means() {
    let images = Images::frames();
    let rec = images.record().expect("records");
    let frame = STORY_FRAME;
    for (cells_per_pixel, level) in [(1.0, 0), (4.0, 2), (16.0, 4), (64.0, 6)] {
        let view = images
            .view(
                &rec,
                frame,
                0..images.height,
                0..images.width,
                cells_per_pixel,
            )
            .expect("view");
        assert_eq!(view.level, level);
        let side = 1u32 << level;
        for r in 0..view.height {
            for c in 0..view.width {
                for ch in 0..CHANNELS {
                    let values: Vec<i128> = (r * side..((r + 1) * side).min(images.height))
                        .flat_map(|y| {
                            (c * side..((c + 1) * side).min(images.width))
                                .map(move |x| i128::from(images.pixel(frame, y, x, ch)))
                        })
                        .collect();
                    let want = div_even(values.iter().sum(), values.len() as u32) as u8;
                    let at = ((r * view.width + c) * CHANNELS + ch) as usize;
                    assert_eq!(
                        view.mean[at], want,
                        "level {level} cell {r}, {c} channel {ch}"
                    );
                }
            }
        }
    }
}

/// A box of pixels reads the tiles it meets, and no cell outside them.
#[test]
fn a_box_reads_its_tiles_only() {
    let images = Images::frames();
    let rec = images.record().expect("records");
    let square = images
        .view(&rec, STORY_FRAME, SQUARE_ROWS, SQUARE_COLUMNS, 1.0)
        .expect("view");
    // rows 190..290 meet the tile rows 2, 3 and 4, and columns 120..220 the tile columns 1, 2 and 3
    assert_eq!(square.chunks, 9);
    let whole = images
        .view(&rec, STORY_FRAME, 0..images.height, 0..images.width, 1.0)
        .expect("view");
    assert_eq!(whole.chunks, 80);
    assert!(square.bytes * 8 < whole.bytes);
    // the square's pixels are the pixels written
    for y in SQUARE_ROWS {
        for x in SQUARE_COLUMNS {
            for ch in 0..CHANNELS {
                let at = ((y * 640 + x) * CHANNELS + ch) as usize;
                assert_eq!(
                    square.mean[at],
                    images.pixel(STORY_FRAME, y, x, ch),
                    "pixel {y}, {x}"
                );
            }
        }
    }
}

/// Every number of the page comes from the run.
#[test]
fn the_page_says_what_the_camera_measures() {
    let images = Images::frames();
    let story = images.story().expect("story");
    let page = page("grids.md");
    let pixels = u64::from(images.width) * u64::from(images.height);
    let mut phrases = vec![
        format!(
            "A camera delivers {} pixels ten times a second.",
            grouped(pixels)
        ),
        format!(
            "Its frames are {} × {} pixels, and each pixel has three channels.",
            images.height, images.width
        ),
        format!(
            "The same {} frames, {} each, in three tile sizes, rows × columns:",
            images.frames,
            about(story.frame_bytes)
        ),
    ];
    assert_eq!(CHANNELS, 3);
    phrases.push(format!(
        "The thumbnail is the whole frame at {THUMB_CELLS_PER_PIXEL} cells per pixel."
    ));
    phrases.push(format!(
        "The square is the {} × {} pixels it covers in the fourth frame.",
        SQUARE_ROWS.end - SQUARE_ROWS.start,
        SQUARE_COLUMNS.end - SQUARE_COLUMNS.start
    ));
    assert_eq!(STORY_FRAME, 3, "the fourth frame");
    for r in &story.rows {
        phrases.push(format!(
            "| {} × {} | {} × {} | {} | {} | level {}, {} chunks, {} | {} chunks, {} |",
            r.tile.0,
            r.tile.1,
            r.tiles.0,
            r.tiles.1,
            r.levels,
            about(r.chain_bytes),
            r.thumb_level,
            r.thumb_chunks,
            about(r.thumb_bytes),
            r.box_chunks,
            about(r.box_bytes)
        ));
        // a thumbnail at any level reads one overview chunk per tile
        assert_eq!(r.thumb_chunks, u64::from(r.tiles.0 * r.tiles.1));
    }
    let [sixty_four, sixty, big] = [story.rows[0], story.rows[1], story.rows[2]];
    // the tiles of 60 stop at level 2, those of 64 reach 6, and a tile of 60 halves twice
    assert_eq!(
        (sixty.levels, sixty_four.levels, 60u32.trailing_zeros()),
        (2, 6, 2)
    );
    assert_eq!(sixty.thumb_bytes.div_ceil(sixty_four.thumb_bytes), 8);
    assert_eq!(big.box_bytes / sixty_four.box_bytes, 4);
    assert_eq!(big.thumb_chunks, 4);
    for phrase in phrases {
        assert!(page.contains(&phrase), "docs/grids.md says: {phrase}");
    }
}
