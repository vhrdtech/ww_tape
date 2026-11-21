//! The two halves of a summary builder, run as a producer and a writing thread would, against the whole builder.

use exact_units::Magnitude;
use ww_tape::{Element, ElementDesc, ItemNum, Ratio64, SampleShape, SeriesGrid, TrackId, TrackNum};

use crate::tests::{Rng, spans};
use crate::{
    EventFields, EventRuns, EventTree, SeriesLeaves, SeriesStart, SeriesStats, SeriesSummaries,
    SeriesSummarySpec, SeriesTree,
};

/// Chunk sizes from `rng`, 1 to `max` items, summing to `n`.
fn chunks(n: usize, max: u64, rng: &mut Rng) -> Vec<usize> {
    let mut out = Vec::new();
    let mut left = n;
    while left > 0 {
        let k = (rng.below(max) as usize + 1).min(left);
        out.push(k);
        left -= k;
    }
    out
}

/// Series leaves sealed at every chunk's end and merged by the tree give the blocks the whole builder gives: integer
/// statistics merge exactly.
#[test]
fn series_leaves_and_tree_write_what_the_whole_builder_writes() {
    let mut rng = Rng(7);
    let shape = SampleShape::new(ElementDesc::of(Element::I16), 2);
    let data: Vec<i16> = (0..2 * 150_000)
        .map(|_| (rng.next() >> 48) as i16)
        .collect();
    let spec = SeriesSummarySpec {
        base_shift: 8,
        stats: SeriesStats::all(shape.element),
        clip: None,
    };
    for first in [0u64, 300, 65_536 - 5] {
        let start = SeriesStart {
            track_id: TrackId::new(9),
            track_number: TrackNum::new(3),
            shape,
            first_item: ItemNum::new(first),
            grid: SeriesGrid::start(first, 0, Magnitude::ONE),
            period: Ratio64::new(1000, 1).unwrap(),
        };
        let sizes = chunks(data.len() / 2, 3000, &mut rng);

        let mut whole = SeriesSummaries::<i16>::new(start, spec).unwrap();
        let mut leaves =
            SeriesLeaves::<i16>::new(shape, start.grid.scale, start.first_item, spec).unwrap();
        let mut tree = SeriesTree::new(start, spec).unwrap();
        let (mut want, mut got) = (Vec::new(), Vec::new());
        let mut at = 0;
        for n in sizes {
            let chunk = &data[at * 2..(at + n) * 2];
            at += n;
            whole.push(chunk, 0).unwrap();
            want.extend(whole.take_blocks());
            // the producer's half
            leaves.push(chunk, 0).unwrap();
            leaves.seal().unwrap();
            // the writing thread's half
            tree.samples(n as u64).unwrap();
            for leaf in leaves.take_leaves() {
                tree.push_leaf(leaf).unwrap();
            }
            got.extend(tree.take_blocks());
        }
        whole.finish().unwrap();
        want.extend(whole.take_blocks());
        tree.finish().unwrap();
        got.extend(tree.take_blocks());
        assert!(!want.is_empty(), "the series fills blocks");
        assert_eq!(got, want, "first item {first}");
    }
}

/// A tree refuses a leaf that doesn't follow the last one or lies past the samples added.
#[test]
fn series_tree_refuses_leaves_out_of_order() {
    let shape = SampleShape::scalar(ElementDesc::of(Element::I16));
    let spec = SeriesSummarySpec {
        base_shift: 4,
        stats: SeriesStats::all(shape.element),
        clip: None,
    };
    let start = SeriesStart {
        track_id: TrackId::new(9),
        track_number: TrackNum::new(3),
        shape,
        first_item: ItemNum::new(0),
        grid: SeriesGrid::start(0, 0, Magnitude::ONE),
        period: Ratio64::new(1, 1).unwrap(),
    };
    let mut leaves =
        SeriesLeaves::<i16>::new(shape, Magnitude::ONE, ItemNum::new(0), spec).unwrap();
    leaves.push(&[1, 2, 3], 0).unwrap();
    leaves.seal().unwrap();
    let leaf = leaves.take_leaves().pop().unwrap();
    let mut tree = SeriesTree::new(start, spec).unwrap();
    assert!(
        tree.push_leaf(leaf.clone()).is_err(),
        "no samples added yet"
    );
    tree.samples(3).unwrap();
    let mut late = leaf.clone();
    late.first_item = ItemNum::new(1);
    assert!(
        tree.push_leaf(late).is_err(),
        "a leaf past the last one's end"
    );
    tree.push_leaf(leaf).unwrap();
}

/// Event runs sealed at every chunk's end give the tree what the items one by one give it.
#[test]
fn event_runs_write_what_the_items_write() {
    let mut rng = Rng(11);
    let items = spans(40_000, &mut rng);
    let fields = EventFields {
        flags: true,
        latest_end: true,
    };
    for first in [0u64, 2047, 5000] {
        let first_item = ItemNum::new(first);
        let mut whole = EventTree::new(TrackId::new(5), TrackNum::new(2), fields, first_item);
        let mut tree = EventTree::new(TrackId::new(5), TrackNum::new(2), fields, first_item);
        let mut runs = EventRuns::new(fields, first_item);
        let mut at = 0;
        for n in chunks(items.len(), 3000, &mut rng) {
            for &(s, e, f) in &items[at..at + n] {
                whole.push(s, e, f).unwrap();
                runs.push(s, e, f).unwrap();
            }
            at += n;
            runs.seal();
            for (first, run) in runs.take_runs() {
                assert_eq!(first, tree.next_item(), "runs follow each other");
                tree.push_run(run).unwrap();
            }
        }
        whole.finish().unwrap();
        tree.finish().unwrap();
        assert_eq!(
            tree.take_blocks(),
            whole.take_blocks(),
            "first item {first}"
        );
    }
}

/// Runs refuse what the tree would: a start before the last, a span on a track of instants.
#[test]
fn event_runs_refuse_bad_items() {
    let mut runs = EventRuns::new(
        EventFields {
            flags: false,
            latest_end: false,
        },
        ItemNum::new(0),
    );
    runs.push(10, 10, 0).unwrap();
    assert!(runs.push(9, 9, 0).is_err(), "out of order");
    assert!(runs.push(11, 12, 0).is_err(), "a span");
}
