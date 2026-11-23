//! A grid tile's tree: an item is a time step, its samples the tile's cells.

use std::collections::BTreeMap;

use super::*;
use crate::tests::Rng;
use ww_tape::{
    Block, Element, ElementDesc, ItemNum, Magnitude, Ratio64, SampleShape, SeriesGrid, TileCoords,
    TrackId, TrackNum,
};

fn start(shape: SampleShape, tile: TileCoords, cells: u64, first: u64) -> SeriesStart {
    SeriesStart {
        track_id: TrackId::new(5),
        track_number: TrackNum::new(3),
        shape,
        first_item: ItemNum::new(first),
        grid: SeriesGrid::start(first, 0, Magnitude::ONE),
        period: Ratio64::int(10).unwrap(),
        tile,
        tile_cells: cells,
    }
}

fn spec(base_shift: u8, element: ElementDesc) -> SeriesSummarySpec {
    SeriesSummarySpec {
        base_shift,
        stats: SeriesStats::all(element),
        clip: None,
    }
}

/// Push `data` in uneven runs of `item` scalars, then finish: every block made.
fn blocks_of(mut s: SeriesSummaries<i16>, data: &[i16], item: usize) -> Vec<Vec<u8>> {
    let items = data.len() / item;
    let (mut at, mut k, mut out) = (0usize, 0usize, Vec::new());
    while at < items {
        let n = (1 + k * 7 % 23).min(items - at);
        s.push(&data[at * item..(at + n) * item], (k as u32 % 3) << 4)
            .unwrap();
        at += n;
        k += 1;
        out.extend(s.take_blocks());
    }
    s.finish().unwrap();
    out.extend(s.take_blocks());
    out
}

/// (level, node, channel) to (first item, items, stats, tile) of every node of `blocks`.
type Nodes = BTreeMap<(u8, u64, u16), (u64, u64, ChannelStats, TileCoords)>;

fn nodes(blocks: &[Vec<u8>]) -> Nodes {
    let mut m = BTreeMap::new();
    for b in blocks {
        let s = Summary::of_block(&Block::read(b).unwrap()).unwrap();
        let h = s.head();
        let Layout::Series { channels, .. } = h.layout else {
            panic!("a series block")
        };
        for i in 0..s.node_count() {
            let (first, items) = h.node_items(i as u64).unwrap();
            for ch in 0..channels {
                let stats = s.channel_stats(ch, i).unwrap();
                let old = m.insert(
                    (h.level, h.first_node() + i as u64, ch),
                    (first, items, stats, h.tile),
                );
                assert!(old.is_none(), "node written twice");
            }
        }
    }
    m
}

/// A tile of 2^k cells is the series of the same samples with a base 2^k larger: the same nodes, statistics and
/// levels, its items 2^k times fewer; its blocks carry the tile.
#[test]
fn a_tile_of_four_cells_is_a_series_of_a_base_four_times_larger() {
    let shape = SampleShape::new(ElementDesc::of(Element::ComplexI16), 2);
    let mut rng = Rng(7);
    let data: Vec<i16> = (0..4 * 4 * 3000)
        .map(|i| {
            if i % 997 == 0 {
                i16::MAX
            } else {
                (rng.next() >> 50) as i16 - 4000
            }
        })
        .collect();
    let tile = TileCoords::new(&[2, 0, 9]).unwrap();
    let el = shape.element;
    for first in [0u64, 37] {
        let t = SeriesSummaries::<i16>::new(start(shape, tile, 4, first), spec(4, el)).unwrap();
        let s =
            SeriesSummaries::<i16>::new(start(shape, TileCoords::NONE, 1, first * 4), spec(6, el))
                .unwrap();
        let tiles = blocks_of(t, &data, 4 * 4);
        let series = blocks_of(s, &data, 4 * 4);
        let (t, s) = (nodes(&tiles), nodes(&series));
        assert_eq!(t.len(), s.len());
        assert!(t.len() > 3000 / 16 * 2, "levels above 0");
        for ((k, a), (l, b)) in t.iter().zip(&s) {
            assert_eq!(k, l);
            assert_eq!((a.0 * 4, a.1 * 4), (b.0, b.1), "{k:?}");
            assert_eq!(a.2, b.2, "{k:?}");
            assert_eq!(a.3, tile);
            assert_eq!(b.3, TileCoords::NONE);
        }
        for b in &tiles {
            assert_eq!(u16::from_le_bytes([b[6], b[7]]), ww_tape::FLAG_TILE);
        }
    }
}

/// A tile of 3 cells, one channel: each level-0 node holds 16 time steps of 3 cells, its minimum, maximum, sum and
/// flags those of its 48 samples.
#[test]
fn a_tile_of_three_cells_matches_a_naive_model() {
    let shape = SampleShape::scalar(ElementDesc::of(Element::I16));
    let data: Vec<i16> = (0..3 * 1000)
        .map(|i| ((i * 7919) % 6007) as i16 - 3000)
        .collect();
    let tile = TileCoords::new(&[1]).unwrap();
    let t = SeriesSummaries::<i16>::new(start(shape, tile, 3, 0), spec(4, shape.element)).unwrap();
    let got = nodes(&blocks_of(t, &data, 3));
    for n in 0..1000u64.div_ceil(16) {
        let (first, items, stats, _) = got[&(0, n, 0)];
        assert_eq!(first, n * 16);
        assert_eq!(items, 16.min(1000 - n * 16));
        let cells = &data[first as usize * 3..(first + items) as usize * 3];
        let min = *cells.iter().min().unwrap() as i64;
        let max = *cells.iter().max().unwrap() as i64;
        let sum: i64 = cells.iter().map(|v| *v as i64).sum();
        assert_eq!(stats.min[0], Value::I64(min));
        assert_eq!(stats.max[0], Value::I64(max));
        assert_eq!(stats.sum[0], sum as f64);
    }
}

/// Samples that are not whole time steps of the tile are refused, as are a tile of no cells, more than
/// [GRID_TILE_CELLS_MAX](ww_tape::grid::GRID_TILE_CELLS_MAX) or a series of more than one.
#[test]
fn a_tile_refuses_partial_steps_and_bad_cells() {
    let shape = SampleShape::new(ElementDesc::of(Element::I16), 2);
    let tile = TileCoords::new(&[0, 0]).unwrap();
    let sp = spec(4, shape.element);
    let mut t = SeriesSummaries::<i16>::new(start(shape, tile, 5, 0), sp).unwrap();
    assert_eq!(
        t.push(&[0; 8], 0),
        Err(BuildError::PartialFrame { stride: 10 })
    );
    t.push(&[0; 20], 0).unwrap();
    assert_eq!(t.next_item(), ItemNum::new(2));
    let max = ww_tape::grid::GRID_TILE_CELLS_MAX;
    for (tile, cells) in [(tile, 0), (tile, max + 1), (TileCoords::NONE, 2)] {
        assert!(SeriesSummaries::<i16>::new(start(shape, tile, cells, 0), sp).is_err());
        assert!(SeriesTree::new(start(shape, tile, cells, 0), sp).is_err());
    }
    assert!(SeriesLeaves::<i16>::of_tile(shape, Magnitude::ONE, ItemNum::new(0), sp, 0).is_err());
}
