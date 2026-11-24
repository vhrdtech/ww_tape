# example_images

The ww_tape book's image sequence: frames of pixels, each pixel a cell of three `u8` channels. It is one grid track with two fixed axes and no time pyramid. A bright square moves along a gradient.

`Images::record` writes the frames through a mux. `view` reads a box of a frame at the space overview level its pixels ask for. `Images::story` records the frames in three tile sizes. It measures what a thumbnail and a box of pixels read in each.

```sh
cargo run -p example_images  # prints the numbers the chapter "Fields and images over time" quotes
```

`tests/images.rs` checks each read against a naive model of the pixels. It also pins every number of the page.
