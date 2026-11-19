//! The head of a summary block in the core: written, checked and listed without its nodes.

use super::*;
use crate::tests::id;
use crate::{Block, BlockError};

/// The last node of a head at the top of the item numbers: its end, `(n + 1) << shift`, is past u64.
#[test]
fn node_items_at_the_top_of_the_item_numbers() {
    let h = SummaryHead {
        track_id: 1,
        track_number: 1,
        tree: Tree::Events,
        level: 0,
        node_shift: 62,
        layout: Layout::Events(EventFields::default()),
        first_item: 3 << 62,
        items: (1 << 62) - 1,
        first_tick: 0,
        last_tick: 0,
        latest_end: 0,
    };
    assert_eq!(h.node_count(), 1);
    assert_eq!(h.node_items(0), Some((3 << 62, (1 << 62) - 1)));
    assert_eq!(h.node_items(1), None);
    let wide = SummaryHead {
        node_shift: 200,
        first_item: 5,
        items: u64::MAX - 5,
        ..h
    };
    assert_eq!(wide.node_count(), 1);
    assert_eq!(wide.node_items(0), Some((5, u64::MAX - 5)));
    let none = SummaryHead { items: 0, ..h };
    assert_eq!((none.node_count(), none.node_items(0)), (0, None));
    let past = SummaryHead {
        items: u64::MAX,
        ..h
    };
    assert_eq!((past.node_count(), past.node_items(0)), (0, None));
}

/// A summary block read by a reader that decodes no node: [Block::read] checks its CRC, head and body length,
/// [Block::summary_head] gives what an index lists, and a lying head or a flipped bit is refused.
#[test]
fn a_block_checks_and_lists_without_its_nodes() {
    let h = SummaryHead {
        track_id: 9,
        track_number: 2,
        tree: Tree::Series,
        level: 1,
        node_shift: 10,
        layout: Layout::Series {
            element: crate::Element::ComplexI16,
            channels: 3,
            stats: SeriesStats::all(crate::Element::ComplexI16),
            scale: Magnitude::pow10(-3),
        },
        first_item: 1000,
        items: 5000,
        first_tick: 100,
        last_tick: 900,
        latest_end: 900,
    };
    let len = summary_len(&h).unwrap();
    let mut b = vec![0u8; len];
    let mut w = SummaryWriter::new(&mut b, id(7), &h).unwrap();
    assert_eq!(w.nodes(), 6);
    w.body_mut().fill(0x5a);
    assert_eq!(w.finish(), len);
    let block = Block::read(&b).unwrap();
    assert_eq!(block.summary_head(), Some(h));
    let (head, body) = SummaryHead::read(&b).unwrap();
    assert_eq!(head, h);
    assert!(body.iter().all(|v| *v == 0x5a));
    // per channel and node: 4 statistics of 2 × 8 bytes, the clipped count of 8, flags and 2 powers of 4
    assert_eq!(body.len(), 6 * 3 * 84);
    for at in [5usize, 40, 100, len - 1] {
        let mut x = b.clone();
        x[at] ^= 1;
        assert_eq!(
            Block::read(&x).unwrap_err(),
            BlockError::BadCrc,
            "byte {at}"
        );
    }
    // a head that lies under a good CRC: one node fewer than its items need
    let mut x = b.clone();
    x[12] = 5;
    let n = x.len();
    let crc = crate::CRC.checksum(&x[..n - 4]);
    x[n - 4..].copy_from_slice(&crc.to_le_bytes());
    assert!(matches!(Block::read(&x), Err(BlockError::Summary(_))));
}
