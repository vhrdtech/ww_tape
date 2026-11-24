# example_thermal

The ww_tape book's thermal field: a plate of 128 × 96 temperature sensors, read ten times a second for 60 s, as one grid track with two fixed axes. A hot spot crosses the plate and a heater stays warm. `Thermal::record` writes it through a mux in tiles of 32 × 32, with a summary tree per tile and space overviews to level 5. `box_query` finds the chunks of a box of cells, `region` merges the summary nodes of the tiles in a box into a timeline, and `view` reads the whole plate at the level its pixels ask for.

```sh
cargo run -p example_thermal  # prints the numbers the chapter "Fields and images over time" quotes
```

`tests/thermal.rs` checks each read against a naive model of the cells and pins every number of the page `docs/grids.md`.
