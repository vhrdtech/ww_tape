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
        "{} × {} sensors (rows × columns), {} readings: {} as written, {} in the chain with its overviews",
        plate.height,
        plate.width,
        plate.steps,
        about(story.raw_bytes),
        about(story.chain_bytes)
    );
    println!(
        "{} × {} tiles (rows × columns), {} bits of tile number, {} overview levels",
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
            row.cells.0,
            row.cells.1,
            row.chunks,
            about(row.bytes)
        );
    }
    for view in &story.views {
        println!(
            "{} cells per pixel: level {}, {} × {} cells, {} chunks, {}",
            view.cells_per_pixel,
            view.level,
            view.cells.0,
            view.cells.1,
            view.chunks,
            about(view.bytes)
        );
    }
    // a level-0 node covers 16 readings
    let cools = story.sweep_peaks.iter().position(|peak| *peak < 3_000);
    println!(
        "hot row's first tile: peak {} in node 0, below 30 °C from node {}",
        story.sweep_peaks.first().unwrap_or(&0),
        cools.unwrap_or(0)
    );
    let (low, high) = (
        story.heater_peaks.iter().min().unwrap_or(&0),
        story.heater_peaks.iter().max().unwrap_or(&0),
    );
    println!("the heater's tile: peaks from {low} to {high}");
    Ok(())
}
// --8<-- [end:main]
