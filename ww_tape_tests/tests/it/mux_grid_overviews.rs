//! A grid's space overviews from the mux (FMT-15): each tile chunk's overview chunks hold, level by level, the min,
//! max and mean of the cells a naive downsample gathers, edge tiles and odd extents included; a box of a level finds
//! them; a mux crashed at any block boundary writes the overviews the crash lost and the bytes of the run without it.

use std::ops::Range;

use shrink_wrap::prelude::*;
use ww_tape::{
    AxisCoords, BlockKind, ClockNum, Element, ElementDesc, FileBlocks, FixedAxis, GridDef, ItemNum,
    Items, Magnitude, MemPieces, Overview, OwnerId, Ratio64, Recovery, SampleShape, TapeReader,
    TileCoords, TileLayout, TrackId, TrackKind, TrackNum, TrackRepeat, TrackSpec, track_key,
};
use ww_tape_mux::{GridHandle, GridOptions, MuxOptions, TapeMux};
use ww_tape_summary::overview::div_even;

use crate::mux_handles::{clock, options};
use crate::mux_multi::{pieces, plant};
use crate::resume::cut_points;

const OWNER: OwnerId = OwnerId([0x48; 32]);
const CHANNELS: u32 = 2;
const PERIOD: i64 = 1_000;
/// Fixed axes of `(extent, tile extent, downsample)`: x of 3 tiles, its last 2 cells; a band of 3 kept whole; y of 2
/// tiles, its last 3 cells, an odd edge. Tiles of 4 halve twice: 2 levels.
const AXES: [(u32, u32, bool); 3] = [(10, 4, true), (3, 3, false), (7, 4, true)];
const LEVELS: u8 = 2;
/// Time steps of a chunk.
const CHUNK_STEPS: u32 = 16;
/// Batches of [BATCH] time steps each handle is pushed.
const BATCHES: u64 = 3;
const BATCH: u64 = 24;
/// Chunks of a piece before the chain cuts it.
const CUT: u64 = 40;

fn period() -> Ratio64 {
    Ratio64::int(PERIOD as u64).unwrap()
}

fn grid_axes() -> &'static [FixedAxis<'static>] {
    let axes: Vec<FixedAxis<'static>> = AXES
        .iter()
        .zip(["x", "band", "y"])
        .map(|(&(extent, tile, downsample), name)| FixedAxis {
            name,
            extent: UNib32(extent),
            tile_extent: UNib32(tile),
            unit: None,
            scale: Magnitude::ONE,
            coords: AxisCoords::Regular { origin: 0, step: 1 },
            downsample,
        })
        .collect();
    Box::leak(axes.into_boxed_slice())
}

/// A grid of i32 on [CHANNELS] channels, with every statistic summarised over nodes of 16 time steps when `summaries`.
fn grid_track(clock: ClockNum, summaries: bool) -> TrackSpec<'static> {
    let mut def = GridDef::of::<i32>(CHANNELS, period());
    def.axes = RefVec::Slice { slice: grid_axes() };
    def.summaries = summaries.then(|| ww_tape_summary::SeriesSummarySpec {
        base_shift: 4,
        stats: ww_tape_summary::SeriesStats::all(ElementDesc::of(Element::I32)),
        clip: None,
    });
    TrackSpec {
        key: track_key(0x48, &[ww_tape::Segment::Name("thermal")]),
        clock,
        dimensions: RefVec::Slice { slice: &[] },
        items: Items::numbers::<i32>(None),
        label: "thermal",
        repeat: TrackRepeat::Stream,
        kind: TrackKind::Series(def),
        relations: RefVec::Slice { slice: &[] },
        owner: [0; 32],
    }
}

fn layout() -> TileLayout {
    let TrackKind::Series(def) = grid_track(ClockNum::new(1), false).kind else {
        unreachable!("a grid track")
    };
    def.tile_layout().unwrap()
}

fn shape() -> SampleShape {
    SampleShape::new(ElementDesc::of(Element::I32), CHANNELS)
}

