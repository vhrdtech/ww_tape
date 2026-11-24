# example_images

The ww_tape book's image sequence: 10 frames of 640 × 480 pixels, a pixel a cell of three `u8` channels, as one grid track with two fixed axes and no time pyramid. A bright square moves along a gradient. `Images::record` writes the frames through a mux, and `view` reads a box of a frame at the space overview level its pixels ask for. `Images::story` records the frames in tiles of 64 × 64, 60 × 60 and 240 × 320 and measures what a thumbnail and a box of pixels read in each.

```sh
cargo run -p example_images  # prints the numbers the chapter "Fields and images over time" quotes
```

`tests/images.rs` checks each read against a naive model of the pixels and pins every number of the page `docs/grids.md`.
