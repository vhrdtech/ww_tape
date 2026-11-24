# example_thermal

The ww_tape book's thermal field: a plate of temperature sensors, read ten times a second for a minute. It is one grid track with two fixed axes. A hot spot crosses the plate and a heater stays warm.

`Thermal::record` writes it through a mux in square tiles. Each tile has a summary tree, and the space overviews reach level five. `box_query` finds the chunks of a box of cells. `region` merges the summary nodes of a box's tiles into a timeline. `view` reads the whole plate at the level its pixels ask for.

```sh
cargo run -p example_thermal  # prints the numbers the chapter "Fields and images over time" quotes
```

`tests/thermal.rs` checks each read against a naive model of the cells. It also pins every number of the page.
