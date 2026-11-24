//! Grid handles (FMT-15): a time step of a grid, or of a box of its tiles, split into the open chunk of each tile.
//! Several threads write boxes of one grid and every cell reads back through a box query; a tile no box holds has
//! no chunk; a series written through a grid handle has the bytes of chunks written directly; a time step of a tile
//! that fits no chunk, a box off the tiles' edges and a partial time step are refused.

use std::collections::BTreeMap;
use std::ops::Range;
use std::thread;

use exact_units::Bytes;
use shrink_wrap::prelude::*;
use ww_tape::chunk::ChunkHead;
use ww_tape::flag_runs::FlagRunsBuilder;
use ww_tape::samples::{Samples, encode, max_encoded_len};
use ww_tape::{
    AxisCoords, BlockKind, ClockNum, Codec, ColumnKind, ColumnSpec, Definitions, Element,
    ElementDesc, EndMode, FixedAxis, GridDef, ItemNum, Items, MAX_BLOCK_LEN, Magnitude, MemPieces,
    PieceChain, Ratio64, SampleShape, SeriesGrid, TapeReader, TileCoords, TrackKind, TrackNum,
    TrackRepeat, TrackSpec, track_key,
};
use ww_tape_mux::{CallError, GridOptions, ItemError, MuxError, MuxOptions, TapeMux};

use crate::mux_handles::{clock, options};
use crate::mux_multi::pieces;

const OWNER: ww_tape::OwnerId = ww_tape::OwnerId([0x6e; 32]);
/// Channels of a cell.
const CHANNELS: u32 = 2;
/// Time steps each box is written for.
const STEPS: u64 = 23;
/// First tick of the grid; a time step every [PERIOD] ticks.
const START: i64 = 5_000;
const PERIOD: i64 = 1_000;

fn period() -> Ratio64 {
    Ratio64::int(PERIOD as u64).unwrap()
}

