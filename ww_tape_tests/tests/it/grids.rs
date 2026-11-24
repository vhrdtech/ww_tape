//! A grid track written and read back by its tiles (FMT-15): each chunk's index entry carries its tile number, the
//! writer refuses coordinates outside the grid, and a box query finds the tiles of a time range and a box of cells
//! from the index alone, also over a stretch a reader scanned.

use std::ops::Range;

use shrink_wrap::prelude::*;
use ww_tape::chunk::ChunkHead;
use ww_tape::index::{IndexEntry, IndexHead, IndexKind, IndexPolicy, index_len, write_index};
use ww_tape::{
    AxisCoords, Block, BlockId, BlockKind, ClockNum, Codec, ColumnKind, ColumnSpec, DescribeError,
    EndMode, FixedAxis, GridDef, ItemNum, Magnitude, MemSink, Ratio64, ReadError, Segment,
    SeriesError, SyncPolicy, TapeReader, TapeWriter, TileCoords, TileLayout, TrackDef, TrackId,
    TrackKind, TrackNum, TrackRepeat, WriteError, track_key,
};

use crate::definitions::{CLOCK, clock};

const TRACK: TrackNum = TrackNum::new(1);
/// Time steps per chunk.
const STEPS: u32 = 4;
/// Runs of [STEPS] time steps each tile is written in.
const RUNS: u64 = 3;
/// First tick of the grid.
const START: i64 = 1_000;

/// Fixed axes of (extent, tile extent): 3, 1 and 4 tiles, edge tiles short on the first and the last.
const EXTENTS: [(u32, u32); 3] = [(5, 2), (4, 4), (10, 3)];

fn period() -> Ratio64 {
    Ratio64::new(1_000, 1).unwrap()
}

fn axes(extents: &[(u32, u32)]) -> Vec<FixedAxis<'static>> {
    extents
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
        .collect()
}

