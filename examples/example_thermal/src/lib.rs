//! The book's thermal field: a plate of temperature sensors on a 2-D grid, read over time.
//!
//! The plate is 128 sensors wide and 96 high. It reads once per 100 ms, in 0.01 °C. A hot spot of the plate's tool
//! crosses it from left to right, and a heater near the lower right corner stays warm. The plate is one grid track:
//! two fixed axes, `y` and `x`, beside time. [Thermal::record] writes it through a mux, and the functions after it
//! read it the three ways the chapter shows: by box ([Thermal::box_query]), by region over time
//! ([Thermal::region]) and by level ([Thermal::view]).
//!
//! ```
//! use example_thermal::Thermal;
//! let plate = Thermal::plate();
//! let rec = plate.record()?;
//! // the hot spot's tile and its neighbours, over the whole run
//! let found = plate.box_query(&rec, 0..plate.ticks(), 0..32, 0..32)?;
//! assert!(!found.is_empty());
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::indexing_slicing,
    reason = "the case computes the cells and times of a plate it makes itself from its settings, far from any limit, \
              and reads no bytes of others"
)]

use std::error::Error;
use std::ops::Range;

use example_capture::{chain_options, hash, pieces};
use shrink_wrap::prelude::*;
use ww_tape::summary::{SeriesStats, SeriesSummarySpec};
use ww_tape::{
    AxisCoords, ClockOrigin, ClockSpec, ColumnKind, Element, ElementDesc, FixedAxis, GridDef,
    GridTile, Items, Magnitude, MemPieces, Overview, OwnerId, Ratio64, SampleShape, Samples,
    Segment, TapeReader, TileLayout, TrackId, TrackKind, TrackRepeat, TrackSpec, clock_key,
    track_key,
};
use ww_tape_mux::{GridOptions, MuxOptions, TapeMux};
use ww_tape_summary::{RegionNode, region_timeline};

/// The root of the plate's API: its track and clock keys start from it.
pub const API_ROOT: u64 = 0x7e12_0001;
/// The owner of the plate's tape.
pub const OWNER: OwnerId = OwnerId([0x7e; 32]);
/// Time steps of a level-0 summary node: `2^BASE_SHIFT`.
pub const BASE_SHIFT: u8 = 4;
/// Ticks (ns) between two readings: 100 ms.
pub const PERIOD_NS: u64 = 100_000_000;
/// The plate's rest temperature, in 0.01 °C.
pub const AMBIENT: i16 = 2_000;

/// The plate: its size, its tiles and how long it is read.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Thermal {
    /// Sensors along `x`.
    pub width: u32,
    /// Sensors along `y`.
    pub height: u32,
    /// Cells along each axis one tile holds.
    pub tile: u32,
    /// Readings of the whole plate.
    pub steps: u64,
    /// Readings of a chunk.
    pub chunk_steps: u32,
}

/// What [Thermal::record] wrote: the pieces of the chain and what is needed to read the track.
pub struct Recording {
    pub pieces: Vec<Vec<u8>>,
    pub id: TrackId,
    pub layout: TileLayout,
}

impl Thermal {
    /// 128 × 96 sensors in tiles of 32 × 32, for 60 s: 600 readings.
    pub const fn plate() -> Self {
        Self {
            width: 128,
            height: 96,
            tile: 32,
            steps: 600,
            chunk_steps: 100,
        }
    }

    /// The tick (ns) after the last reading.
    pub fn ticks(&self) -> i64 {
        (self.steps * PERIOD_NS) as i64
    }

    /// The time of reading `step`, in ticks.
    pub fn tick(&self, step: u64) -> i64 {
        (step * PERIOD_NS) as i64
    }

    // --8<-- [start:axes]
    /// The plate's two fixed axes: rows, then columns, each a regular axis of one cell per sensor and each halved by
    /// space overviews.
    pub fn axes(&self) -> [FixedAxis<'static>; 2] {
        let axis = |name, extent| FixedAxis {
            name,
            extent: UNib32(extent),
            tile_extent: UNib32(self.tile),
            unit: None,
            scale: Magnitude::ONE,
            coords: AxisCoords::Regular { origin: 0, step: 1 },
            downsample: true,
        };
        [axis("y", self.height), axis("x", self.width)]
    }
    // --8<-- [end:axes]

