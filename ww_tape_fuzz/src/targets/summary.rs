//! ww_tape summary blocks on any bytes, and trees built from the input: reading never panics, every accessor of a
//! good summary block stays in bounds, an event node's flag counts are at most its items and above 0 exactly for the
//! bits of its flags OR, and every block a builder writes reads back with its nodes covering the items and counting
//! the flags of exactly those items, also across prefix changes of a series, whose nodes then merge at a common scale.
use ww_tape::summary::{
    EventTree, FlagCounts, ItemRun, SeriesStart, SeriesStats, SeriesSummaries, SeriesSummarySpec,
    Stat,
};
use ww_tape::{
    Block, BlockId, Element, EventFields, Magnitude, Ratio64, SampleShape, SeriesGrid, SessionTag,
    set_id,
};

/// Look at everything a summary block read from `data` gives.
pub(crate) fn walk(data: &[u8]) {
    let Ok(b) = Block::read(data) else { return };
    let Some(s) = b.summary() else { return };
    let h = s.head();
    assert_eq!(h.node_count(), s.node_count() as u64);
    for i in 0..s.node_count() {
        let (first, items) = h
            .node_items(i as u64)
            .expect("every node of a read block has items");
        assert!(items >= 1 && first >= h.first_item);
        if let Some(n) = s.event_node(i) {
            for j in 0..n.counts.known() {
                let c = n.counts.get(j).expect("a known bit has a count");
                assert!(c <= items, "a flag count above the node's items");
                assert_eq!(
                    n.flags >> j & 1 == 1,
                    c > 0,
                    "a flag count against the flags OR"
                );
            }
        }
        let _ = s.series_node(i);
        for ch in 0..4 {
            let _ = s.channel_stats(ch, i);
            for stat in Stat::ALL {
                for c in 0..3 {
                    if let Some(col) = s.series_column(stat, ch, c) {
                        assert_eq!(col.len(), s.node_count());
                        assert!(col.get(i).is_some());
                    }
                }
            }
        }
    }
    assert_eq!(s.scale().is_some(), s.event_node(0).is_none());
    let mut copy = b.bytes().to_vec();
    let id = BlockId::new(
        SessionTag::from_u32(b.session.get().wrapping_add(1)),
        b.seq.wrapping_add(1),
    );
    set_id(&mut copy, id).expect("a good block renumbers");
    assert!(Block::read(&copy).is_ok());
}

/// Every block reads back, and the level-0 nodes cover `[first, end)` once.
fn check(blocks: &[Vec<u8>], first: u64, end: u64) {
    let mut covered = 0u64;
    for b in blocks {
        walk(b);
        let s = Block::read(b)
            .expect("written good")
            .summary()
            .expect("a summary");
        let h = s.head();
        assert!(h.first_item >= first && h.first_item + h.items <= end);
        if h.level == 0 {
            covered += h.items;
        }
    }
    if end > first {
        assert_eq!(covered, end - first);
    }
}

/// Flag counts of a run of `count` items whose flags OR is `flags`: for each set bit, between 1 and `count`.
fn run_counts(flags: u32, count: u64) -> FlagCounts {
    let mut n = [0u64; 8];
    for (j, c) in n.iter_mut().enumerate() {
        if flags >> j & 1 == 1 {
            *c = 1 + (u64::from(flags) * (j as u64 + 3)) % count;
        }
    }
    FlagCounts::new(8, n).expect("8 bits counted")
}

/// Every node of every event block counts, per counted bit, the flags of exactly the items it covers: the sum over
/// the runs pushed that start inside it (a run never crosses a level-0 node, so never a node of any level).
fn check_counts(blocks: &[Vec<u8>], pushed: &[(u64, FlagCounts)], k: u8) {
    for b in blocks {
        let s = Block::read(b)
            .expect("written good")
            .summary()
            .expect("a summary");
        let h = s.head();
        for i in 0..s.node_count() {
            let (a, n) = h.node_items(i as u64).expect("a node of the block");
            let node = s.event_node(i).expect("an event node");
            assert_eq!(node.counts.known(), k, "the bits the tree counts");
            for j in 0..k {
                let want: u64 = pushed
                    .iter()
                    .filter(|(at, _)| *at >= a && *at < a + n)
                    .map(|(_, c)| c.get(j).expect("8 bits counted"))
                    .sum();
                assert_eq!(
                    node.counts.get(j),
                    Some(want),
                    "bit {j} of node {i} at level {}",
                    h.level
                );
            }
        }
    }
}

