//! The image sequence, headless: `cargo run -p example_images` records the frames in three tile sizes and prints the
//! numbers the book's chapter on grids quotes.
#![allow(clippy::print_stdout, reason = "the binary prints the story's numbers")]

use example_capture::about;
use example_images::Images;

// --8<-- [start:main]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let images = Images::frames();
    let story = images.story()?;
    println!(
        "{} frames of {} × {} pixels, {} a frame as written",
        images.frames,
        images.width,
        images.height,
        about(story.frame_bytes)
    );
    for row in &story.rows {
        println!(
            "tiles of {} × {}: {} × {} tiles ({} bits), {} levels, chain {}",
            row.tile.0,
            row.tile.1,
            row.tiles.0,
            row.tiles.1,
            row.tile_bits,
            row.levels,
            about(row.chain_bytes)
        );
        println!(
            "  thumbnail at level {}: {} chunks, {}; the square: {} chunks, {}",
            row.thumb_level,
            row.thumb_chunks,
            about(row.thumb_bytes),
            row.box_chunks,
            about(row.box_bytes)
        );
    }
    Ok(())
}
// --8<-- [end:main]