    // --8<-- [start:def]
    /// The grid's definition: one `i16` per cell, a reading per period, in 0.01 °C, with a summary node per 16
    /// readings of a tile.
    pub fn def<'a>(&self, axes: &'a [FixedAxis<'static>]) -> Option<GridDef<'a>> {
        let mut def = GridDef::of::<i16>(1, Ratio64::new(PERIOD_NS, 1)?);
        def.scale = Magnitude::pow10(-2); // a stored 2 000 is 20.00 °C
        def.summaries = Some(SeriesSummarySpec {
            base_shift: BASE_SHIFT,
            stats: SeriesStats::all(ElementDesc::of(Element::I16)),
            clip: None,
        });
        def.axes = RefVec::Slice { slice: axes };
        Some(def)
    }
    // --8<-- [end:def]

    /// The clock the readings are stamped with: ns since the run starts.
    pub fn clock() -> ClockSpec<'static> {
        ClockSpec {
            key: clock_key(API_ROOT, &[Segment::Name("clock")], 1),
            rate: Magnitude::pow10(9).into(),
            resolution: Magnitude::pow10(-9).into(),
            origin: ClockOrigin::Boot,
            label: "plate ns",
        }
    }

    /// The tile layout the definition gives, which a reader names its boxes by.
    pub fn layout(&self) -> Option<TileLayout> {
        let axes = self.axes();
        self.def(&axes)?.tile_layout().ok()
    }

    // --8<-- [start:value]
    /// The reading of cell (`y`, `x`) at `step`, in 0.01 °C: the ambient with a little noise, a hot spot that moves
    /// along the plate's middle row at one cell per 5 steps, and a heater of the lower right corner.
    pub fn value(&self, step: u64, y: u32, x: u32) -> i16 {
        let noise = (hash(7, (step << 24) | (u64::from(y) << 12) | u64::from(x)) % 11) as i16;
        let spot = (14 + step / 5) as i64;
        let heat = |cy: i64, cx: i64, reach: i64, per: i64| {
            let (dy, dx) = (i64::from(y) - cy, i64::from(x) - cx);
            (reach - dy * dy - dx * dx).max(0) * per
        };
        let hot = heat(i64::from(self.height) / 2, spot, 144, 40);
        let heater = heat(
            i64::from(self.height) - 12,
            i64::from(self.width) - 12,
            100,
            20,
        );
        AMBIENT + noise + (hot + heater) as i16
    }
    // --8<-- [end:value]

    /// Readings `steps` of the whole plate, a time step after another: rows, then columns.
    fn readings(&self, steps: Range<u64>) -> Vec<i16> {
        let mut out = Vec::new();
        for step in steps {
            for y in 0..self.height {
                for x in 0..self.width {
                    out.push(self.value(step, y, x));
                }
            }
        }
        out
    }

    // --8<-- [start:record]
    /// Write the run through a mux into a chain in memory and close it.
    pub fn record(&self) -> Result<Recording, Box<dyn Error>> {
        let tapes = MemPieces::new();
        let mux = TapeMux::create(
            tapes.clone(),
            OWNER,
            0,
            &[],
            chain_options(),
            MuxOptions::default(),
        )?;
        let clock = mux.define_clock(Self::clock())?;
        let axes = self.axes();
        let spec = TrackSpec {
            key: track_key(API_ROOT, &[Segment::Name("plate")]),
            clock,
            dimensions: RefVec::Slice { slice: &[] },
            items: Items::numbers::<i16>(None),
            label: "plate",
            repeat: TrackRepeat::Stream,
            kind: TrackKind::Series(self.def(&axes).ok_or("the period")?),
            relations: RefVec::Slice { slice: &[] },
            owner: [0; 32],
        };
        let id = spec.key.id();
        let track = mux.define_track(spec)?;
        // one handle for the whole plate: it splits each reading into the open chunk of every tile
        let opts = GridOptions {
            chunk_steps: self.chunk_steps,
            latency: None,
            ..GridOptions::default()
        };
        let mut plate = mux.grid::<i16>(track, opts)?;
        for first in (0..self.steps).step_by(50) {
            plate.push(&self.readings(first..(first + 50).min(self.steps)))?;
        }
        plate.finish()?;
        let closed = mux.close()?;
        Ok(Recording {
            pieces: pieces(&tapes, &closed)?,
            id,
            layout: self.layout().ok_or("the layout")?,
        })
    }
    // --8<-- [end:record]

    // --8<-- [start:box_query]
    /// The chunks of the cells `ys` × `xs` over the ticks `time`, found from the index alone.
    pub fn box_query(
        &self,
        rec: &Recording,
        time: Range<i64>,
        ys: Range<u32>,
        xs: Range<u32>,
    ) -> Result<Vec<GridTile>, Box<dyn Error>> {
        let tiles = rec.layout.tile_box(&[ys, xs]).ok_or("two axes")?;
        let mut found = Vec::new();
        for (i, piece) in rec.pieces.iter().enumerate() {
            let reader = TapeReader::open(&piece[..])?;
            found.extend(reader.grid_tiles(i, rec.id, time.clone(), &tiles)?);
        }
        Ok(found)
    }
    // --8<-- [end:box_query]

