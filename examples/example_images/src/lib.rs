//! The book's image sequence: frames of 480 × 640 pixels (rows × columns) on a 2-D grid over time.
//!
//! Each pixel is a cell of three channels (red, green, blue, a byte each), and a frame is one time step of the
//! grid. A bright square moves along a gradient. [Images::record] writes the frames through a mux, in tiles of any
//! size, and [Images::view] reads a box of a frame at the space overview level its pixels ask for.
//!
//! ```
//! use example_images::Images;
//! let images = Images::frames();
//! let rec = images.record()?;
//! // a thumbnail of frame 3 at 16 cells per pixel reads far less than the frame
//! let thumb = images.view(&rec, 3, 0..480, 0..640, 16.0)?;
//! assert_eq!(thumb.level, 4);
//! assert!(thumb.bytes < images.frame_bytes());
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::indexing_slicing,
    reason = "the case computes the cells and times of frames it makes itself from its settings, far from any limit, \
              and reads no bytes of others"
)]

use std::error::Error;
use std::ops::Range;

use example_capture::{chain_options, hash, pieces};
use shrink_wrap::prelude::*;
use ww_tape::{
    AxisCoords, ClockOrigin, ClockSpec, ColumnKind, Element, ElementDesc, FixedAxis, GridDef,
    Items, Magnitude, MemPieces, Overview, OwnerId, Ratio64, SampleShape, Samples, Segment,
    TapeReader, TileLayout, TrackId, TrackKind, TrackRepeat, TrackSpec, clock_key, track_key,
};
use ww_tape_mux::{GridOptions, MuxOptions, TapeMux};

/// The root of the camera's API: its track and clock keys start from it.
pub const API_ROOT: u64 = 0x1a9e_0001;
/// The owner of the camera's tape.
pub const OWNER: OwnerId = OwnerId([0x1a; 32]);
/// Ticks (ns) between two frames: 100 ms, 10 frames a second.
pub const PERIOD_NS: u64 = 100_000_000;
/// The frame the story reads, counting from 0.
pub const STORY_FRAME: u64 = 3;
/// Cells a pixel of the story's thumbnail covers along each axis.
pub const THUMB_CELLS_PER_PIXEL: f64 = 16.0;
/// The pixels the story reads at full size: where the bright square is in [STORY_FRAME].
pub const SQUARE_ROWS: Range<u32> = 190..290;
pub const SQUARE_COLUMNS: Range<u32> = 120..220;
/// Bytes of a pixel: red, green and blue.
pub const CHANNELS: u32 = 3;

/// The camera: the size of its frames, their tiles and how many it records.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Images {
    /// Pixel rows of a frame.
    pub height: u32,
    /// Pixel columns of a frame.
    pub width: u32,
    /// Rows of a tile.
    pub tile_y: u32,
    /// Columns of a tile.
    pub tile_x: u32,
    /// Frames recorded.
    pub frames: u64,
    /// Frames of a chunk.
    pub chunk_steps: u32,
}

/// What [Images::record] wrote: the pieces of the chain and what is needed to read the track.
pub struct Recording {
    pub pieces: Vec<Vec<u8>>,
    pub id: TrackId,
    pub layout: TileLayout,
}

impl Images {
    /// 10 frames of 480 × 640 in tiles of 64 × 64, a chunk a frame.
    pub const fn frames() -> Self {
        Self {
            height: 480,
            width: 640,
            tile_y: 64,
            tile_x: 64,
            frames: 10,
            chunk_steps: 1,
        }
    }

    /// The same frames in tiles of `tile_y` × `tile_x`.
    #[must_use]
    pub const fn tiles(self, tile_y: u32, tile_x: u32) -> Self {
        Self {
            tile_y,
            tile_x,
            ..self
        }
    }

    /// Bytes of a frame as written.
    pub fn frame_bytes(&self) -> u64 {
        u64::from(self.height) * u64::from(self.width) * u64::from(CHANNELS)
    }

    /// The tick of frame `frame`.
    pub fn tick(&self, frame: u64) -> i64 {
        (frame * PERIOD_NS) as i64
    }

    // --8<-- [start:axes]
    /// The frame's two fixed axes: pixel rows, then pixel columns, each halved by space overviews.
    pub fn axes(&self) -> [FixedAxis<'static>; 2] {
        let axis = |name, extent, tile| FixedAxis {
            name,
            extent: UNib32(extent),
            tile_extent: UNib32(tile),
            unit: None,
            scale: Magnitude::ONE,
            coords: AxisCoords::Regular { origin: 0, step: 1 },
            downsample: true,
        };
        [
            axis("y", self.height, self.tile_y),
            axis("x", self.width, self.tile_x),
        ]
    }
    // --8<-- [end:axes]

