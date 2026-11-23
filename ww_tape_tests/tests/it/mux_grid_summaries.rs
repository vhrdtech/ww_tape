//! A grid's tile trees on the writing thread (FMT-15): each tile's summaries are those a tree of its cells alone
//! builds, a region's timeline reads them back as a naive model of the cells has them, and a mux crashed anywhere
//! rebuilds the tile trees of the torn piece and writes the bytes of the run without the crash.

use std::ops::Range;

use shrink_wrap::prelude::*;
use ww_tape::{
    AxisCoords, BlockKind, ClockNum, Element, ElementDesc, FileBlocks, FixedAxis, GridDef, ItemNum,
    Items, Magnitude, MemPieces, OwnerId, Ratio64, Recovery, TapeReader, TileCoords, TileLayout,
    TrackId, TrackKind, TrackNum, TrackRepeat, TrackSpec, track_key,
};
use ww_tape_mux::{GridHandle, GridOptions, MuxOptions, TapeMux};
use ww_tape_summary::{SeriesStats, SeriesSummarySpec, Value, region_timeline};

use crate::mux_handles::{clock, options};
use crate::mux_multi::{pieces, plant};
use crate::resume::cut_points;

const OWNER: OwnerId = OwnerId([0x47; 32]);
const CHANNELS: u32 = 2;
const PERIOD: i64 = 1_000;
const START: i64 = 0;
/// Fixed axes of `(extent, tile extent)`: 2 × 2 tiles, the first axis' last tile 2 cells short.
const EXTENTS: [(u32, u32); 2] = [(5, 3), (4, 2)];
/// A level-0 node holds 2^4 time steps.
const BASE_SHIFT: u8 = 4;
/// Time steps of a chunk.
const CHUNK_STEPS: u32 = 64;
/// Batches of [BATCH] time steps each tile is pushed.
const BATCHES: u64 = 7;
const BATCH: u64 = 128;
/// Chunks of a piece before the chain cuts it: the first piece full, the second the rest.
const CUT: u64 = 32;

fn period() -> Ratio64 {
    Ratio64::int(PERIOD as u64).unwrap()
}

fn grid_axes() -> &'static [FixedAxis<'static>] {
    let axes: Vec<FixedAxis<'static>> = EXTENTS
        .iter()
        .map(|&(extent, tile)| FixedAxis {
            name: "x",
            extent: UNib32(extent),
            tile_extent: UNib32(tile),
            unit: None,
            scale: Magnitude::ONE,
            coords: AxisCoords::Regular { origin: 0, step: 1 },
            downsample: true,
        })
        .collect();
    Box::leak(axes.into_boxed_slice())
}

/// A grid of i32 on [CHANNELS] channels, every statistic summarised from nodes of 16 time steps.
fn grid_track(clock: ClockNum) -> TrackSpec<'static> {
    let mut def = GridDef::of::<i32>(CHANNELS, period());
    def.axes = RefVec::Slice { slice: grid_axes() };
    def.summaries = Some(SeriesSummarySpec {
        base_shift: BASE_SHIFT,
        stats: SeriesStats::all(ElementDesc::of(Element::I32)),
        clip: None,
    });
    TrackSpec {
        key: track_key(0x47, &[ww_tape::Segment::Name("field")]),
        clock,
        dimensions: RefVec::Slice { slice: &[] },
        items: Items::numbers::<i32>(None),
        label: "field",
        repeat: TrackRepeat::Stream,
        kind: TrackKind::Series(def),
        relations: RefVec::Slice { slice: &[] },
        owner: [0; 32],
    }
}

fn layout() -> TileLayout {
    let TrackKind::Series(def) = grid_track(ClockNum::new(1)).kind else {
        unreachable!("a grid track")
    };
    def.tile_layout().unwrap()
}

/// The tiles of the grid, row-major.
fn tiles() -> Vec<TileCoords> {
    let l = layout();
    let mut out = vec![];
    for a in 0..l.tiles()[0] {
        for b in 0..l.tiles()[1] {
            out.push(TileCoords::new(&[a, b]).unwrap());
        }
    }
    out
}

/// The cells of tile `t`, one range per axis.
fn cells_of(t: &TileCoords) -> Vec<Range<u32>> {
    let l = layout();
    (0..2)
        .map(|a| l.cell_range(a, t.as_slice()[a]).unwrap())
        .collect()
}