/// The tiles of the grid, row-major.
fn tiles() -> Vec<TileCoords> {
    let l = layout();
    let t = l.tiles();
    let mut out = vec![];
    for a in 0..t[0] {
        for b in 0..t[1] {
            for c in 0..t[2] {
                out.push(TileCoords::new(&[a, b, c]).unwrap());
            }
        }
    }
    out
}

/// The cells of tile `t` in layout `l`, one range per axis.
fn cells_in(l: &TileLayout, t: &TileCoords) -> Vec<Range<u32>> {
    (0..3)
        .map(|a| l.cell_range(a, t.as_slice()[a]).unwrap())
        .collect()
}

/// Channel `ch` of cell `c` at time step `t`: values whose means fall on ties now and then.
fn value(t: u64, c: &[u32], ch: u32) -> i32 {
    let h = t * 7919 + u64::from(c[0]) * 104_729 + u64::from(c[1]) * 613 + u64::from(c[2]) * 1_009;
    ((h + u64::from(ch) * 31) % 4_001) as i32 - 2_000
}

/// Time steps `steps` of the box `cells`: row-major, each cell's channels interleaved.
fn box_steps(steps: Range<u64>, cells: &[Range<u32>]) -> Vec<i32> {
    let mut out = vec![];
    for t in steps {
        for x in cells[0].clone() {
            for b in cells[1].clone() {
                for y in cells[2].clone() {
                    for ch in 0..CHANNELS {
                        out.push(value(t, &[x, b, y], ch));
                    }
                }
            }
        }
    }
    out
}

/// The source cells of cell `c` of level `level` along axis `a`.
fn sources(level: u8, a: usize, c: u32) -> Range<u32> {
    let (extent, _, down) = AXES[a];
    if !down {
        return c..c + 1;
    }
    (c << level)..((c + 1) << level).min(extent)
}

/// The naive min, max and mean, per channel, of the cells as written that cell `c` of `level` covers at step `t`.
fn naive(t: u64, level: u8, c: &[u32], ch: u32) -> (i32, i32, i32) {
    let mut vals = vec![];
    for x in sources(level, 0, c[0]) {
        for b in sources(level, 1, c[1]) {
            for y in sources(level, 2, c[2]) {
                vals.push(value(t, &[x, b, y], ch));
            }
        }
    }
    let sum: i128 = vals.iter().map(|v| i128::from(*v)).sum();
    (
        *vals.iter().min().unwrap(),
        *vals.iter().max().unwrap(),
        div_even(sum, vals.len() as u32) as i32,
    )
}

type TileHandle = (GridHandle<MemPieces, i32>, Vec<Range<u32>>, u64);