fn grid_track<'a>(axes: &'a [FixedAxis<'a>]) -> TrackDef<'a> {
    TrackDef {
        number: TRACK,
        key: track_key(0x9d, &[Segment::Name("field")]),
        clock: ClockNum::new(CLOCK),
        dimensions: RefVec::Slice { slice: &[] },
        items: ww_tape::Items::numbers::<i32>(None),
        label: "field",
        repeat: TrackRepeat::Stream,
        kind: TrackKind::Series(GridDef {
            axes: RefVec::Slice { slice: axes },
            ..GridDef::of::<i32>(1, period())
        }),
        relations: RefVec::Slice { slice: &[] },
        owner: [0; 32],
    }
}

/// A grid of `extents` written tile by tile, run after run: its bytes, closed or as a crash leaves them, its track
/// id and layout.
struct Written {
    bytes: Vec<u8>,
    id: TrackId,
    layout: TileLayout,
}

/// Every tile of `layout`, the first axis outermost.
fn all_tiles(layout: &TileLayout) -> Vec<TileCoords> {
    let mut out = vec![vec![]];
    for &tiles in layout.tiles() {
        out = out
            .into_iter()
            .flat_map(|c: Vec<u32>| {
                (0..tiles).map(move |t| {
                    let mut c = c.clone();
                    c.push(t);
                    c
                })
            })
            .collect();
    }
    out.iter().map(|c| TileCoords::new(c).unwrap()).collect()
}

/// The head of the chunk of run `run` at tile `tile` of the track `id`.
fn head(id: TrackId, run: u64, tile: TileCoords) -> ChunkHead {
    let grid = GridDef::of::<i32>(1, period()).first_grid(START);
    let first = ItemNum::new(run * u64::from(STEPS));
    let (first_tick, last_tick) = grid.chunk_ticks(period(), first, STEPS).unwrap();
    ChunkHead {
        kind: BlockKind::Series,
        track_id: id,
        track_number: TRACK,
        first_item: first,
        count: STEPS,
        first_tick,
        last_tick,
        latest_end: last_tick,
        time_mode: 0,
        end_mode: EndMode::Absent,
        grid: Some(grid),
        tile,
        level: 0,
    }
}

/// Write the grid of `extents`, every tile of every run, with a checkpoint every `checkpoint` bytes; closed when
/// `close`, else the bytes a crash leaves.
fn write(extents: &[(u32, u32)], checkpoint: u64, close: bool) -> Written {
    write_tiles(&axes(extents), all_tiles, checkpoint, close)
}

/// Write the grid of `axes`, the tiles `tiles` picks of its layout in every run, with a checkpoint every
/// `checkpoint` bytes; closed when `close`, else the bytes a crash leaves.
fn write_tiles(
    axes: &[FixedAxis<'_>],
    tiles: fn(&TileLayout) -> Vec<TileCoords>,
    checkpoint: u64,
    close: bool,
) -> Written {
    let track = grid_track(axes);
    let layout = track.series().unwrap().tile_layout().unwrap();
    let id = track.id();
    let mut defs = ww_tape::Definitions::new();
    crate::definitions::hold(&mut defs, &[clock(CLOCK), ww_tape::Describe::Track(track)]).unwrap();
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    let tape = w.sink().tape();
    w.set_session(crate::id(0).session).unwrap();
    w.set_index_policy(IndexPolicy {
        bytes: exact_units::Bytes::new(checkpoint),
        interval: None,
        ..IndexPolicy::default()
    })
    .unwrap();
    let samples = [0u8; 4 * STEPS as usize];
    let spec = ColumnSpec {
        kind: ColumnKind::Samples,
        codec: Codec::RAW,
        param: 0,
        len: samples.len(),
    };
    for run in 0..RUNS {
        for tile in tiles(&layout) {
            w.write_chunk(&defs, &head(id, run, tile), &[(spec, &samples[..])])
                .unwrap();
        }
    }
    if close {
        w.close().unwrap();
    } else {
        drop(w);
    }
    Written {
        bytes: tape.to_vec().unwrap(),
        id,
        layout,
    }
}

/// Every entry of a grid chunk in the final index carries the tile number its head's coordinates pack to; the
/// piece verifies, its index the same set as its walk.
#[test]
fn each_grid_entry_carries_its_tile_number() {
    let g = write(&EXTENTS, 1 << 20, true);
    assert_eq!(g.layout.tiles(), [3, 1, 4]);
    assert_eq!(g.layout.widths(), [2, 0, 2]);
    let r = TapeReader::open(&g.bytes[..]).unwrap();
    assert!(r.verify(0).unwrap().holds);
    let entries = r.track_entries(0, g.id).unwrap();
    assert_eq!(entries.len(), RUNS as usize * 12);
    let mut buf = vec![];
    for e in &entries {
        let head = r.read_chunk_head(0, e, &mut buf).unwrap();
        assert_eq!(e.tile, g.layout.pack(&head.tile), "{e:?}");
        assert!(e.range.is_none());
    }
    let last = entries.last().unwrap();
    assert_eq!(last.tile, Some(2 << 2 | 3), "tile (2, 0, 3)");
}

/// A series' entries carry no tile number: bytes 72..88 stay zero, as before grids.
#[test]
fn a_series_entry_carries_no_tile_number() {
    let g = write(&[], 1 << 20, true);
    assert_eq!(g.layout, TileLayout::SERIES);
    let r = TapeReader::open(&g.bytes[..]).unwrap();
    let entries = r.track_entries(0, g.id).unwrap();
    assert_eq!(entries.len(), RUNS as usize);
    assert!(entries.iter().all(|e| e.tile.is_none()));
}

/// A chunk whose coordinates name no tile of its grid is refused before anything is written: one past an axis'
/// tiles, one too few, and coordinates on a series.
#[test]
fn coordinates_outside_the_grid_are_refused() {
    for (extents, coords) in [
        (&EXTENTS[..], &[3, 0, 0][..]),
        (&EXTENTS[..], &[0, 1, 0][..]),
        (&EXTENTS[..], &[0, 0][..]),
        (&[][..], &[0][..]),
    ] {
        let axes = axes(extents);
        let track = grid_track(&axes);
        let id = track.id();
        let mut defs = ww_tape::Definitions::new();
        crate::definitions::hold(&mut defs, &[clock(CLOCK), ww_tape::Describe::Track(track)])
            .unwrap();
        let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
        let samples = [0u8; 4 * STEPS as usize];
        let spec = ColumnSpec {
            kind: ColumnKind::Samples,
            codec: Codec::RAW,
            param: 0,
            len: samples.len(),
        };
        let h = head(id, 0, TileCoords::new(coords).unwrap());
        let r = w.write_chunk(&defs, &h, &[(spec, &samples[..])]);
        assert!(
            matches!(
                r,
                Err(WriteError::Describe(DescribeError::Kind {
                    error: SeriesError::TileOutside { .. },
                    ..
                }))
            ),
            "{coords:?}: {r:?}"
        );
        assert_eq!(w.chunks(), 0);
    }
}

/// An overview chunk of a level past its grid's levels is refused before anything is written: a tile extent of 3
/// halves no time.
#[test]
fn an_overview_past_the_grid_levels_is_refused() {
    let axes = axes(&EXTENTS);
    let track = grid_track(&axes);
    let id = track.id();
    let mut defs = ww_tape::Definitions::new();
    crate::definitions::hold(&mut defs, &[clock(CLOCK), ww_tape::Describe::Track(track)]).unwrap();
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    let stats = [0u8; 4 * STEPS as usize];
    let columns: Vec<(ColumnSpec, &[u8])> = [
        ColumnKind::OverviewMin,
        ColumnKind::OverviewMax,
        ColumnKind::OverviewMean,
    ]
    .into_iter()
    .map(|kind| {
        let spec = ColumnSpec {
            kind,
            codec: Codec::RAW,
            param: 0,
            len: stats.len(),
        };
        (spec, &stats[..])
    })
    .collect();
    let h = ww_tape::ChunkHead {
        level: 1,
        ..head(id, 0, TileCoords::new(&[0, 0, 0]).unwrap())
    };
    let r = w.write_chunk(&defs, &h, &columns);
    assert!(
        matches!(
            r,
            Err(WriteError::Describe(DescribeError::Kind {
                error: SeriesError::LevelOutside { level: 1, .. },
                ..
            }))
        ),
        "{r:?}"
    );
    assert_eq!(w.chunks(), 0);
}

/// The tiles a box query must find: every chunk whose ticks meet `time` and whose tile lies in the box, found by
/// reading every chunk's head.
fn by_heads(
    r: &TapeReader<&[u8]>,
    g: &Written,
    time: &Range<i64>,
    cells: &[Range<u32>],
) -> Vec<u64> {
    let mut buf = vec![];
    let tiles = g.layout.tile_box(cells).unwrap();
    r.track_entries(0, g.id)
        .unwrap()
        .into_iter()
        .filter(|e| {
            let h = r.read_chunk_head(0, e, &mut buf).unwrap();
            h.first_tick < time.end && time.start <= h.last_tick && tiles.holds_coords(&h.tile)
        })
        .map(|e| e.offset)
        .collect()
}

/// Boxes and time ranges, edge tiles and empty boxes among them.
fn queries() -> Vec<(Range<i64>, Vec<Range<u32>>)> {
    let all = i64::MIN..i64::MAX;
    let run1 = START + 4_000..START + 8_000;
    vec![
        (all.clone(), vec![0..5, 0..4, 0..10]),
        (all.clone(), vec![2..3, 0..4, 9..10]),
        (run1.clone(), vec![4..5, 1..2, 0..4]),
        (run1, vec![0..1, 0..4, 3..6]),
        (START + 3_000..START + 4_001, vec![0..5, 0..4, 6..9]),
        (all.clone(), vec![3..3, 0..4, 0..10]),
        (all, vec![5..9, 0..4, 0..10]),
        (i64::MIN..START, vec![0..5, 0..4, 0..10]),
    ]
}

/// A box query finds the tiles reading every head finds, from the final index, from checkpoints and over a stretch
/// a reader scanned (entries without a tile number): the same tiles, in file order.
#[test]
fn a_box_query_finds_the_tiles_every_head_names() {
    for (checkpoint, close) in [(1 << 20, true), (2048, false), (1 << 20, false)] {
        let g = write(&EXTENTS, checkpoint, close);
        let r = TapeReader::open(&g.bytes[..]).unwrap();
        let entries = r.track_entries(0, g.id).unwrap();
        let scanned = entries.iter().filter(|e| e.tile.is_none()).count();
        match (close, checkpoint) {
            (true, _) => assert_eq!(scanned, 0),
            (false, 2048) => assert!(scanned > 0 && scanned < entries.len(), "{scanned}"),
            _ => assert_eq!(scanned, entries.len()),
        }
        let sizes: Vec<usize> = queries()
            .iter()
            .map(|(time, cells)| by_heads(&r, &g, time, cells).len())
            .collect();
        assert_eq!(sizes, [36, 3, 2, 1, 6, 0, 0, 0]);
        for (time, cells) in queries() {
            let tiles = g.layout.tile_box(&cells).unwrap();
            let found = r.grid_tiles(0, g.id, time.clone(), &tiles).unwrap();
            let offsets: Vec<u64> = found.iter().map(|t| t.entry.offset).collect();
            assert_eq!(
                offsets,
                by_heads(&r, &g, &time, &cells),
                "{time:?} {cells:?}"
            );
            for t in &found {
                assert!(tiles.holds_coords(&t.tile));
            }
        }
    }
}

/// A box query on a series is a time query: each chunk whose ticks meet the range, no coordinates.
#[test]
fn a_box_query_on_a_series_tests_time_alone() {
    let g = write(&[], 1 << 20, true);
    let r = TapeReader::open(&g.bytes[..]).unwrap();
    let tiles = g.layout.tile_box(&[]).unwrap();
    let found = r
        .grid_tiles(0, g.id, START + 4_000..START + 4_001, &tiles)
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].entry.first_item, ItemNum::new(4));
    assert!(found[0].tile.is_empty());
}