/// Channel `ch` of cell `(x, y)` at time step `t`.
fn value(t: u64, x: u32, y: u32, ch: u32) -> i32 {
    ((t * 7919 + u64::from(x) * 104_729 + u64::from(y) * 1_009 + u64::from(ch) * 31) % 40_009)
        as i32
        - 20_000
}

/// Time steps `steps` of the box `cells`: row-major, each cell's channels interleaved.
fn box_steps(steps: Range<u64>, cells: &[Range<u32>]) -> Vec<i32> {
    let mut out = vec![];
    for t in steps {
        for x in cells[0].clone() {
            for y in cells[1].clone() {
                for ch in 0..CHANNELS {
                    out.push(value(t, x, y, ch));
                }
            }
        }
    }
    out
}

/// A tile's handle, its cells along each fixed axis and the next time step it writes.
type TileHandle = (GridHandle<MemPieces, i32>, Vec<Range<u32>>, u64);

/// Write the grid on `mux`, a handle per tile, each from its time step in `from`, then close.
fn write(mux: TapeMux<MemPieces>, track: TrackNum, from: &[u64]) {
    let mut handles: Vec<TileHandle> = tiles()
        .iter()
        .zip(from)
        .map(|(t, &first)| {
            let cells = cells_of(t);
            let h = mux
                .grid_box::<i32>(
                    track,
                    &cells,
                    GridOptions {
                        chunk_steps: CHUNK_STEPS,
                        latency: None,
                        first_item: ItemNum::new(first),
                        first_tick: START + first as i64 * PERIOD,
                        ..GridOptions::default()
                    },
                )
                .unwrap();
            // the grid the run started with, from time step 0 on
            let mut h = h;
            h.regrid(ww_tape::SeriesGrid::start(0, START, Magnitude::ONE))
                .unwrap();
            (h, cells, first)
        })
        .collect();
    for b in 0..BATCHES {
        for (h, cells, first) in &mut handles {
            let from = (b * BATCH).max(*first);
            let to = (b + 1) * BATCH;
            if from < to {
                h.push(&box_steps(from..to, cells)).unwrap();
            }
        }
    }
    for (h, ..) in handles {
        h.finish().unwrap();
    }
    mux.close().unwrap();
}

fn define(mux: &TapeMux<MemPieces>) -> (TrackNum, TrackId) {
    let c = mux.define_clock(clock()).unwrap();
    let spec = grid_track(c);
    let id = spec.key.id();
    (mux.define_track(spec).unwrap(), id)
}

fn whole_run() -> (Vec<Vec<u8>>, TrackId) {
    let mem = MemPieces::new();
    let mux = TapeMux::create(
        mem.clone(),
        OWNER,
        0,
        &[],
        options(CUT),
        MuxOptions::default(),
    )
    .unwrap();
    let (track, id) = define(&mux);
    write(mux, track, &[0; 4]);
    (pieces(&mem), id)
}

/// Each tile's timeline read back from the whole run is the naive statistics of its cells, node by node, at levels
/// 0 and 1; the whole grid's is that of every cell.
#[test]
fn a_mux_grid_summarises_each_tile() {
    let (pieces, id) = whole_run();
    assert_eq!(pieces.len(), 2);
    let l = layout();
    let steps = BATCHES * BATCH;
    let whole: Vec<Range<u32>> = EXTENTS.iter().map(|(e, _)| 0..*e).collect();
    let mut boxes: Vec<Vec<Range<u32>>> = tiles().iter().map(cells_of).collect();
    boxes.push(whole);
    for cells in boxes {
        let b = l.tile_box(&cells).unwrap();
        for level in [0u8, 1] {
            let shift = BASE_SHIFT + 4 * level;
            // a node cut by the piece's end has a part in each piece
            let mut got: std::collections::BTreeMap<u64, Vec<ww_tape_summary::RegionNode>> =
                Default::default();
            for p in &pieces {
                let r = TapeReader::open(&p[..]).unwrap();
                for n in region_timeline(&r, 0, id, i64::MIN..i64::MAX, &b, level).unwrap() {
                    got.entry(n.node).or_default().push(n);
                }
            }
            assert_eq!(got.len() as u64, steps.div_ceil(1 << shift));
            for (node, parts) in got {
                let from = node << shift;
                let to = ((node + 1) << shift).min(steps);
                for ch in 0..CHANNELS {
                    let values: Vec<i64> = (from..to)
                        .flat_map(|t| {
                            let cells = cells.clone();
                            cells[0].clone().flat_map(move |x| {
                                cells[1].clone().map(move |y| i64::from(value(t, x, y, ch)))
                            })
                        })
                        .collect();
                    let lo = parts
                        .iter()
                        .map(|p| match p.stats.channels[ch as usize].min[0] {
                            Value::I64(v) => v,
                            v => panic!("{v:?}"),
                        })
                        .min()
                        .unwrap();
                    let hi = parts
                        .iter()
                        .map(|p| match p.stats.channels[ch as usize].max[0] {
                            Value::I64(v) => v,
                            v => panic!("{v:?}"),
                        })
                        .max()
                        .unwrap();
                    let sum: f64 = parts
                        .iter()
                        .map(|p| p.stats.channels[ch as usize].sum[0])
                        .sum();
                    assert_eq!(
                        lo,
                        *values.iter().min().unwrap(),
                        "{cells:?} {level} {node}"
                    );
                    assert_eq!(hi, *values.iter().max().unwrap());
                    assert_eq!(sum, values.iter().sum::<i64>() as f64);
                }
            }
        }
    }
}