    // --8<-- [start:def]
    /// The grid's definition: a pixel is a cell of three `u8` channels, a frame per period. The image has no time
    /// pyramid: only its space overviews.
    pub fn def<'a>(&self, axes: &'a [FixedAxis<'static>]) -> Option<GridDef<'a>> {
        let mut def = GridDef::of::<u8>(CHANNELS, Ratio64::new(PERIOD_NS, 1)?);
        def.axes = RefVec::Slice { slice: axes };
        Some(def)
    }
    // --8<-- [end:def]

    /// The clock the frames are stamped with: ns since the camera starts.
    pub fn clock() -> ClockSpec<'static> {
        ClockSpec {
            key: clock_key(API_ROOT, &[Segment::Name("clock")], 1),
            rate: Magnitude::pow10(9).into(),
            resolution: Magnitude::pow10(-9).into(),
            origin: ClockOrigin::Boot,
            label: "camera ns",
        }
    }

    /// The tile layout the definition gives.
    pub fn layout(&self) -> Option<TileLayout> {
        let axes = self.axes();
        self.def(&axes)?.tile_layout().ok()
    }

    // --8<-- [start:pixel]
    /// Channel `c` of the pixel (`y`, `x`) of frame `frame`: a gradient with a little noise, and a bright square of
    /// 100 × 100 pixels that moves 40 pixels to the right in each frame.
    pub fn pixel(&self, frame: u64, y: u32, x: u32, c: u32) -> u8 {
        let (h, w) = (u64::from(self.height), u64::from(self.width));
        let inside =
            (190..290).contains(&y) && (frame * 40..frame * 40 + 100).contains(&u64::from(x));
        if inside {
            return 255;
        }
        let ramp = match c {
            0 => u64::from(x) * 200 / w,
            1 => u64::from(y) * 200 / h,
            _ => 60,
        };
        let noise = hash(
            3,
            (frame << 32) | (u64::from(y) << 16) | (u64::from(x) << 2) | u64::from(c),
        ) % 8;
        (ramp + noise) as u8
    }
    // --8<-- [end:pixel]

    /// The frames `frames`, one after another: rows, then columns, then the pixel's channels.
    fn readings(&self, frames: Range<u64>) -> Vec<u8> {
        let mut out = Vec::new();
        for frame in frames {
            for y in 0..self.height {
                for x in 0..self.width {
                    for c in 0..CHANNELS {
                        out.push(self.pixel(frame, y, x, c));
                    }
                }
            }
        }
        out
    }

    // --8<-- [start:record]
    /// Write the frames through a mux into a chain in memory and close it.
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
            key: track_key(API_ROOT, &[Segment::Name("camera")]),
            clock,
            dimensions: RefVec::Slice { slice: &[] },
            items: Items::numbers::<u8>(None),
            label: "camera",
            repeat: TrackRepeat::Stream,
            kind: TrackKind::Series(self.def(&axes).ok_or("the period")?),
            relations: RefVec::Slice { slice: &[] },
            owner: [0; 32],
            defined_twice: false,
        };
        let id = spec.key.id();
        let track = mux.define_track(spec)?;
        let opts = GridOptions {
            chunk_steps: self.chunk_steps,
            latency: None,
            ..GridOptions::default()
        };
        let mut camera = mux.grid::<u8>(track, opts)?;
        for frame in 0..self.frames {
            camera.push(&self.readings(frame..frame + 1))?;
        }
        camera.finish()?;
        let closed = mux.close()?;
        Ok(Recording {
            pieces: pieces(&tapes, &closed)?,
            id,
            layout: self.layout().ok_or("the layout")?,
        })
    }
    // --8<-- [end:record]

    // --8<-- [start:view]
    /// The pixels `ys` × `xs` of frame `frame` at the level `cells_per_pixel` asks for: the chunks of the tiles the
    /// box meets at that level, and the mean of each cell they hold.
    pub fn view(
        &self,
        rec: &Recording,
        frame: u64,
        ys: Range<u32>,
        xs: Range<u32>,
        cells_per_pixel: f64,
    ) -> Result<View, Box<dyn Error>> {
        let level = rec.layout.level_for(&[cells_per_pixel, cells_per_pixel]);
        let at = rec.layout.overview(level).ok_or("a level of the frame")?;
        let [(h, _), (w, _)] = [at.extents()[0], at.extents()[1]];
        // the box in cells of the level: each cell covers 2^level pixels
        let cells = |r: Range<u32>| r.start >> level..r.end.div_ceil(1 << level);
        let tiles = at.tile_box(&[cells(ys), cells(xs)]).ok_or("two axes")?;
        let time = self.tick(frame)..self.tick(frame) + 1;
        let shape = SampleShape::new(ElementDesc::of(Element::U8), CHANNELS);
        let (mut bytes, mut chunks) = (0, 0);
        let mut mean = vec![0u8; (h * w * CHANNELS) as usize];
        let mut buf = Vec::new();
        for (i, piece) in rec.pieces.iter().enumerate() {
            let reader = TapeReader::open(&piece[..])?;
            for found in reader.grid_tiles(i, rec.id, time.clone(), &tiles)? {
                let block = reader.read_block(i, &found.entry, &mut buf)?;
                let chunk = block.chunk().ok_or("a chunk")?;
                let head = chunk.head();
                bytes += u64::from(found.entry.len);
                chunks += 1;
                let rows = at.cell_range(0, found.tile.as_slice()[0]).ok_or("rows")?;
                let cols = at
                    .cell_range(1, found.tile.as_slice()[1])
                    .ok_or("columns")?;
                let n = rows.len() * cols.len();
                let all = n * head.count as usize;
                // a chunk of level 0 holds the pixels, an overview its min, max and mean
                let mut values = vec![0u8; all * CHANNELS as usize];
                if level == 0 {
                    let column = chunk.find(ColumnKind::Samples, 0).ok_or("samples")?;
                    Samples::of(&column, shape, all)?.decode(0, &mut values)?;
                } else {
                    Overview::of(&chunk, &rec.layout, shape)?
                        .mean()
                        .decode(0, &mut values)?;
                }
                let first = (frame - head.first_item.get()) as usize * n * CHANNELS as usize;
                let mut at_cell = first;
                for r in rows {
                    for c in cols.clone() {
                        let to = ((r * w + c) * CHANNELS) as usize;
                        mean[to..to + CHANNELS as usize]
                            .copy_from_slice(&values[at_cell..at_cell + CHANNELS as usize]);
                        at_cell += CHANNELS as usize;
                    }
                }
            }
        }
        Ok(View {
            level,
            width: w,
            height: h,
            chunks,
            bytes,
            mean,
        })
    }
    // --8<-- [end:view]
}