/// Coordinates of the golden grids' axes, listed: as many as the longest axis' cells.
static LISTED: [i64; 4] = [-500, 0, 250, 1_000_000];

/// The axes of a golden grid of `n` fixed axes: 2 to 4 cells, tiles of 1 to 2, regular and listed coordinates in
/// turn, a scale and a downsample flag each.
fn golden_axes(n: u32) -> Vec<FixedAxis<'static>> {
    (0..n)
        .map(|k| {
            let extent = 2 + k % 3;
            FixedAxis {
                name: Box::leak(format!("axis{k}").into_boxed_str()),
                extent: UNib32(extent),
                tile_extent: UNib32(1 + k % 2),
                unit: None,
                scale: Magnitude::pow10(-(k as i8 % 4)),
                coords: match k % 2 {
                    0 => AxisCoords::Regular {
                        origin: -i64::from(k),
                        step: 1 + i64::from(k),
                    },
                    _ => AxisCoords::Listed(RefVec::Slice {
                        slice: &LISTED[..extent as usize],
                    }),
                },
                downsample: k % 3 != 2,
            }
        })
        .collect()
}

/// Three tiles of a layout: the first, the last and one between.
fn three_tiles(layout: &TileLayout) -> Vec<TileCoords> {
    let first: Vec<u32> = layout.tiles().iter().map(|_| 0).collect();
    let last: Vec<u32> = layout.tiles().iter().map(|t| t - 1).collect();
    let mid: Vec<u32> = layout
        .tiles()
        .iter()
        .enumerate()
        .map(|(k, t)| k as u32 % t)
        .collect();
    [first, mid, last]
        .iter()
        .map(|c| TileCoords::new(c).unwrap())
        .collect()
}