    // --8<-- [start:region]
    /// The timeline of the cells `ys` × `xs` at summary level `level`: per node, the tiles the box meets merged.
    pub fn region(
        &self,
        rec: &Recording,
        ys: Range<u32>,
        xs: Range<u32>,
        level: u8,
    ) -> Result<Vec<RegionNode>, Box<dyn Error>> {
        let tiles = rec.layout.tile_box(&[ys, xs]).ok_or("two axes")?;
        let mut nodes = Vec::new();
        for (i, piece) in rec.pieces.iter().enumerate() {
            let reader = TapeReader::open(&piece[..])?;
            nodes.extend(region_timeline(
                &reader,
                i,
                rec.id,
                i64::MIN..i64::MAX,
                &tiles,
                level,
            )?);
        }
        Ok(nodes)
    }
    // --8<-- [end:region]

    // --8<-- [start:view]
    /// A view of the whole plate at one reading: the level for `cells_per_pixel`, the chunks of that level at the
    /// reading's time, and the smallest, largest and mean value of every cell of that level, row by row.
    pub fn view(
        &self,
        rec: &Recording,
        step: u64,
        cells_per_pixel: f64,
    ) -> Result<View, Box<dyn Error>> {
        let level = rec.layout.level_for(&[cells_per_pixel, cells_per_pixel]);
        let at = rec.layout.overview(level).ok_or("a level of the grid")?;
        let [(h, _), (w, _)] = [at.extents()[0], at.extents()[1]];
        let tiles = at.tile_box(&[0..h, 0..w]).ok_or("two axes")?;
        let time = self.tick(step)..self.tick(step) + 1;
        let shape = SampleShape::new(ElementDesc::of(Element::I16), 1);
        let (mut bytes, mut chunks) = (0u64, 0u64);
        let mut cells = vec![[0i16; 3]; (h * w) as usize];
        let mut buf = Vec::new();
        for (i, piece) in rec.pieces.iter().enumerate() {
            let reader = TapeReader::open(&piece[..])?;
            for found in reader.grid_tiles(i, rec.id, time.clone(), &tiles)? {
                let block = reader.read_block(i, &found.entry, &mut buf)?;
                let chunk = block.chunk().ok_or("a chunk")?;
                let head = chunk.head();
                bytes += u64::from(found.entry.len);
                chunks += 1;
                let (rows, cols) = (
                    at.cell_range(0, found.tile.as_slice()[0]),
                    at.cell_range(1, found.tile.as_slice()[1]),
                );
                let (rows, cols) = (rows.ok_or("a row range")?, cols.ok_or("a column range")?);
                let n = (rows.len() * cols.len()) as usize;
                let nth = (step - head.first_item.get()) as usize * n;
                // the chunk is of level 0 when the view is: its samples are the cells themselves
                let mut lo = vec![0i16; n];
                let mut hi = vec![0i16; n];
                let mut mean = vec![0i16; n];
                if level == 0 {
                    let column = chunk.find(ColumnKind::Samples, 0).ok_or("samples")?;
                    let samples = Samples::of(&column, shape, n * head.count as usize)?;
                    let mut all = vec![0i16; n * head.count as usize];
                    samples.decode(0, &mut all)?;
                    lo.copy_from_slice(&all[nth..nth + n]);
                    hi.copy_from_slice(&all[nth..nth + n]);
                    mean.copy_from_slice(&all[nth..nth + n]);
                } else {
                    let o = Overview::of(&chunk, &rec.layout, shape)?;
                    for (part, col) in [
                        (o.min(), &mut lo),
                        (o.max(), &mut hi),
                        (o.mean(), &mut mean),
                    ] {
                        let mut all = vec![0i16; n * head.count as usize];
                        part.decode(0, &mut all)?;
                        col.copy_from_slice(&all[nth..nth + n]);
                    }
                }
                for (k, (r, c)) in rows
                    .clone()
                    .flat_map(|r| cols.clone().map(move |c| (r, c)))
                    .enumerate()
                {
                    cells[(r * w + c) as usize] = [lo[k], hi[k], mean[k]];
                }
            }
        }
        Ok(View {
            level,
            width: w,
            height: h,
            chunks,
            bytes,
            cells,
        })
    }
    // --8<-- [end:view]
}

/// A view of the plate at one reading, as [Thermal::view] reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    /// The overview level the view was read from.
    pub level: u8,
    /// Cells of the level along `x`.
    pub width: u32,
    /// Cells of the level along `y`.
    pub height: u32,
    /// Chunks read.
    pub chunks: u64,
    /// Bytes of the blocks of those chunks.
    pub bytes: u64,
    /// Each cell's smallest, largest and mean value, row by row.
    pub cells: Vec<[i16; 3]>,
}

