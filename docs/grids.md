---
level: advanced
---

# Fields and images over time

A plate of 12 288 temperature sensors reads ten times a second. A camera delivers 307 200 pixels ten times a second.
Each reading is a whole field of values. A viewer wants a corner of one moment, one corner over the whole run, or the
whole field from far away. How does a tape hold such a field so that each view reads a small part of it?

The tape keeps it as a [[grid track]]: a [[series]] track with [[fixed axes|fixed axis]] beside time. The writer cuts
the field into [[tiles|tile]] and keeps two coarse views, one for zooming in time and one for zooming in space. The
crate `example_thermal` records the plate, and `example_images` records the camera.

## Cells on fixed axes

The plate has 96 rows of 128 sensors. It reads in steps of 0.01 °C, once per 100 ms, for 60 s.
The run is 600 readings of 12 288 cells. The cells take 15 MB as written. Sizes in this chapter read rows × columns.

The track's definition names the two axes:

```rust
--8<-- "examples/example_thermal/src/lib.rs:axes"
```

```rust
--8<-- "examples/example_thermal/src/lib.rs:def"
```

A series track has no fixed axes, and a grid track adds them. A grid track is not the [[grid]] of a series chunk, which
only places that chunk's samples in time. Time is the one axis with a clock and the one that
grows. A cell holds the value of one place at one time, as one sample of a series does. The channels of a sample,
such as the red, green and blue of a pixel, stay inside the cell and are not an axis.

The readings come from one function of the cell and the reading:

```rust
--8<-- "examples/example_thermal/src/lib.rs:value"
```

A hot spot crosses the plate along cell row 48, one cell every 5 readings. A heater near the lower right corner stays
warm.

## Tiles

A [[chunk]] of a grid track holds one tile: a run of readings and a box of cells. The plate's tiles are 32 cells on
each side, so the plate has 3 tile rows and 4 tile columns. Each tile has its coordinates, such as `(1, 0)`, and the
[[index entry]] of its chunk packs them into one [[tile number]]. The number is a bit field per axis, and the plate's 12
tiles take 4 bits.

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/plate_tiles.svg"
<figcaption>Left: the plate's 12 tiles with their coordinates and tile numbers, and a box query over rows 40 to 56 and
columns 20 to 60, which meets 2 tiles. Right: the levels of space overviews, each half the size of the one before.
Drawn by <code>ww_tape_figures</code> from the plate's layout.</figcaption>
</figure>

A writer takes whole readings of the plate. Its [`GridHandle`] splits each reading into the open chunk of every tile,
and it completes the chunks of all tiles together:

```rust
--8<-- "examples/example_thermal/src/lib.rs:record"
```

A chunk holds 100 readings, so each tile has 6 chunks and the run has 72 in all.
The writer cuts a tile's chunk by readings, bytes or latency, as it does for a series.

## The box query

A viewer that opens the corner of the plate asks for a [[box query]]: a box of cells over a stretch of time.
[`TileLayout::tile_box`] turns the box into a range of tile coordinates along each axis. [`TapeReader::grid_tiles`]
then tests each index entry of the track by shift and mask on the tile number. It reads no chunk to do so:

```rust
--8<-- "examples/example_thermal/src/lib.rs:box_query"
```

The tile at row 1, column 0 holds 6 chunks over the whole run. A box of 16 rows and 40 columns meets 2 tiles.
It finds 2 chunks in the first 10 s, where the whole plate has 12 of them. The query returns index entries.
The viewer reads only the chunks it wants.

```rust
--8<-- "ww_tape_tests/tests/it/book_pages.rs:tile_box"
```

## A region over time

A timeline of one part of the plate needs no cell. The writer keeps a [[summary node]] per tile for every 16 readings
of the tile. The nodes of a tile form the same tree a series has ([Summaries](summaries.md)). A viewer merges the
nodes of the tiles in a box into the timeline of that region with [`region_timeline`]:

```rust
--8<-- "examples/example_thermal/src/lib.rs:region"
```

A node covers a whole tile, so a box that meets a tile part way counts the tile whole. The timeline is at tile
resolution. In the tile at row 1, column 0 the largest value of node 0 is 7770, which is 77.70 °C. It falls below 30 °C
from node 9, when the spot has left the tile. The heater's tile stays between 4008 and 4010 for the whole run.