/// A golden piece per axis count, 1, 2, 5 and 31 fixed axes: the grid's definition, its chunks' tile coordinates and
/// their entries' tile numbers pin their bytes (`WW_TAPE_GOLDEN=update` rewrites them). The same input gives the same
/// bytes.
#[test]
fn golden_grids() {
    for n in [1, 2, 5, 31] {
        let g = write_tiles(&golden_axes(n), three_tiles, 1 << 20, true);
        assert_eq!(g.layout.axes(), n as usize);
        let again = write_tiles(&golden_axes(n), three_tiles, 1 << 20, true);
        assert_eq!(g.bytes, again.bytes, "{n} axes");
        let r = TapeReader::open(&g.bytes[..]).unwrap();
        assert!(r.verify(0).unwrap().holds);
        let entries = r.track_entries(0, g.id).unwrap();
        assert!(entries.iter().all(|e| e.tile.is_some()));
        crate::reclose::golden(&g.bytes, &format!("v1-grid-{n}.hex"));
    }
}

/// A box made for another grid is refused, at the first chunk it would test: its layout isn't the track's.
#[test]
fn a_box_of_another_grid_is_refused() {
    let g = write(&EXTENTS, 1 << 20, true);
    let r = TapeReader::open(&g.bytes[..]).unwrap();
    let cells = [0..5, 0..4, 0..10];
    let other = TileLayout::new(&[(5, 2), (4, 4), (10, 2)]).unwrap();
    for tiles in [
        other.tile_box(&cells).unwrap(),
        TileLayout::SERIES.tile_box(&[]).unwrap(),
    ] {
        let e = r
            .grid_tiles(0, g.id, i64::MIN..i64::MAX, &tiles)
            .unwrap_err();
        assert!(
            matches!(e, ReadError::GridLayout { track_id, .. } if track_id == g.id),
            "{e}"
        );
    }
    let own = g.layout.tile_box(&cells).unwrap();
    assert_eq!(
        r.grid_tiles(0, g.id, i64::MIN..i64::MAX, &own)
            .unwrap()
            .len(),
        36
    );
}

