//! The head of a summary block in the core: written, checked and listed without its nodes.

use crate::ItemNum;

use super::*;
use crate::ClockNum;
use crate::tests::id;
use crate::{Block, BlockError};

/// The last node of a head at the top of the item numbers: its end, `(n + 1) << shift`, is past u64.
#[test]
fn node_items_at_the_top_of_the_item_numbers() {
    let h = SummaryHead {
        track_id: TrackId::new(1),
        track_number: TrackNum::new(1),
        tree: Tree::Events,
        level: 0,
        node_shift: 62,
        layout: Layout::Events(EventFields::default()),
        first_item: ItemNum::new(3 << 62),
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
        first_item: ItemNum::new(5),
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
        track_id: TrackId::new(9),
        track_number: TrackNum::new(2),
        tree: Tree::Series,
        level: 1,
        node_shift: 10,
        layout: Layout::Series {
            element: crate::ElementDesc::of(crate::Element::ComplexI16),
            channels: 3,
            stats: SeriesStats::all(crate::ElementDesc::of(crate::Element::ComplexI16)),
            scale: Magnitude::pow10(-3),
        },
        first_item: ItemNum::new(1000),
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
    let (head, body, nodes) = SummaryHead::read(&b).unwrap();
    assert_eq!(nodes, 6);
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

/// [SummaryHead::read] checks the header itself: bytes of another magic, version or kind, or a length field other
/// than the bytes handed over, are refused with what they are, even under a good CRC.
#[test]
fn head_read_checks_the_header() {
    let h = SummaryHead {
        track_id: TrackId::new(1),
        track_number: TrackNum::new(1),
        tree: Tree::Events,
        level: 0,
        node_shift: 11,
        layout: Layout::Events(EventFields::default()),
        first_item: ItemNum::new(0),
        items: 10,
        first_tick: 0,
        last_tick: 9,
        latest_end: 9,
    };
    let mut b = vec![0u8; summary_len(&h).unwrap()];
    SummaryWriter::new(&mut b, id(1), &h).unwrap().finish();
    assert!(SummaryHead::read(&b).is_ok());
    let changed = |at: usize, v: u8| {
        let mut x = b.clone();
        x[at] = v;
        let n = x.len();
        let crc = crate::CRC.checksum(&x[..n - 4]);
        x[n - 4..].copy_from_slice(&crc.to_le_bytes());
        SummaryHead::read(&x).map(|(head, _, _)| head)
    };
    assert_eq!(
        changed(0, b'X'),
        Err(SummaryError::NotSummary(NotSummary::Magic))
    );
    assert_eq!(
        changed(4, 2),
        Err(SummaryError::NotSummary(NotSummary::Version(2)))
    );
    assert_eq!(
        changed(5, crate::BlockKind::Events.to_u8()),
        Err(SummaryError::NotSummary(NotSummary::Kind(
            crate::BlockKind::Events
        )))
    );
    let mut longer = b.clone();
    longer.extend_from_slice(&[0; 8]);
    assert_eq!(
        SummaryHead::read(&longer).map(|(head, _, _)| head),
        Err(SummaryError::NotSummary(NotSummary::Length {
            stated: b.len() as u32,
            bytes: b.len() + 8
        }))
    );
}

/// The definitions of series track 1 on clock 1 (1 ns ticks): i16 samples on one channel, one per 1000 ticks.
fn series_definitions() -> Vec<crate::Describe<'static>> {
    use crate::{
        ClockDef, ClockOrigin, Describe, GridDef, TrackDef, TrackKind, TrackRepeat, clock_key,
        track_key,
    };
    use shrink_wrap::prelude::*;
    vec![
        Describe::Clock(ClockDef {
            number: ClockNum::new(1),
            key: clock_key(0x5e, &[], 1),
            rate: Magnitude::pow10(9).into(),
            resolution: Magnitude::pow10(-9).into(),
            origin: ClockOrigin::Boot,
            label: "dev",
        }),
        Describe::Track(TrackDef {
            number: TrackNum::new(1),
            key: track_key(0x5e, &[crate::Segment::Name("signal")]),
            clock: ClockNum::new(1),
            dimensions: RefVec::Slice { slice: &[] },
            items: crate::Items::numbers::<i16>(None),
            label: "signal",
            repeat: TrackRepeat::Stream,
            kind: TrackKind::Series(GridDef::of::<i16>(1, crate::Ratio64::int(1000).unwrap())),
            relations: RefVec::Slice { slice: &[] },
            owner: [0; 32],
        }),
    ]
}

/// The id of the track of [series_definitions].
fn series_track_id() -> TrackId {
    series_definitions()
        .iter()
        .find_map(|d| match d {
            crate::Describe::Track(t) => Some(t.id()),
            _ => None,
        })
        .unwrap()
}

/// A level-0 series summary block over samples `[first, first + count)` written by the core's [SummaryWriter], its
/// node bytes arbitrary.
fn series_summary_block(first: u64, count: u64) -> Vec<u8> {
    let h = SummaryHead {
        track_id: series_track_id(),
        track_number: TrackNum::new(1),
        tree: Tree::Series,
        level: 0,
        node_shift: 10,
        layout: Layout::Series {
            element: ElementDesc::of(Element::I16),
            channels: 1,
            stats: SeriesStats::all(ElementDesc::of(Element::I16)),
            scale: Magnitude::ONE,
        },
        first_item: ItemNum::new(first),
        items: count,
        first_tick: first as i64 * 1000,
        last_tick: (first + count - 1) as i64 * 1000,
        latest_end: (first + count - 1) as i64 * 1000,
    };
    let mut b = vec![0u8; summary_len(&h).unwrap()];
    let mut w = SummaryWriter::new(&mut b, id(0), &h).unwrap();
    for (i, v) in w.body_mut().iter_mut().enumerate() {
        *v = (i as u8).wrapping_mul(13).wrapping_add(first as u8);
    }
    let n = w.finish();
    b.truncate(n);
    b
}

/// A tape written and read with the core alone: summary blocks from [SummaryWriter] go into the gaps before aligned
/// chunks, a walk skips them by kind with their head read, the final index lists them under their track, and verify
/// covers them: a node byte changed under a good CRC, a head field changed under a good CRC and a flipped bit each
/// fail it.
#[test]
fn a_core_only_tape_lists_verifies_and_skips_summary_blocks() {
    use crate::{
        BlockKind, ChunkHead, Codec, ColumnKind, ColumnSpec, Definitions, EndMode, FileBlocks,
        MemSink, SeriesGrid, SyncPolicy, TapeReader, TapeWriter,
    };
    let chunk = 32_768usize;
    let chunks = 12usize;
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    let tape = w.sink().tape();
    w.set_alignment(crate::ALIGNMENT).unwrap();
    let mut defs = Definitions::new();
    crate::tests::hold(&mut defs, &series_definitions()).unwrap();
    for c in 0..chunks {
        let first = (c * chunk) as u64;
        w.defer_block(&defs, series_summary_block(first, chunk as u64))
            .unwrap();
        let bytes: Vec<u8> = (0..chunk)
            .flat_map(|i| (i as i16).wrapping_mul(31).to_le_bytes())
            .collect();
        let head = ChunkHead {
            kind: BlockKind::Series,
            track_id: series_track_id(),
            track_number: TrackNum::new(1),
            first_item: ItemNum::new(first),
            count: chunk as u32,
            first_tick: first as i64 * 1000,
            last_tick: (first as i64 + chunk as i64 - 1) * 1000,
            latest_end: (first as i64 + chunk as i64 - 1) * 1000,
            time_mode: 0,
            end_mode: EndMode::Absent,
            grid: Some(SeriesGrid::start(0, 0, Magnitude::ONE)),
        };
        let col = ColumnSpec {
            kind: ColumnKind::Samples,
            codec: Codec::RAW,
            param: 0,
            len: bytes.len(),
        };
        w.write_chunk(&defs, &head, &[(col, &bytes)]).unwrap();
    }
    w.close().unwrap();
    let file = tape.to_vec().unwrap();

    let mut rd = FileBlocks::new(&file[..]);
    let mut walked = 0;
    while let Some(b) = rd.next_block() {
        let b = b.unwrap();
        if b.kind == BlockKind::Summary {
            let h = b.summary_head().expect("a head");
            assert_eq!(
                (h.track_id, h.level, h.items),
                (series_track_id(), 0, chunk as u64)
            );
            walked += 1;
        }
    }
    assert_eq!(walked, chunks);

    let r = TapeReader::open(&file[..]).unwrap();
    let listed: Vec<_> = r
        .track_entries(0, series_track_id())
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == BlockKind::Summary)
        .collect();
    assert_eq!(listed.len(), chunks);
    assert!(
        listed
            .iter()
            .all(|e| e.tree == 2 && e.level == 0 && e.count == chunk as u64)
    );
    assert_eq!(
        r.verify(0).map(|v| v.holds).ok(),
        Some(true),
        "the whole piece verifies"
    );

    let e = listed[3];
    let (at, len) = (e.offset as usize, e.len as usize);
    let restamp = |x: &mut [u8]| {
        let crc = crate::CRC.checksum(&x[at..at + len - 4]);
        x[at + len - 4..at + len].copy_from_slice(&crc.to_le_bytes());
    };
    // a node byte, CRC restamped: the block reads, the content hash doesn't hold
    let mut bad = file.clone();
    bad[at + 120] ^= 0x01;
    restamp(&mut bad);
    assert!(
        Block::read(&bad[at..at + len]).is_ok(),
        "the changed block reads"
    );
    let held = TapeReader::open(&bad[..]).and_then(|r| r.verify(0).map(|v| v.holds));
    assert_ne!(held.ok(), Some(true), "a changed node byte doesn't verify");
    // the head's items, CRC restamped
    let mut bad = file.clone();
    bad[at + 64] ^= 0x01;
    restamp(&mut bad);
    assert!(Block::read(&bad[at..at + len]).is_err(), "a lying head");
    let held = TapeReader::open(&bad[..]).and_then(|r| r.verify(0).map(|v| v.holds));
    assert_ne!(held.ok(), Some(true), "a changed head doesn't verify");
    // a node bit, the old CRC
    let mut bad = file.clone();
    bad[at + 130] ^= 0x80;
    let held = TapeReader::open(&bad[..]).and_then(|r| r.verify(0).map(|v| v.holds));
    assert_ne!(held.ok(), Some(true), "a flipped bit doesn't verify");
}
