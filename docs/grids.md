---
level: advanced
---

# Fields and images over time

A plate of 12 288 temperature sensors reads ten times a second. A camera delivers 307 200 pixels ten times a second.
Each reading is a whole field of values, and a viewer wants a corner of one moment, one corner over the whole run, or
the whole field from far away. How does a tape hold such a field so that each view reads a small part of it?

The tape keeps it as a [[grid track]]: a [[series]] track with [[fixed axes|fixed axis]] beside time. The writer cuts
the field into [[tiles|tile]] and keeps two coarse views, one for zooming in time and one for zooming in space. The
crate `example_thermal` records the plate, and `example_images` records the camera.

## Cells on fixed axes

The plate has 128 sensors along `x` and 96 along `y`. It reads in steps of 0.01 °C, once per 100 ms, for 60 s. The
run is 600 readings of 12 288 cells, and the cells take 15 MB as written. The track's definition names the two axes
and the sample type:

```rs
--8<-- "examples/example_thermal/src/lib.rs:axes"
```

```rs
--8<-- "examples/example_thermal/src/lib.rs:def"
```

A series is the grid track with no fixed axes, and it keeps the bytes it always had. Time is the one axis with a clock
and the one that grows. A cell holds the value of one place at one time, as one sample of a series does. The
channels of a sample, such as the red, green and blue of a pixel, stay inside the cell and are not an axis.

??? example "What the example uses here: the plate's readings"

    ```rs
    --8<-- "examples/example_thermal/src/lib.rs:value"
    ```

A hot spot crosses the plate's middle row at one cell every 5 readings. A heater near the lower right corner stays
warm.

## Tiles

A chunk of a grid track holds one tile: a run of readings and a box of cells. The plate's tiles are 32 cells on each
side, so the plate has 3 tiles along `y` and 4 along `x`. Each tile has its coordinates, such as `(1, 0)`, and the
index entry of its chunk packs them into one [[tile number]]. The number is a bit field per axis, and the plate's 12
tiles take 4 bits.

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/plate_tiles.svg"
<figcaption>Left: the plate's 12 tiles with their coordinates and tile numbers, and a box query over rows 40 to 56 and
columns 20 to 60, which meets 2 tiles. Right: the levels of space overviews, each half the size of the one before.
Drawn by <code>ww_tape_figures</code> from the plate's layout, at 1/6 of its readings.</figcaption>
</figure>

A writer takes whole readings of the plate. Its handle splits each reading into the open chunk of every tile, and it
completes the chunks of all tiles together:

```rs
--8<-- "examples/example_thermal/src/lib.rs:record"
```

A chunk holds 100 readings, so each tile has 6 chunks and the run has 72. Nothing about the chunk size is special to
grids: the writer cuts a tile's chunk by readings, bytes or latency, as it does for a series.

## The box query

A viewer that opens the corner of the plate asks for a box of cells over a stretch of time. The reader turns the box
into a range of tile coordinates along each axis. It then tests each index entry of the track by shift and mask on the
tile number, and it reads no chunk to do so:

```rs
--8<-- "examples/example_thermal/src/lib.rs:box_query"
```

The tile in the hot row's first column holds 6 chunks over the whole run. A box of 16 rows and 40 columns meets 2 tiles,
so it finds 2 chunks in the first 10 s, where the whole plate has 12. The query returns index entries, and the viewer
reads only the chunks it wants.

```rust
use example_thermal::Thermal;
let layout = Thermal::plate().layout().expect("a valid layout");
// the box of the figure meets tile row 1 and tile columns 0 and 1
let tiles = layout.tile_box(&[40..56, 20..60]).expect("two axes");
assert_eq!(tiles.ranges(), [(1, 2), (0, 2)]);
```

## A region over time

A timeline of the plate's hot corner needs no cell. The writer keeps a [[summary node]] per tile for every 16 readings
of the tile, and the nodes of a tile form the same tree a series has ([Summaries](summaries.md)). A viewer merges the
nodes of the tiles in a box into the timeline of that region:

```rs
--8<-- "examples/example_thermal/src/lib.rs:region"
```

A node covers a whole tile, so a box that meets a tile part way counts the tile whole. The timeline is at tile
resolution. In the hot row's first tile the largest value of node 0 is 7770, which is 77.70 °C. It falls below 30 °C
from node 9, when the spot has left the tile. The heater's tile stays between 4008 and 4010 for the whole run.