/// An empty time range meets no chunk, also one whose ticks span it.
#[test]
fn an_empty_time_range_finds_no_chunk() {
    let g = write(&EXTENTS, 1 << 20, true);
    let r = TapeReader::open(&g.bytes[..]).unwrap();
    let tiles = g.layout.tile_box(&[0..5, 0..4, 0..10]).unwrap();
    let mid = START + 1_500;
    assert!(
        !r.grid_tiles(0, g.id, mid..mid + 1, &tiles)
            .unwrap()
            .is_empty()
    );
    assert!(r.grid_tiles(0, g.id, mid..mid, &tiles).unwrap().is_empty());
}

/// An entry whose tile number has bits outside the fields names no tile of the grid: the query reads its chunk's head
/// instead and finds the tile there, as for an entry without a number.
#[test]
fn a_tile_number_outside_the_fields_falls_back_to_the_head() {
    let g = write(&EXTENTS, 1 << 20, false);
    let mut bytes = g.bytes.clone();
    // a checkpoint over every block, each grid chunk's number with its top bit set
    let mut entries = vec![];
    let mut at = 0;
    let mut last = None;
    while let Some(b) = bytes.get(at..).and_then(|r| Block::read(r).ok()) {
        for mut e in IndexEntry::all_of_block(at as u64, &b) {
            if e.kind == BlockKind::Series {
                let h = ChunkHead::read_head(b.bytes()).unwrap();
                e.tile = Some(g.layout.pack(&h.tile).unwrap() | 1 << 127);
            }
            entries.push(e);
        }
        last = Some((b.session, b.seq));
        at += b.bytes().len();
    }
    assert_eq!(at, bytes.len());
    let (session, seq) = last.unwrap();
    let mut buf = vec![0u8; index_len(entries.len(), 0)];
    let head = IndexHead {
        kind: IndexKind::Checkpoint,
        self_offset: at as u64,
        prev: None,
    };
    let n = write_index(&mut buf, BlockId::new(session, seq + 1), &head, &entries).unwrap();
    bytes.extend_from_slice(&buf[..n]);
    let r = TapeReader::open(&bytes[..]).unwrap();
    let listed = r.track_entries(0, g.id).unwrap();
    assert!(listed.iter().all(|e| e.tile.is_some_and(|n| n >> 127 == 1)));
    let plain = TapeReader::open(&g.bytes[..]).unwrap();
    for (time, cells) in queries() {
        let tiles = g.layout.tile_box(&cells).unwrap();
        let found = r.grid_tiles(0, g.id, time.clone(), &tiles).unwrap();
        let scanned = plain.grid_tiles(0, g.id, time, &tiles).unwrap();
        let tiles_of = |v: &[ww_tape::GridTile]| -> Vec<(u64, TileCoords)> {
            v.iter().map(|t| (t.entry.offset, t.tile)).collect()
        };
        assert_eq!(tiles_of(&found), tiles_of(&scanned), "{cells:?}");
    }
}