/// Write the grid on `mux`, a handle per tile from its time step in `from`, or one handle of the whole grid when
/// `from` is empty; then close.
fn write(mux: TapeMux<MemPieces>, track: TrackNum, from: &[u64]) {
    let opts = |first: u64| GridOptions {
        chunk_steps: CHUNK_STEPS,
        latency: None,
        first_item: ItemNum::new(first),
        first_tick: first as i64 * PERIOD,
        ..GridOptions::default()
    };
    let mut handles: Vec<TileHandle> = if from.is_empty() {
        let whole: Vec<Range<u32>> = AXES.iter().map(|(e, ..)| 0..*e).collect();
        vec![(mux.grid::<i32>(track, opts(0)).unwrap(), whole, 0)]
    } else {
        tiles()
            .iter()
            .zip(from)
            .map(|(t, &first)| {
                let cells = cells_in(&layout(), t);
                let mut h = mux.grid_box::<i32>(track, &cells, opts(first)).unwrap();
                h.regrid(ww_tape::SeriesGrid::start(0, 0, Magnitude::ONE))
                    .unwrap();
                (h, cells, first)
            })
            .collect()
    };
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

fn define(mux: &TapeMux<MemPieces>, summaries: bool) -> (TrackNum, TrackId) {
    let c = mux.define_clock(clock()).unwrap();
    let spec = grid_track(c, summaries);
    let id = spec.key.id();
    (mux.define_track(spec).unwrap(), id)
}

/// The pieces of a run without a crash: a handle per tile, or one of the whole grid; the time pyramid with
/// `summaries`.
fn whole_run(per_tile: bool, summaries: bool) -> (Vec<Vec<u8>>, TrackId) {
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
    let (track, id) = define(&mux, summaries);
    let from = if per_tile {
        vec![0; tiles().len()]
    } else {
        vec![]
    };
    write(mux, track, &from);
    (pieces(&mem), id)
}

/// Every tile chunk has one overview chunk per level of the same time steps, found by a box of that level; each
/// cell of each holds the naive min, max and mean of its source cells, at edge tiles and the odd edge too.
#[test]
fn a_mux_grid_writes_overviews_of_a_naive_downsample() {
    let (pieces, id) = whole_run(false, false);
    let l = layout();
    assert_eq!(l.overview_levels(), LEVELS);
    let steps = BATCHES * BATCH;
    let tile_chunks = tiles().len() as u64 * steps.div_ceil(u64::from(CHUNK_STEPS));
    let mut found = [0u64; LEVELS as usize + 1];
    for p in &pieces {
        let r = TapeReader::open(&p[..]).unwrap();
        for level in 0..=LEVELS {
            let at = l.overview(level).unwrap();
            let whole: Vec<Range<u32>> = at.extents().iter().map(|(e, _)| 0..*e).collect();
            let b = at.tile_box(&whole).unwrap();
            let got = r.grid_tiles(0, id, i64::MIN..i64::MAX, &b).unwrap();
            found[level as usize] += got.len() as u64;
            for t in got.iter().filter(|_| level > 0) {
                assert_eq!(t.entry.level, level);
                let mut buf = vec![];
                let block = r.read_block(0, &t.entry, &mut buf).unwrap();
                let chunk = block.chunk().unwrap();
                let head = chunk.head();
                assert_eq!((head.level, head.tile), (level, t.tile));
                let o = Overview::of(&chunk, &l, shape()).unwrap();
                let mut cols = [vec![], vec![], vec![]];
                for (col, s) in cols.iter_mut().zip([o.min(), o.max(), o.mean()]) {
                    *col = vec![0i32; o.cells() * CHANNELS as usize];
                    s.decode(0, col).unwrap();
                }
                let cells = cells_in(&at, &t.tile);
                let mut i = 0;
                for step in head.first_item.get()..head.first_item.get() + u64::from(head.count) {
                    for x in cells[0].clone() {
                        for b in cells[1].clone() {
                            for y in cells[2].clone() {
                                for ch in 0..CHANNELS {
                                    let (lo, hi, mean) = naive(step, level, &[x, b, y], ch);
                                    let got = (cols[0][i], cols[1][i], cols[2][i]);
                                    assert_eq!(
                                        got,
                                        (lo, hi, mean),
                                        "level {level} step {step} cell {x} {b} {y} ch {ch}"
                                    );
                                    i += 1;
                                }
                            }
                        }
                    }
                }
                assert_eq!(i, cols[0].len());
            }
        }
    }
    assert_eq!(found, [tile_chunks; LEVELS as usize + 1]);
}

/// A view picks its level by the cells a pixel spans; the box of that level finds the level's chunks only.
#[test]
fn a_view_picks_its_level_and_finds_only_it() {
    let l = layout();
    assert_eq!(l.level_for(&[1.0, 1.0, 1.0]), 0);
    assert_eq!(l.level_for(&[2.5, 1.0, 3.0]), 1);
    assert_eq!(l.level_for(&[16.0, 1.0, 16.0]), LEVELS);
    // a box made of the layout as written finds no overview, and one of a level none of the chunks as written
    let (pieces, id) = whole_run(false, false);
    let r = TapeReader::open(&pieces[0][..]).unwrap();
    let at = l.overview(1).unwrap();
    let b = at.tile_box(&[0..1, 0..1, 0..1]).unwrap();
    for t in r.grid_tiles(0, id, i64::MIN..i64::MAX, &b).unwrap() {
        assert_eq!(t.entry.level, 1);
        assert_eq!(t.tile.as_slice(), [0, 0, 0]);
    }
    let b = l.tile_box(&[0..1, 0..1, 0..1]).unwrap();
    for t in r.grid_tiles(0, id, i64::MIN..i64::MAX, &b).unwrap() {
        assert_eq!(t.entry.level, 0);
    }
}

/// The time step after the last chunk as written of each tile in the pieces `done` and the bytes `torn`.
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
            if h.track_id != id || h.level != 0 {
                continue;
            }
            let k = tiles.iter().position(|t| *t == h.tile).unwrap();
            next[k] = next[k].max(h.first_item.get() + u64::from(h.count));
        }
    }
    next
}