/// A row of the story: what the whole plate takes at one overview level.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LevelRow {
    pub level: u8,
    /// Cells of the level along `y` and `x`.
    pub cells: (u32, u32),
    /// Chunks of the level over the whole run, and their bytes.
    pub chunks: u64,
    pub bytes: u64,
}

/// A row of the story: what a view of `cells_per_pixel` reads.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ViewRow {
    pub cells_per_pixel: f64,
    pub level: u8,
    pub cells: (u32, u32),
    pub chunks: u64,
    pub bytes: u64,
}

/// The numbers the chapter quotes, measured from a recording of the plate.
#[derive(Clone, Debug)]
pub struct Story {
    /// Tiles along `y` and `x`, and the bits of the tile number.
    pub tiles: (u32, u32),
    pub tile_bits: u32,
    /// Overview levels above the cells as written.
    pub levels: u8,
    /// Bytes of the cells as written, and of the chain with its overviews.
    pub raw_bytes: u64,
    pub chain_bytes: u64,
    /// Chunks of the whole plate over the whole run; of the one tile in the hot row's first column; of a box over
    /// the first 10 s.
    pub whole: usize,
    pub one_tile: usize,
    pub whole_first: usize,
    pub box_chunks: usize,
    /// Each overview level over the whole run, and the views.
    pub level_rows: Vec<LevelRow>,
    pub views: Vec<ViewRow>,
    /// The largest value of each level-0 node of the tile the hot spot starts in, and of the heater's tile.
    pub sweep_peaks: Vec<i64>,
    pub heater_peaks: Vec<i64>,
}

impl Thermal {
    /// Record the plate and measure what the chapter quotes.
    pub fn story(&self) -> Result<Story, Box<dyn Error>> {
        let rec = self.record()?;
        let l = rec.layout;
        let (h, w) = (self.height, self.width);
        let mut level_rows = Vec::new();
        for level in 0..=l.overview_levels() {
            let at = l.overview(level).ok_or("a level")?;
            let [(ch, _), (cw, _)] = [at.extents()[0], at.extents()[1]];
            let tiles = at.tile_box(&[0..ch, 0..cw]).ok_or("two axes")?;
            let (mut chunks, mut bytes) = (0, 0);
            for (i, piece) in rec.pieces.iter().enumerate() {
                let reader = TapeReader::open(&piece[..])?;
                for t in reader.grid_tiles(i, rec.id, 0..self.ticks(), &tiles)? {
                    chunks += 1;
                    bytes += u64::from(t.entry.len);
                }
            }
            level_rows.push(LevelRow {
                level,
                cells: (ch, cw),
                chunks,
                bytes,
            });
        }
        let mut views = Vec::new();
        for cells_per_pixel in [1.0, 4.0, 16.0, 32.0] {
            let v = self.view(&rec, 300, cells_per_pixel)?;
            views.push(ViewRow {
                cells_per_pixel,
                level: v.level,
                cells: (v.height, v.width),
                chunks: v.chunks,
                bytes: v.bytes,
            });
        }
        let peaks = |ys, xs| -> Result<Vec<i64>, Box<dyn Error>> {
            Ok(self
                .region(&rec, ys, xs, 0)?
                .iter()
                .map(|n| match n.stats.channels[0].max[0] {
                    ww_tape_summary::Value::I64(v) => v,
                    _ => 0,
                })
                .collect())
        };
        let (_, tile_x) = (self.height, self.tile);
        let hot_row = self.tile..2 * self.tile;
        let part = self.box_query(&rec, 0..self.tick(100), 40..56, 20..60)?;
        Ok(Story {
            tiles: (l.tiles()[0], l.tiles()[1]),
            tile_bits: l.bits(),
            levels: l.overview_levels(),
            raw_bytes: self.steps * u64::from(h) * u64::from(w) * 2,
            chain_bytes: rec.pieces.iter().map(|p| p.len() as u64).sum(),
            whole: self.box_query(&rec, 0..self.ticks(), 0..h, 0..w)?.len(),
            whole_first: self.box_query(&rec, 0..self.tick(100), 0..h, 0..w)?.len(),
            one_tile: self
                .box_query(&rec, 0..self.ticks(), hot_row.clone(), 0..tile_x)?
                .len(),
            box_chunks: part.len(),
            level_rows,
            views,
            sweep_peaks: peaks(hot_row, 0..tile_x)?,
            heater_peaks: peaks(h - self.tile..h, w - self.tile..w)?,
        })
    }
}