/// Units of three chunks each ([TapeWriter::write_chunk_unit]) with a checkpoint due after every block: each chunk
/// but a unit's last is marked FLAG_WITH_NEXT, nothing of the writer's own but marked puts lies inside a unit, the
/// checkpoint follows
/// the unit; a resume of the bytes cut anywhere inside a unit keeps the bytes before it. A unit of two tracks is
/// refused before anything is written.
#[test]
fn a_unit_is_written_whole_and_dropped_whole() {
    for alignment in [0, ww_tape::ALIGNMENT] {
        let axes = axes(&EXTENTS);
        let track = grid_track(&axes);
        let layout = track.series().unwrap().tile_layout().unwrap();
        let id = track.id();
        let mut defs = ww_tape::Definitions::new();
        crate::definitions::hold(&mut defs, &[clock(CLOCK), ww_tape::Describe::Track(track)])
            .unwrap();
        let mut w = crate::resume::begin(alignment);
        w.set_index_policy(IndexPolicy {
            bytes: exact_units::Bytes::new(1),
            interval: None,
            ..IndexPolicy::default()
        })
        .unwrap();
        let tape = w.sink().tape();
        let samples = [0u8; 4 * STEPS as usize];
        let spec = ColumnSpec {
            kind: ColumnKind::Samples,
            codec: Codec::RAW,
            param: 0,
            len: samples.len(),
        };
        let columns = [(spec, &samples[..])];
        let tiles = all_tiles(&layout);
        for run in 0..RUNS {
            let heads: Vec<ChunkHead> = tiles[..3].iter().map(|t| head(id, run, *t)).collect();
            let unit: Vec<ww_tape::UnitChunk<'_>> = heads
                .iter()
                .map(|h| ww_tape::UnitChunk {
                    head: h,
                    columns: &columns,
                    payload_hash: None,
                })
                .collect();
            w.write_chunk_unit(&defs, &unit).unwrap();
            assert!(!w.unit_open(), "a unit written whole is closed");
        }
        let before = tape.to_vec().unwrap();
        let other = ChunkHead {
            track_number: TrackNum::new(2),
            ..head(id, RUNS, tiles[0])
        };
        let first = head(id, RUNS, tiles[0]);
        let mixed = [&first, &other].map(|h| ww_tape::UnitChunk {
            head: h,
            columns: &columns,
            payload_hash: None,
        });
        assert!(matches!(
            w.write_chunk_unit(&defs, &mixed),
            Err(WriteError::Input(ww_tape::InputError::UnitTracks))
        ));
        assert!(
            tape.to_vec().unwrap() == before,
            "a refused unit writes nothing"
        );
        drop(w);
        let bytes = tape.to_vec().unwrap();
        // the blocks in file order: chunks in runs of three, marked but the last, and no other block inside a run
        let blocks = crate::follow::walk(&bytes);
        let mut run_len = 0;
        let mut units = vec![];
        for (offset, b) in &blocks {
            let b = Block::read(b).unwrap();
            let marked = b.flags & ww_tape::FLAG_WITH_NEXT != 0;
            if b.chunk().is_some() {
                run_len += 1;
                assert_eq!(
                    marked,
                    run_len < 3,
                    "alignment {alignment}: chunk at {offset}"
                );
                if run_len == 1 {
                    units.push(*offset as usize);
                }
                if run_len == 3 {
                    run_len = 0;
                }
            } else {
                // inside a unit only puts belonging with the chunk after them, as the near policy asks
                assert!(
                    run_len == 0 || (marked && b.is_repeat()),
                    "alignment {alignment}: a {:?} block inside a unit at {offset}",
                    b.kind
                );
                assert!(
                    !marked || b.is_repeat(),
                    "only a unit's chunks or puts are marked"
                );
            }
        }
        assert_eq!(units.len(), RUNS as usize);
        let index_after = |at: usize| {
            blocks
                .iter()
                .filter(|(o, _)| *o as usize > at)
                .any(|(_, b)| Block::read(b).unwrap().kind == BlockKind::Index)
        };
        assert!(index_after(units[0]), "a checkpoint follows the first unit");
        // a cut anywhere from a unit's first chunk to inside its last: the resume keeps what lies before the unit
        for &start in &units {
            let end = blocks
                .iter()
                .filter(|(o, b)| *o as usize >= start && Block::read(b).unwrap().chunk().is_some())
                .nth(2)
                .map(|(o, b)| *o as usize + b.len())
                .unwrap();
            for cut in [start + 1, (start + end) / 2, end - 1] {
                let mut sink = MemSink::new();
                ww_tape::TapeSink::write_all(&mut sink, &bytes[..cut]).unwrap();
                let (_, resumed) =
                    TapeWriter::resume_on(sink, crate::resume::options(alignment)).unwrap();
                assert!(
                    resumed.kept_bytes <= start as u64,
                    "alignment {alignment}: cut at {cut} in the unit at {start}, kept {}",
                    resumed.kept_bytes
                );
            }
        }
    }
}