/// A view of a frame, as [Images::view] reads it.
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
    /// The mean of each cell of the level, row by row, three channels each; cells outside the box stay 0.
    pub mean: Vec<u8>,
}

/// One way of cutting the frames into tiles, and what it costs the two reads the chapter compares.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TileRow {
    /// Rows and columns of a tile.
    pub tile: (u32, u32),
    /// Tiles along `y` and `x`, and the bits of the tile number.
    pub tiles: (u32, u32),
    pub tile_bits: u32,
    /// Overview levels above the pixels as written.
    pub levels: u8,
    /// The level, chunks and bytes of a thumbnail of the whole frame at 16 cells per pixel.
    pub thumb_level: u8,
    pub thumb_chunks: u64,
    pub thumb_bytes: u64,
    /// The chunks and bytes of the square's 100 × 100 pixels at full resolution.
    pub box_chunks: u64,
    pub box_bytes: u64,
    /// Bytes of the whole chain, with its overviews.
    pub chain_bytes: u64,
}

/// The numbers the chapter quotes: the same frames in several tile sizes.
#[derive(Clone, Debug)]
pub struct Story {
    /// Bytes of a frame as written, and of all the frames.
    pub frame_bytes: u64,
    pub raw_bytes: u64,
    pub rows: Vec<TileRow>,
}

impl Images {
    // --8<-- [start:tiles]
    /// The tile sizes the chapter compares, rows × columns: 64 × 64, 60 × 60 and 240 × 320.
    pub const TILES: [(u32, u32); 3] = [(64, 64), (60, 60), (240, 320)];
    // --8<-- [end:tiles]

    /// Record the frames in each of [Self::TILES] and measure the two reads on [STORY_FRAME].
    pub fn story(&self) -> Result<Story, Box<dyn Error>> {
        let mut rows = Vec::new();
        for (tile_y, tile_x) in Self::TILES {
            let images = self.tiles(tile_y, tile_x);
            let rec = images.record()?;
            let l = rec.layout;
            let thumb = images.view(
                &rec,
                STORY_FRAME,
                0..self.height,
                0..self.width,
                THUMB_CELLS_PER_PIXEL,
            )?;
            let square = images.view(&rec, STORY_FRAME, SQUARE_ROWS, SQUARE_COLUMNS, 1.0)?;
            rows.push(TileRow {
                tile: (tile_y, tile_x),
                tiles: (l.tiles()[0], l.tiles()[1]),
                tile_bits: l.bits(),
                levels: l.overview_levels(),
                thumb_level: thumb.level,
                thumb_chunks: thumb.chunks,
                thumb_bytes: thumb.bytes,
                box_chunks: square.chunks,
                box_bytes: square.bytes,
                chain_bytes: rec.pieces.iter().map(|p| p.len() as u64).sum(),
            });
        }
        Ok(Story {
            frame_bytes: self.frame_bytes(),
            raw_bytes: self.frame_bytes() * self.frames,
            rows,
        })
    }
}