## Zooming out in space

A viewer that draws the whole plate in 8 columns of pixels needs far fewer cells than the plate has. The writer therefore
keeps [[space overviews|space overview]]. Overview level `k` halves each downsampled axis `k` times, and it keeps every
reading. Each cell of a level holds the smallest value, the largest value and the mean of the cells it covers. The
chunk of a tile and its overview chunks go to the tape as one unit, so a crash never leaves a tile without its views.

The levels of the plate:

| Level | Cells | Chunks | Bytes |
|---|---|---|---|
| 0 | 128 × 96 | 72 | 15 MB |
| 1 | 64 × 48 | 72 | 11 MB |
| 2 | 32 × 24 | 72 | 2.8 MB |
| 3 | 16 × 12 | 72 | 703 KB |
| 4 | 8 × 6 | 72 | 184 KB |
| 5 | 4 × 3 | 72 | 55 KB |

The chain with its overviews takes 30 MB, twice the cells as written. The sum of the levels above level 0 is about the
size of level 0, since each level holds three values per cell on a quarter of the cells.

A viewer picks the level from the cells one pixel covers. `TileLayout::level_for` returns the highest level whose cell
still fits in a pixel. The viewer then finds the chunks of that level with a box in that level's cells, and reads the
mean, the smallest or the largest column of each:

```rs
--8<-- "examples/example_thermal/src/lib.rs:view"
```

The views of the plate at reading 300 show the saving. Each view reads the 12 chunks of one reading, and the level
decides their size:

| Cells a pixel | Level | Cells | Chunks | Bytes |
|---|---|---|---|---|
| 1 | 0 | 128 × 96 | 12 | 2.5 MB |
| 4 | 2 | 32 × 24 | 12 | 463 KB |
| 16 | 4 | 8 × 6 | 12 | 31 KB |
| 32 | 5 | 4 × 3 | 12 | 9.1 KB |

```rust
use example_thermal::Thermal;
let layout = Thermal::plate().layout().expect("a valid layout");
assert_eq!(layout.overview_levels(), 5);
// a pixel of 4 cells takes level 2, whose cells are 4 cells wide
assert_eq!(layout.level_for(&[4.0, 4.0]), 2);
```

## Where overviews end

An overview belongs to one tile: it summarises the tile's own cells. A level therefore exists only while each
downsampled tile extent halves evenly. A tile of 32 cells halves 5 times, a tile of 60 cells only twice, since
60 is 4 times 15. Past that level a cell would straddle two tiles.

The image sequence shows what this costs. Its frames are 640 × 480 pixels, and each pixel has three channels. A bright
square moves along a gradient. The same 10 frames, 922 KB each, in three tile sizes:

```rs
--8<-- "examples/example_images/src/lib.rs:def"
```

| Tile | Tiles | Levels | Chain | Thumbnail | Square |
|---|---|---|---|---|---|
| 64 × 64 | 8 × 10 | 6 | 21 MB | level 4, 80 chunks, 24 KB | 9 chunks, 112 KB |
| 60 × 60 | 8 × 11 | 2 | 19 MB | level 2, 88 chunks, 187 KB | 4 chunks, 44 KB |
| 240 × 320 | 2 × 2 | 4 | 18 MB | level 4, 4 chunks, 11 KB | 2 chunks, 461 KB |

The thumbnail is the whole frame at 16 cells a pixel, and the square is the 100 × 100 pixels it covers at frame 3.
Two limits show. The tiles of 60 stop at level 2, so the thumbnail reads about 8 times the bytes of the tiles of 64. The
tiles of 240 × 320 read the thumbnail in 4 chunks, but the square needs about 4 times the bytes.

An overview saves bytes, not chunks. A thumbnail at any level reads one overview chunk per tile, so many small tiles
mean many small reads. A tile extent is therefore a choice between a cheap box query and a cheap overview, and a power
of two keeps every level.

```rust
use example_images::Images;
let sixty = Images::frames().tiles(60, 60).layout().expect("a valid layout");
let sixty_four = Images::frames().layout().expect("a valid layout");
assert_eq!((sixty.overview_levels(), sixty_four.overview_levels()), (2, 6));
```

The [format reference](format.md#fixed-axes-and-tiles) holds the bytes of the tile number, the overview chunks and the
tile summaries.