/// A grid writer's definitions, a writer of a piece begun, and the samples of a chunk of [STEPS] time steps.
fn unit_writer() -> (
    ww_tape::Definitions,
    TapeWriter<MemSink>,
    TrackId,
    Vec<TileCoords>,
) {
    let axes = axes(&EXTENTS);
    let track = grid_track(&axes);
    let layout = track.series().unwrap().tile_layout().unwrap();
    let id = track.id();
    let mut defs = ww_tape::Definitions::new();
    crate::definitions::hold(&mut defs, &[clock(CLOCK), ww_tape::Describe::Track(track)]).unwrap();
    (defs, crate::resume::begin(0), id, all_tiles(&layout))
}

/// Raw samples of one chunk, `len` bytes long in its column spec.
fn samples_column(samples: &[u8], len: usize) -> [(ColumnSpec, &[u8]); 1] {
    let spec = ColumnSpec {
        kind: ColumnKind::Samples,
        codec: Codec::RAW,
        param: 0,
        len,
    };
    [(spec, samples)]
}

/// A unit of a chunk of `columns` per head.
fn unit_of<'a>(
    heads: &'a [ChunkHead],
    columns: &'a [(ColumnSpec, &'a [u8])],
) -> Vec<ww_tape::UnitChunk<'a>> {
    heads
        .iter()
        .map(|h| ww_tape::UnitChunk {
            head: h,
            columns,
            payload_hash: None,
        })
        .collect()
}