/// Fixed axes of `(extent, tile extent)`, leaked for a definition of any lifetime.
fn axes(extents: &[(u32, u32)]) -> &'static [FixedAxis<'static>] {
    let axes: Vec<FixedAxis<'static>> = extents
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

/// A grid of i32 on [CHANNELS] channels over `axes`, each tile summarised.
fn grid_track(clock: ClockNum, axes: &'static [FixedAxis<'static>]) -> TrackSpec<'static> {
    let mut def = GridDef::of::<i32>(CHANNELS, period());
    def.axes = RefVec::Slice { slice: axes };
    TrackSpec {
        key: track_key(0x6e, &[ww_tape::Segment::Name("field")]),
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

/// The value of channel `ch` of cell `cell` at time step `t`.
fn value(t: u64, cell: &[u32], ch: u32) -> i32 {
    let mut v = (t as i32).wrapping_mul(10_000_000).wrapping_add(ch as i32);
    for (a, c) in cell.iter().enumerate() {
        v = v.wrapping_add((*c as i32 + 1) * 10_i32.pow(2 * a as u32 + 1));
    }
    v
}

/// Time step `t` of the box `cells`: its cells row-major, the last axis fastest, each cell's channels interleaved.
fn step(t: u64, cells: &[Range<u32>]) -> Vec<i32> {
    let mut at: Vec<u32> = cells.iter().map(|r| r.start).collect();
    let n: usize = cells.iter().map(|r| r.len()).product();
    let mut out = Vec::with_capacity(n * CHANNELS as usize);
    for _ in 0..n {
        for ch in 0..CHANNELS {
            out.push(value(t, &at, ch));
        }
        for (a, r) in at.iter_mut().zip(cells).rev() {
            *a += 1;
            if *a < r.end {
                break;
            }
            *a = r.start;
        }
    }
    out
}

/// A cell's channels by time step and cell.
type Cells = BTreeMap<(u64, Vec<u32>), Vec<i32>>;

/// Every value of the grid track `id` in the pieces `pieces` read back through a box query of the whole grid and
/// time, by time step and cell: a cell's channels. Each cell is read once.
fn read_cells(
    pieces: &[Vec<u8>],
    spec: &TrackSpec<'static>,
    track: TrackNum,
) -> (Cells, Vec<(TileCoords, u32)>) {
    let def = spec.clone().numbered(track);
    let TrackKind::Series(grid) = &def.kind else {
        unreachable!("a grid track")
    };
    let layout = grid.tile_layout().unwrap();
    let whole: Vec<Range<u32>> = layout.extents().iter().map(|(e, _)| 0..*e).collect();
    let tiles = layout.tile_box(&whole).unwrap();
    let shape = SampleShape::new(ElementDesc::of(Element::I32), CHANNELS);
    let mut cells = BTreeMap::new();
    let mut chunks = Vec::new();
    let mut buf = Vec::new();
    for bytes in pieces {
        let r = TapeReader::open(&bytes[..]).unwrap();
        for g in r
            .grid_tiles(0, def.id(), i64::MIN..i64::MAX, &tiles)
            .unwrap()
        {
            let b = r.read_block(0, &g.entry, &mut buf).unwrap();
            let c = b.chunk().unwrap();
            let head = c.head();
            assert_eq!(head.tile, g.tile, "the head names the tile the index found");
            chunks.push((g.tile, head.count));
            let ranges: Vec<Range<u32>> = g
                .tile
                .as_slice()
                .iter()
                .enumerate()
                .map(|(a, t)| layout.cell_range(a, *t).unwrap())
                .collect();
            let per_step = layout.tile_cells(&g.tile).unwrap() as usize;
            let n = per_step * head.count as usize;
            let col = c.find(ColumnKind::Samples, 0).unwrap();
            let s = Samples::of(&col, shape, n).unwrap();
            let mut out = vec![0i32; n * CHANNELS as usize];
            s.decode(0, &mut out).unwrap();
            let mut values = out.chunks(CHANNELS as usize);
            for k in 0..u64::from(head.count) {
                let t = head.first_item.get() + k;
                let tick = START + t as i64 * PERIOD;
                assert!(head.first_tick <= tick && tick <= head.last_tick);
                let mut at: Vec<u32> = ranges.iter().map(|r| r.start).collect();
                for _ in 0..per_step {
                    let v = values.next().unwrap().to_vec();
                    assert!(
                        cells.insert((t, at.clone()), v).is_none(),
                        "{t} {at:?} once"
                    );
                    for (a, r) in at.iter_mut().zip(&ranges).rev() {
                        *a += 1;
                        if *a < r.end {
                            break;
                        }
                        *a = r.start;
                    }
                }
            }
        }
    }
    (cells, chunks)
}

/// Grid options of chunks of `chunk_steps` time steps, no latency.
fn grid_options(chunk_steps: u32) -> GridOptions {
    GridOptions {
        chunk_steps,
        latency: None,
        first_tick: START,
        ..GridOptions::default()
    }
}

/// A grid of two fixed axes, its tiles short at both far edges, written by three threads each holding a box of
/// tiles, in batches of uneven size: every cell of every box reads back cell by cell through a box query, at its
/// time step; the tiles no box holds have no chunk; the boxes' chunks are completed every 5 steps.
#[test]
fn a_grid_written_from_several_threads_reads_back_cell_by_cell() {
    // 3 x 3 tiles: rows of 2, 2, 1 cells, columns of 3, 3, 1
    let axes = axes(&[(5, 2), (7, 3)]);
    let boxes: [[Range<u32>; 2]; 3] = [[0..2, 0..7], [2..4, 0..3], [4..5, 3..7]];
    let mem = MemPieces::new();
    let mux = TapeMux::create(
        mem.clone(),
        OWNER,
        0,
        &[],
        options(20),
        MuxOptions::default(),
    )
    .unwrap();
    let c = mux.define_clock(clock()).unwrap();
    let spec = grid_track(c, axes);
    let track = mux.define_track(spec.clone()).unwrap();
    let handles: Vec<_> = boxes
        .iter()
        .map(|b| mux.grid_box::<i32>(track, b, grid_options(5)).unwrap())
        .collect();
    assert_eq!(
        handles[0].tiles(),
        [[0, 0], [0, 1], [0, 2]].map(|c| TileCoords::new(&c).unwrap())
    );
    thread::scope(|s| {
        for (k, (mut h, b)) in handles.into_iter().zip(&boxes).enumerate() {
            s.spawn(move || {
                let mut t = 0;
                let mut batch = 1 + k as u64;
                while t < STEPS {
                    let n = batch.min(STEPS - t);
                    let steps: Vec<i32> = (t..t + n).flat_map(|t| step(t, b)).collect();
                    assert_eq!(steps.len(), n as usize * h.step_len());
                    h.push(&steps).unwrap();
                    t += n;
                    batch = batch % 4 + 1;
                }
                h.finish().unwrap();
            });
        }
    });
    mux.close().unwrap();

    let (cells, chunks) = read_cells(&pieces(&mem), &spec, track);
    let mut want = BTreeMap::new();
    for b in &boxes {
        for x in b[0].clone() {
            for y in b[1].clone() {
                for t in 0..STEPS {
                    let v = (0..CHANNELS).map(|ch| value(t, &[x, y], ch)).collect();
                    want.insert((t, vec![x, y]), v);
                }
            }
        }
    }
    assert_eq!(cells.len(), want.len(), "every written cell once");
    assert!(cells == want, "each cell's values at each time step");
    let unwritten = [[1, 1], [1, 2], [2, 0]].map(|c| TileCoords::new(&c).unwrap());
    assert!(
        chunks.iter().all(|(t, _)| !unwritten.contains(t)),
        "a tile no box holds is never written"
    );
    // 23 steps in chunks of 5: 4 full, the last of 3
    let mut counts: Vec<u32> = chunks
        .iter()
        .filter(|(t, _)| *t == TileCoords::new(&[0, 0]).unwrap())
        .map(|(_, n)| *n)
        .collect();
    counts.sort_unstable();
    assert_eq!(counts, [3, 5, 5, 5, 5]);
    assert_eq!(
        chunks.len(),
        6 * 5,
        "6 tiles in chunks of 5, 5, 5, 5, 3 steps"
    );
}

/// A grid of three fixed axes written whole by one handle, its time steps split across the tiles: the same cells
/// read back as when each box of one tile is written by a handle of its own.
#[test]
fn a_whole_grid_reads_back_as_its_boxes_written_one_by_one() {
    let axes = axes(&[(3, 2), (4, 4), (5, 2)]);
    let run = |boxed: bool| {
        let mem = MemPieces::new();
        let mux = TapeMux::create(
            mem.clone(),
            OWNER,
            0,
            &[],
            options(1000),
            MuxOptions::default(),
        )
        .unwrap();
        let c = mux.define_clock(clock()).unwrap();
        let spec = grid_track(c, axes);
        let track = mux.define_track(spec.clone()).unwrap();
        let whole = [0..3, 0..4, 0..5];
        if boxed {
            for x in [0..2, 2..3] {
                for z in [0..2, 2..4, 4..5] {
                    let b = [x.clone(), 0..4, z];
                    let mut h = mux.grid_box::<i32>(track, &b, grid_options(4)).unwrap();
                    assert_eq!(h.tiles().len(), 1);
                    for t in 0..STEPS {
                        h.push(&step(t, &b)).unwrap();
                    }
                    h.finish().unwrap();
                }
            }
        } else {
            let mut h = mux.grid::<i32>(track, grid_options(4)).unwrap();
            assert_eq!(h.tiles().len(), 6);
            let steps: Vec<i32> = (0..STEPS).flat_map(|t| step(t, &whole)).collect();
            h.push(&steps).unwrap();
            h.finish().unwrap();
        }
        mux.close().unwrap();
        read_cells(&pieces(&mem), &spec, track).0
    };
    let whole = run(false);
    assert_eq!(whole.len(), 3 * 4 * 5 * STEPS as usize);
    assert!(whole == run(true));
}

/// The bytes a series of [grid_track] with no fixed axes writes through a chain directly, in chunks of `chunk`
/// samples, its samples flagged by `flags` per batch.
fn direct_series(batches: &[(u64, u32)], chunk: u64) -> Vec<Vec<u8>> {
    let mem = MemPieces::new();
    let mut defs = Definitions::new();
    let c = defs.define_clock(clock()).unwrap();
    let spec = grid_track(c, &[]);
    let track = defs.define_track(spec.clone()).unwrap();
    let id = spec.numbered(track).id();
    let shape = SampleShape::new(ElementDesc::of(Element::I32), CHANNELS);
    let grid = SeriesGrid::start(0, START, Magnitude::ONE);
    let mut chain = PieceChain::create_on(mem.clone(), OWNER, 0, &[], options(4)).unwrap();
    let flag_of: Vec<u32> = batches
        .iter()
        .flat_map(|&(n, f)| std::iter::repeat_n(f, n as usize))
        .collect();
    let total = flag_of.len() as u64;
    let mut first = 0;
    while first < total {
        let n = chunk.min(total - first);
        let samples: Vec<i32> = (first..first + n).flat_map(|t| step(t, &[])).collect();
        let mut bytes = vec![0; max_encoded_len(Codec::RAW, shape, n as usize).unwrap()];
        let len = encode(Codec::RAW, shape, &samples, &mut bytes).unwrap();
        bytes.truncate(len);
        let mut runs = FlagRunsBuilder::new();
        for f in &flag_of[first as usize..(first + n) as usize] {
            runs.push(1, *f).unwrap();
        }
        let (first_tick, last_tick) = grid
            .chunk_ticks(period(), ItemNum::new(first), n as u32)
            .unwrap();
        let head = ChunkHead {
            kind: BlockKind::Series,
            track_id: id,
            track_number: track,
            first_item: ItemNum::new(first),
            count: n as u32,
            first_tick,
            last_tick,
            latest_end: last_tick,
            time_mode: 0,
            end_mode: EndMode::Absent,
            grid: Some(grid),
            tile: TileCoords::NONE,
            level: 0,
        };
        let mut columns = vec![(
            ColumnSpec {
                kind: ColumnKind::Samples,
                codec: Codec::RAW,
                param: 0,
                len: bytes.len(),
            },
            bytes,
        )];
        if !runs.is_empty() {
            let mut b = Vec::new();
            runs.write(&mut b);
            columns.push((
                ColumnSpec {
                    kind: ColumnKind::Flags,
                    codec: Codec::FLAG_RUNS,
                    param: 0,
                    len: b.len(),
                },
                b,
            ));
        }
        let columns: Vec<(ColumnSpec, &[u8])> =
            columns.iter().map(|(s, b)| (*s, b.as_slice())).collect();
        chain.write_chunk(&defs, &head, &columns).unwrap();
        first += n;
    }
    chain.close().unwrap();
    pieces(&mem)
}

/// A series, the grid of no fixed axes, written through a grid handle in uneven flagged batches has the bytes of
/// the same chunks written through a chain directly: its chunk heads carry no tile, as before grids.
#[test]
fn a_series_through_a_grid_handle_writes_the_bytes_of_chunks_written_directly() {
    let batches: Vec<(u64, u32)> = (0..40u64)
        .map(|k| (1 + k * 37 % 211, (k % 3 == 0) as u32))
        .collect();
    let mem = MemPieces::new();
    let mux = TapeMux::create(
        mem.clone(),
        OWNER,
        0,
        &[],
        options(4),
        MuxOptions::default(),
    )
    .unwrap();
    let c = mux.define_clock(clock()).unwrap();
    let track = mux.define_track(grid_track(c, &[])).unwrap();
    let mut h = mux.grid::<i32>(track, grid_options(500)).unwrap();
    assert_eq!(h.tiles(), [TileCoords::NONE]);
    assert_eq!(h.step_len(), CHANNELS as usize);
    let mut t = 0;
    for &(n, flags) in &batches {
        let steps: Vec<i32> = (t..t + n).flat_map(|t| step(t, &[])).collect();
        h.push_flagged(&steps, flags).unwrap();
        t += n;
    }
    h.finish().unwrap();
    mux.close().unwrap();
    let got = pieces(&mem);
    assert!(got.len() > 1, "the chain is cut");
    assert!(got == direct_series(&batches, 500), "the same pieces");
}

/// The error a grid handle of `extents` with `options` is refused with.
fn refusal(
    extents: &[(u32, u32)],
    cells: Option<&[Range<u32>]>,
    options: GridOptions,
) -> CallError {
    let mux = TapeMux::create(
        MemPieces::new(),
        OWNER,
        0,
        &[],
        crate::mux_handles::options(4),
        MuxOptions::default(),
    )
    .unwrap();
    let c = mux.define_clock(clock()).unwrap();
    let track = mux.define_track(grid_track(c, axes(extents))).unwrap();
    let r = match cells {
        Some(b) => mux.grid_box::<i32>(track, b, options),
        None => mux.grid::<i32>(track, options),
    };
    match r {
        Err(MuxError::Call(e)) => e,
        Err(e) => panic!("another error: {e}"),
        Ok(_) => panic!("a handle"),
    }
}

/// A time step of a tile more than the chunk bytes or a block hold is refused with its bytes, not split.
#[test]
fn a_tile_step_over_a_chunk_or_a_block_is_refused() {
    // a tile of 16 x 16 cells of 2 channels of i32: 2048 bytes a step
    let small = GridOptions {
        chunk_bytes: Bytes::new(2047),
        ..grid_options(10)
    };
    assert_eq!(
        refusal(&[(32, 16), (16, 16)], None, small),
        CallError::StepTooLarge {
            bytes: Bytes::new(2048),
            max: Bytes::new(2047)
        }
    );
    // the edge tile is short, the largest still counts: a box of the edge tile alone fits
    let edge = GridOptions {
        chunk_bytes: Bytes::new(1024),
        ..grid_options(10)
    };
    let mux_ok = TapeMux::create(
        MemPieces::new(),
        OWNER,
        0,
        &[],
        options(4),
        MuxOptions::default(),
    )
    .unwrap();
    let c = mux_ok.define_clock(clock()).unwrap();
    let track = mux_ok
        .define_track(grid_track(c, axes(&[(24, 16), (16, 16)])))
        .unwrap();
    assert!(
        mux_ok
            .grid_box::<i32>(track, &[16..24, 0..16], edge)
            .is_ok()
    );
    assert!(matches!(
        mux_ok.grid::<i32>(track, edge),
        Err(MuxError::Call(CallError::StepTooLarge { bytes, .. })) if bytes == Bytes::new(2048)
    ));
    // 2^24 cells of 8 bytes: 128 MiB a step, more than a block
    let big = GridOptions {
        chunk_bytes: Bytes::new(u32::MAX),
        ..grid_options(10)
    };
    assert_eq!(
        refusal(&[(1 << 24, 1 << 24)], None, big),
        CallError::StepTooLarge {
            bytes: Bytes::new(1 << 27),
            max: Bytes::new(MAX_BLOCK_LEN as u64)
        }
    );
}

/// A box that is not one non-empty range per fixed axis within its extent, starting and ending at a tile's edge, is
/// refused; one ending at the axis' extent is taken.
#[test]
fn a_box_off_the_tiles_is_refused() {
    let extents = [(5, 2), (7, 3)];
    let boxes: [Vec<Range<u32>>; 7] = [
        std::iter::once(0..2).collect(),
        vec![0..2, 0..3, 0..1],
        vec![1..2, 0..3],
        vec![0..3, 0..3],
        vec![0..2, 0..8],
        vec![2..2, 0..3],
        vec![0..2, 0..5],
    ];
    for b in boxes {
        assert_eq!(
            refusal(&extents, Some(&b), grid_options(4)),
            CallError::TileBox,
            "{b:?}"
        );
    }
}

/// Values that are not whole time steps of the box are refused before anything is added.
#[test]
fn a_partial_time_step_is_refused() {
    let mux = TapeMux::create(
        MemPieces::new(),
        OWNER,
        0,
        &[],
        options(4),
        MuxOptions::default(),
    )
    .unwrap();
    let c = mux.define_clock(clock()).unwrap();
    let track = mux
        .define_track(grid_track(c, axes(&[(5, 2), (7, 3)])))
        .unwrap();
    let mut h = mux
        .grid_box::<i32>(track, &[0..4, 3..7], grid_options(4))
        .unwrap();
    assert_eq!(h.step_len(), 4 * 4 * CHANNELS as usize);
    let r = h.push(&[0; 31]);
    assert!(matches!(
        r,
        Err(MuxError::Call(CallError::Item {
            why: ItemError::PartialStep { scalars: 32 },
            ..
        }))
    ));
    assert_eq!(h.next_item(), ItemNum::new(0));
}