/// Crash the run at every block boundary of each piece and at bytes between; a mux resumes the chain, writes the
/// overview chunks the crash lost after the tile chunks the piece kept, each tile's handle goes on after its last
/// chunk kept, and the chain holds the bytes of the run without the crash.
#[test]
fn a_mux_grid_crashed_anywhere_resumes_its_overviews_to_the_bytes_of_the_whole_run() {
    crash_anywhere(false);
}

/// [a_mux_grid_crashed_anywhere_resumes_its_overviews_to_the_bytes_of_the_whole_run] with each tile's time pyramid:
/// the lost summary blocks and overview chunks come back in the order the run made them.
#[test]
fn a_mux_grid_with_summaries_crashed_anywhere_resumes_to_the_bytes_of_the_whole_run() {
    crash_anywhere(true);
}

fn crash_anywhere(summaries: bool) {
    let (want, id) = whole_run(true, summaries);
    assert!(want.len() >= 2, "{} pieces", want.len());
    let mut crashes = 0;
    let mut torn_overviews = 0;
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
            let (track, _) = define(&mux, summaries);
            let kept = usize::try_from(resumed.kept_bytes).unwrap();
            let kept_bytes = &piece[..kept.min(at)];
            // the piece ends between a tile chunk and its last overview
            let mut rd = FileBlocks::with_recovery(kept_bytes, Recovery::Prefix);
            let mut last = None;
            while let Some(Ok(b)) = rd.next_block() {
                if let Some(c) = b.chunk() {
                    last = Some(c.head().level);
                }
            }
            if last.is_some_and(|l| l < LEVELS) {
                torn_overviews += 1;
            }
            let from = next_steps(done, kept_bytes, id);
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
    assert!(
        torn_overviews > 10,
        "{torn_overviews} crashes between a chunk and its overviews"
    );
}

/// A track's items hand out the samples of the chunks as written, one tile per time run as for any grid, and pass
/// over the overview chunks.
#[test]
fn a_follower_skips_overview_chunks() {
    let (pieces, id) = whole_run(false, false);
    let mut f = ww_tape_fs::follow::TrackItems::new(id);
    let mut defs = ww_tape::Definitions::new();
    let mut out = std::collections::VecDeque::new();
    let (mut written, mut overviews) = (0, 0);
    let mut runs = std::collections::BTreeSet::new();
    for p in &pieces {
        let mut rd = FileBlocks::with_recovery(&p[..], Recovery::Prefix);
        while let Some(Ok(b)) = rd.next_block() {
            defs.take_block(&b).unwrap();
            if let Some(c) = b.chunk().filter(|c| c.head().track_id == id) {
                if c.head().level == 0 {
                    written += 1;
                    runs.insert(c.head().first_item);
                } else {
                    overviews += 1;
                }
            }
            let buf = ww_tape::BlockBuf::new(b.bytes().to_vec()).unwrap();
            f.take(&buf, &defs, &mut out).unwrap();
        }
    }
    assert_eq!(overviews, written * u32::from(LEVELS));
    let samples = out
        .iter()
        .filter(|i| matches!(i, ww_tape_fs::follow::TrackItem::Samples(_)))
        .count();
    assert_eq!(samples, runs.len());
}