/// A unit holds back the writer's own blocks and its cuts, so it has at most UNIT_CHUNKS_MAX chunks: a longer
/// [TapeWriter::write_chunk_unit] is refused whole, and a caller's (a relay's) chunk marked FLAG_WITH_NEXT that would
/// make a longer unit is refused, nothing written either way.
#[test]
fn a_unit_longer_than_its_cap_is_refused() {
    use ww_tape::writer::UNIT_CHUNKS_MAX;
    assert_eq!(
        UNIT_CHUNKS_MAX,
        usize::from(ww_tape::grid::GRID_OVERVIEW_LEVELS_MAX) + 1,
        "a tile chunk and one overview chunk per level"
    );
    let (defs, mut w, id, tiles) = unit_writer();
    let samples = [0u8; 4 * STEPS as usize];
    let columns = samples_column(&samples, samples.len());
    let heads: Vec<ChunkHead> = (0..RUNS)
        .flat_map(|run| tiles.iter().map(move |t| head(id, run, *t)))
        .take(UNIT_CHUNKS_MAX + 1)
        .collect();
    assert_eq!(heads.len(), UNIT_CHUNKS_MAX + 1);
    let unit = |heads| unit_of(heads, &columns);
    let tape = w.sink().tape();
    let before = tape.to_vec().unwrap();
    // puts of the piece head may wait for the first chunk, marked as belonging with it
    let open = w.unit_open();
    assert!(matches!(
        w.write_chunk_unit(&defs, &unit(&heads)),
        Err(WriteError::Input(ww_tape::InputError::UnitTooLong {
            max: UNIT_CHUNKS_MAX
        }))
    ));
    assert!(
        tape.to_vec().unwrap() == before,
        "a refused unit writes nothing"
    );
    assert_eq!(w.unit_open(), open);
    w.write_chunk_unit(&defs, &unit(&heads[..UNIT_CHUNKS_MAX]))
        .unwrap();
    assert!(!w.unit_open());

    // the marked chunks of two units another writer made: a relay passing on the first's without its last chunk
    let all: Vec<ChunkHead> = (0..RUNS)
        .flat_map(|run| tiles.iter().map(move |t| head(id, run, *t)))
        .collect();
    let (defs, mut src, _, _) = unit_writer();
    let src_tape = src.sink().tape();
    let mut placed = vec![];
    for heads in all.chunks(UNIT_CHUNKS_MAX).take(2) {
        assert_eq!(heads.len(), UNIT_CHUNKS_MAX);
        placed.extend(src.write_chunk_unit(&defs, &unit(heads)).unwrap());
    }
    drop(src);
    let src_bytes = src_tape.to_vec().unwrap();
    let marked: Vec<Vec<u8>> = placed
        .iter()
        .map(|p| {
            let at = usize::try_from(p.offset).unwrap();
            src_bytes[at..at + p.len as usize].to_vec()
        })
        .filter(|b| Block::read(b).unwrap().flags & ww_tape::FLAG_WITH_NEXT != 0)
        .collect();
    assert_eq!(marked.len(), 2 * (UNIT_CHUNKS_MAX - 1));
    let (_, mut relay, _, _) = unit_writer();
    let tape = relay.sink().tape();
    for b in &marked[..UNIT_CHUNKS_MAX - 1] {
        relay.write_block(&defs, b).unwrap();
        assert!(relay.unit_open());
    }
    let before = tape.to_vec().unwrap();
    assert!(matches!(
        relay.write_block(&defs, &marked[UNIT_CHUNKS_MAX - 1]),
        Err(WriteError::Input(ww_tape::InputError::UnitTooLong {
            max: UNIT_CHUNKS_MAX
        }))
    ));
    assert!(
        tape.to_vec().unwrap() == before,
        "a refused chunk writes nothing"
    );
}