## Zooming out in space

A viewer that draws the whole plate in 8 columns of pixels needs far fewer cells than the plate has. The writer
therefore keeps [[space overviews|space overview]]. Overview level `k` halves each downsampled axis `k` times.
It keeps every reading.

Each cell of a level holds the smallest value, the largest value and the mean of the cells it covers. A tile's chunk
and its overview chunks go to the tape as one unit. A reader never sees a tile chunk without its overviews, because a
resume drops a torn unit whole.

The levels of the plate:

| Level | Cells | Chunks | Bytes |
|---|---|---|---|
| 0 | 96 × 128 | 72 | 15 MB |
| 1 | 48 × 64 | 72 | 11 MB |
| 2 | 24 × 32 | 72 | 2.8 MB |
| 3 | 12 × 16 | 72 | 703 KB |
| 4 | 6 × 8 | 72 | 184 KB |
| 5 | 3 × 4 | 72 | 55 KB |

The chain with its overviews takes 30 MB, twice the cells as written.
The levels above level 0 add up to about the size of the cells as written.
Each level holds three values per cell on a quarter of the cells.

A viewer picks the level from the cells one pixel covers. [`TileLayout::level_for`] returns the highest level whose cell
still fits in a pixel. The viewer then finds the chunks of that level with a box in that level's cells. It reads the
mean, the smallest or the largest column of each:

```rust
--8<-- "examples/example_thermal/src/lib.rs:view"
```

The views of the plate at reading 300 show the saving. Each view reads the 12 chunks that hold reading 300, and the
level decides their size. The chunks also hold 99 other readings, since `chunk_steps` is 100. That is why the view at
level 0 reads 2.5 MB for one reading:

| Cells per pixel | Level | Cells | Chunks | Bytes |
|---|---|---|---|---|
| 1 | 0 | 96 × 128 | 12 | 2.5 MB |
| 4 | 2 | 24 × 32 | 12 | 463 KB |
| 16 | 4 | 6 × 8 | 12 | 31 KB |
| 32 | 5 | 3 × 4 | 12 | 9.1 KB |

```rust
--8<-- "ww_tape_tests/tests/it/book_pages.rs:overview_level"
```

## Where overviews end

An overview belongs to one tile: it summarises the tile's own cells. A level therefore exists only while each
downsampled tile extent halves evenly. A tile of 32 cells halves 5 times. A tile of 60 cells halves twice, because
60 is 4 times an odd number. Past that level a cell would straddle two tiles.

A tile extent gives as many levels as it has trailing zero bits, at most [`GRID_OVERVIEW_LEVELS_MAX`].

The image sequence shows what this costs. Its frames are 480 × 640 pixels, and each pixel has three channels.
A bright square moves along a gradient.

The same 10 frames, 922 KB each, in three tile sizes, rows × columns:

```rust
--8<-- "examples/example_images/src/lib.rs:tiles"
```

| Tile | Tiles | Levels | Chain | Thumbnail | Square |
|---|---|---|---|---|---|
| 64 × 64 | 8 × 10 | 6 | 21 MB | level 4, 80 chunks, 24 KB | 9 chunks, 112 KB |
| 60 × 60 | 8 × 11 | 2 | 19 MB | level 2, 88 chunks, 187 KB | 4 chunks, 44 KB |
| 240 × 320 | 2 × 2 | 4 | 18 MB | level 4, 4 chunks, 11 KB | 2 chunks, 461 KB |

The thumbnail is the whole frame at 16 cells per pixel. The square is the 100 × 100 pixels it covers in the fourth
frame. Two limits show. The tiles of 60 stop at level 2, so the thumbnail reads about 8 times the bytes that the tiles
of 64 take. The tiles of 240 × 320 read the thumbnail in 4 chunks, but the square needs about 4 times the bytes.

An overview saves bytes, not chunks. A thumbnail at any level reads one overview chunk per tile, so many small tiles
mean many small reads. A tile extent is therefore a choice between a cheap box query and a cheap overview.
A power of two gives the most levels: log2 of the extent, at most [`GRID_OVERVIEW_LEVELS_MAX`].

```rust
--8<-- "ww_tape_tests/tests/it/book_pages.rs:tile_sizes"
```

The [format reference](format.md#fixed-axes-and-tiles) holds the bytes of the tile number, the overview chunks and the
tile summaries.
