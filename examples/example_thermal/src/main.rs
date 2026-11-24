//! The thermal plate, headless: `cargo run -p example_thermal` records the run and prints the numbers the book's
//! chapter on grids quotes.
#![allow(clippy::print_stdout, reason = "the binary prints the story's numbers")]

use example_capture::about;
use example_thermal::Thermal;

// --8<-- [start:main]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let plate = Thermal::plate();
    let story = plate.story()?;
    println!(
        "{} × {} sensors, {} readings: {} as written, {} in the chain with its overviews",
        plate.width,
        plate.height,
        plate.steps,
        about(story.raw_bytes),
        about(story.chain_bytes)
    );
    println!(
        "{} × {} tiles, {} bits of tile number, {} overview levels",
        story.tiles.0, story.tiles.1, story.tile_bits, story.levels
    );
    println!(
        "chunks over the whole run: {} for the plate, {} for one tile, {} for a box over the first 10 s",
        story.whole, story.one_tile, story.box_chunks
    );
    for row in &story.level_rows {
        println!(
            "level {}: {} × {} cells, {} chunks, {}",
            row.level,
            row.cells.1,
            row.cells.0,
            row.chunks,
            about(row.bytes)
        );
    }
    for view in &story.views {
        println!(
            "{} cells a pixel: level {}, {} × {} cells, {} chunks, {}",
            view.cells_per_pixel,
            view.level,
            view.cells.1,
            view.cells.0,
            view.chunks,
            about(view.bytes)
        );
    }
    println!(
        "hot row's first tile, peak per node: {:?}",
        story.sweep_peaks
    );
    println!("heater's tile, peak per node: {:?}", story.heater_peaks);
    Ok(())
}
// --8<-- [end:main]