pub fn summary(data: &[u8]) {
    walk(data);
    if data.len() < 4 {
        return;
    }
    let first = u64::from(u16::from_le_bytes([data[0], data[1]])) * 37;
    let flags = data[2] & 1 != 0;
    let fields = EventFields {
        flags,
        latest_end: data[2] & 2 != 0,
        flag_counts: if flags { (data[2] >> 2) % 9 } else { 0 },
    };
    // each run or item pushed: its first item and its flag counts, the oracle of every node's counts
    let mut pushed: Vec<(u64, FlagCounts)> = Vec::new();
    let body = &data[4..];

    // an event tree: each input byte is a gap, a span length, flags, or a run
    let mut tree = EventTree::new(1, 1, fields, first);
    tree.set_max_block_bytes(96 + 64 * usize::from(data[3] % 8 + 1));
    let mut t = i64::from(data[3]) - 128;
    for &x in body {
        t += i64::from(x >> 2);
        let end = if fields.latest_end {
            t + i64::from(x & 3) * 1000
        } else {
            t
        };
        let at = tree.next_item();
        let (r, counts) = if x & 0x80 != 0 {
            let room = 2048 - tree.next_item() % 2048;
            let count = room.min(u64::from(x & 0x3f) + 1);
            let counts = run_counts(u32::from(x), count);
            let r = tree.push_run(ItemRun {
                count,
                first_tick: t,
                last_tick: t + 5,
                flags: u32::from(x),
                latest_end: end.max(t + 5),
                counts,
            });
            (r, counts)
        } else {
            let f = u32::from(x & 3);
            (tree.push(t, end, f), FlagCounts::of_item(f))
        };
        if r.is_err() {
            break;
        }
        pushed.push((at, counts));
        t += 5;
    }
    let end = tree.next_item();
    tree.finish().expect("finishes");
    let blocks = tree.take_blocks();
    check(&blocks, first, end);
    check_counts(&blocks, &pushed, fields.flag_counts);

    // a series of complex i16 from the input bytes
    let shape = SampleShape::new(Element::ComplexI16, 1 + u32::from(data[3] % 3));
    let lanes = shape.lanes().expect("lanes") as usize;
    let xs: Vec<i16> = body
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| i16::from_le_bytes(*c))
        .collect();
    let xs = &xs[..xs.len() / lanes * lanes];
    let start = SeriesStart {
        track_id: 2,
        track_number: 2,
        shape,
        first_item: first,
        grid: SeriesGrid::start(first, -5, Magnitude::pow10(-3)),
        period: Ratio64::new(3, 2).expect("a ratio"),
    };
    let spec = SeriesSummarySpec {
        base_shift: 4 + data[2] % 3,
        stats: SeriesStats::all(Element::ComplexI16),
        clip: None,
    };
    let mut s = SeriesSummaries::<i16>::new(start, spec).expect("a valid spec");
    s.set_max_block_bytes(512);
    let mut grid = start.grid;
    for (i, part) in xs
        .chunks(lanes * (1 + usize::from(data[3] % 5)))
        .enumerate()
    {
        // a prefix change (milli, micro, nano) where a part's first number says so
        if part.first().is_some_and(|x| x % 5 == 0) && s.next_item() > first {
            let e = -3 - 3 * (part[0] / 5).rem_euclid(3) as i8;
            grid = grid.rescaled(Magnitude::pow10(e));
            s.set_grid(grid).expect("an i16 rescaled to nano fits");
        }
        s.push(part, i as u32).expect("pushes");
    }
    s.finish().expect("finishes");
    let blocks = s.take_blocks();
    check(&blocks, first, first + (xs.len() / lanes) as u64);
    // every node of every block merges with every other at their common scale
    let nodes: Vec<_> = blocks
        .iter()
        .flat_map(|b| {
            let s = Block::read(b)
                .expect("written good")
                .summary()
                .expect("a summary");
            (0..s.node_count())
                .map(|i| s.series_node(i).expect("a series node"))
                .collect::<Vec<_>>()
        })
        .collect();
    if let Some((head, rest)) = nodes.split_first() {
        let mut all = head.clone();
        for n in rest {
            all.merge(n, 1, 1).expect("decimal scales merge");
        }
    }
}