/// The time step after the last chunk of each tile in the pieces `done` and the first `kept` bytes of `torn`.
fn next_steps(done: &[Vec<u8>], torn: &[u8], id: TrackId) -> Vec<u64> {
    let tiles = tiles();
    let mut next = vec![0u64; tiles.len()];
    for bytes in done.iter().map(|p| &p[..]).chain([torn]) {
        let mut rd = FileBlocks::with_recovery(bytes, Recovery::Prefix);
        while let Some(Ok(b)) = rd.next_block() {
            if b.kind != BlockKind::Series {
                continue;
            }
            let Some(c) = b.chunk() else { continue };
            let h = c.head();
            if h.track_id != id {
                continue;
            }
            let k = tiles.iter().position(|t| *t == h.tile).unwrap();
            next[k] = next[k].max(h.first_item.get() + u64::from(h.count));
        }
    }
    next
}

/// Crash the run at every block boundary of both pieces and at bytes between; a mux resumes the chain, each tile's
/// handle goes on after the tile's last chunk kept, and the chain holds the bytes of the run without the crash:
/// the tile trees of the torn piece were rebuilt and their lost blocks written again.
#[test]
fn a_mux_grid_crashed_anywhere_resumes_to_the_bytes_of_the_whole_run() {
    let (want, id) = whole_run();
    // tile trees write blocks before the piece's last chunk
    let r = TapeReader::open(&want[0][..]).unwrap();
    let entries = r.entries(0).unwrap();
    let last_chunk = entries
        .iter()
        .filter(|e| e.kind == BlockKind::Series)
        .map(|e| e.offset)
        .max()
        .unwrap();
    let mid: Vec<_> = entries
        .iter()
        .filter(|e| e.kind == BlockKind::Summary && e.offset < last_chunk)
        .collect();
    assert!(
        mid.iter().filter(|e| e.tile.is_some()).count() >= 4,
        "every tile's tree mid-piece"
    );
    let mut crashes = 0;
    for (p, piece) in want.iter().enumerate() {
        let (done, _) = want.split_at(p);
        for at in cut_points(piece, 30) {
            if p + 1 == want.len() && at == piece.len() {
                continue;
            }
            let mem = plant(done, &piece[..at]);
            let (mux, resumed) =
                TapeMux::resume(mem.clone(), OWNER, 0, options(CUT), MuxOptions::default())
                    .unwrap();
            let (track, _) = define(&mux);
            let kept = usize::try_from(resumed.kept_bytes).unwrap();
            let from = next_steps(done, &piece[..kept.min(at)], id);
            write(mux, track, &from);
            let got = pieces(&mem);
            assert!(
                got == want,
                "a crash in piece {p} at {at} of {}, kept {kept}, from {from:?}: lengths {:?}, want {:?}",
                piece.len(),
                got.iter().map(Vec::len).collect::<Vec<_>>(),
                want.iter().map(Vec::len).collect::<Vec<_>>(),
            );
            crashes += 1;
        }
    }
    assert!(crashes > 50, "{crashes} crashes");
}
