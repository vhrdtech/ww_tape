use super::*;
use crate::ItemNum;
use shrink_wrap::prelude::*;

/// The session of blocks made by the tests.
pub(crate) const TEST_SESSION: SessionTag = SessionTag::writable(0x7465_7374);

/// Block `seq` of [TEST_SESSION].
pub(crate) fn id(seq: u64) -> BlockId {
    BlockId::new(TEST_SESSION, seq)
}

const C1: ClockNum = ClockNum::new(1);
const C3: ClockNum = ClockNum::new(3);

fn at(clock: ClockNum, ticks: i64) -> ClockTime {
    ClockTime::new(clock, ticks)
}

/// An unverified read skips the body CRC of a records block, not its structure.
#[test]
fn read_unverified_skips_the_crc_not_the_records() {
    let mut buf = [0u8; 256];
    let mut w = BlockWriter::new(&mut buf, BlockKind::Records, id(7), &[C1]).unwrap();
    w.push(TrackNum::new(1), at(C1, 1_000), &[b"abcdef"])
        .unwrap();
    let len = w.finish();
    let good = &buf[..len];
    assert_eq!(Block::read(good).unwrap().checked(), Checked::Verified);
    assert_eq!(
        Block::read_unverified(good).unwrap().checked(),
        Checked::Unverified
    );
    // a byte of the item, before the CRC: only the CRC sees it
    let mut item = good.to_vec();
    item[len - 6] ^= 0x01;
    assert_eq!(Block::read(&item).unwrap_err(), BlockError::BadCrc);
    let b = Block::read_unverified(&item).unwrap();
    assert!(b.is_intact());
    // a record count the records don't match: refused either way
    let mut count = good.to_vec();
    count[12] = 2;
    assert_eq!(Block::read(&count).unwrap_err(), BlockError::BadCrc);
    assert_eq!(
        Block::read_unverified(&count).unwrap_err(),
        BlockError::BadRecords
    );
}

#[test]
fn write_and_read_back() {
    let mut buf = [0u8; 256];
    let mut w = BlockWriter::new(&mut buf, BlockKind::Records, id(7), &[C1]).unwrap();
    w.push(TrackNum::new(1), at(C1, 1_000), &[b"ab"]).unwrap();
    w.push(TrackNum::new(300), at(C1, 900), &[b"c", b"de"])
        .unwrap();
    w.push_with(TrackNum::new(2), at(C1, 5_000), |b| {
        b[..3].copy_from_slice(b"xyz");
        Some(3)
    })
    .unwrap();
    assert_eq!(w.records(), 3);
    let len = w.finish();
    let block = Block::read(&buf[..len]).unwrap();
    assert_eq!(
        (
            block.kind,
            block.seq,
            block.record_count,
            block.clocks().collect::<Vec<_>>()
        ),
        (BlockKind::Records, 7, 3, vec![C1])
    );
    let records: Vec<_> = block.records().map(|r| r.unwrap()).collect();
    assert_eq!(
        records,
        [
            Record {
                track: RecordTrack::from_u32(1),
                at: Some(at(C1, 1_000)),
                item: b"ab"
            },
            Record {
                track: RecordTrack::from_u32(300),
                at: Some(at(C1, 900)),
                item: b"cde"
            },
            Record {
                track: RecordTrack::from_u32(2),
                at: Some(at(C1, 5_000)),
                item: b"xyz"
            },
        ]
    );
    // header + clock table (2) + (1+2+1+2: 1000 is 2 bytes) + (2+2+1+3: -100 is 2 bytes) + (1+2+1+3) + crc
    assert_eq!(len, 32 + 2 + 6 + 8 + 7 + 4);
}

/// Deltas are per clock: records of two clocks interleave, each coded against the previous one on its clock, and
/// the slot costs one bit of the head.
#[test]
fn deltas_per_clock() {
    let mut buf = [0u8; 256];
    let mut w = BlockWriter::new(&mut buf, BlockKind::Records, id(0), &[C1, C3]).unwrap();
    let big = 1_700_000_000_000_000_000i64;
    w.push(TrackNum::new(1), at(C1, 10), &[b"a"]).unwrap();
    w.push(TrackNum::new(2), at(C3, big), &[b"b"]).unwrap();
    w.push(TrackNum::new(1), at(C1, 11), &[b"c"]).unwrap();
    w.push(TrackNum::new(2), at(C3, big + 1), &[b"d"]).unwrap();
    w.push(TrackNum::new(5), at(C1, -4), &[b"e"]).unwrap();
    w.push(TrackNum::new(3), at(C1, i64::MIN), &[]).unwrap();
    w.push(TrackNum::new(3), at(C1, i64::MAX), &[]).unwrap();
    let len = w.finish();
    let block = Block::read(&buf[..len]).unwrap();
    assert_eq!(block.clocks().collect::<Vec<_>>(), [C1, C3]);
    let got: Vec<_> = block
        .records()
        .map(|r| r.unwrap())
        .map(|r| (r.track.get(), r.at.unwrap(), r.item))
        .collect();
    assert_eq!(
        got,
        [
            (1, at(C1, 10), &b"a"[..]),
            (2, at(C3, big), b"b"),
            (1, at(C1, 11), b"c"),
            (2, at(C3, big + 1), b"d"),
            (5, at(C1, -4), b"e"),
            (3, at(C1, i64::MIN), b""),
            (3, at(C1, i64::MAX), b""),
        ]
    );
    // the second record of each clock is a one-byte delta
    let r = &buf[HEADER_LEN + 3..];
    let first_two = 1 + 1 + 2 + 1 + 9 + 2;
    assert_eq!(&r[first_two..first_two + 3], &[1 << 1, 2, 1]);
}

/// Describe records carry no time; a record on a slot the block has no clock for is damage.
#[test]
fn describe_records_have_no_time() {
    let mut buf = [0u8; 64];
    let mut w = BlockWriter::new(&mut buf, BlockKind::Records, id(0), &[]).unwrap();
    w.push_describe(&[b"xy"]).unwrap();
    let len = w.finish();
    assert_eq!(len, HEADER_LEN + 1 + 1 + 1 + 2 + CRC_LEN);
    let block = Block::read(&buf[..len]).unwrap();
    let r = block.records().next().unwrap().unwrap();
    assert_eq!(
        (r.track, r.at, r.item),
        (RecordTrack::Definitions, None, &b"xy"[..])
    );
    // track 1 in a block without clocks
    let mut bad = buf;
    bad[HEADER_LEN + 1] = 1 << 0;
    let crc = CRC.checksum(&bad[..len - CRC_LEN]);
    bad[len - CRC_LEN..len].copy_from_slice(&crc.to_le_bytes());
    assert_eq!(
        Block::read(&bad[..len]).unwrap_err(),
        BlockError::BadRecords
    );
}

#[test]
fn block_writer_refuses_bad_input() {
    let mut buf = [0u8; 64];
    let mut w = BlockWriter::new(&mut buf, BlockKind::Records, id(0), &[C1]).unwrap();
    assert_eq!(
        w.push(TrackNum::new(1), at(C3, 0), &[]),
        Err(PushError::UnknownClock(C3))
    );
    assert_eq!(w.records(), 0, "a refused record writes nothing");
    let start = |buf: &mut [u8], clocks: &[u32]| {
        let clocks: Vec<ClockNum> = clocks.iter().map(|c| ClockNum::new(*c)).collect();
        BlockWriter::new(buf, BlockKind::Records, id(0), &clocks).err()
    };
    assert_eq!(
        start(&mut buf, &[1; MAX_BLOCK_CLOCKS + 1]),
        Some(BlockStartError::TooManyClocks)
    );
    assert_eq!(
        start(&mut buf, &[3, 1]),
        Some(BlockStartError::ClocksUnordered)
    );
    assert_eq!(
        start(&mut buf, &[1, 1]),
        Some(BlockStartError::ClocksUnordered)
    );
    assert_eq!(
        start(&mut buf[..HEADER_LEN], &[C1.get()]),
        Some(BlockStartError::BufferTooSmall {
            needed: HEADER_LEN + 2 + CRC_LEN
        })
    );
}

#[test]
fn an_encoder_claiming_more_than_it_was_given_is_full() {
    let mut buf = [0u8; 64];
    let mut w = BlockWriter::new(&mut buf, BlockKind::Records, id(0), &[C1]).unwrap();
    let free = w.free();
    assert_eq!(
        w.push_with(TrackNum::new(1), at(C1, 0), |space| Some(space.len() + 1)),
        Err(PushError::Full)
    );
    assert_eq!(w.push_describe_with(|_| Some(usize::MAX)), Err(Full));
    assert_eq!((w.records(), w.free()), (0, free), "nothing kept");
    w.push(TrackNum::new(1), at(C1, 5), &[b"ok"]).unwrap();
    let len = w.finish();
    let b = Block::read(&buf[..len]).unwrap();
    assert_eq!(b.records().count(), 1);
}

#[test]
fn tape_writer_refuses_bad_settings() {
    let mut w = TapeWriter::create_on(crate::MemSink::new(), SyncPolicy::None);
    assert!(matches!(
        w.set_alignment(512),
        Err(WriteError::Input(InputError::Alignment(512)))
    ));
    w.set_alignment(ALIGNMENT).unwrap();
    assert!(matches!(
        w.set_session(SessionTag::NONE),
        Err(WriteError::Input(InputError::Session(_)))
    ));
    assert!(matches!(
        w.write_block(&Definitions::new(), &[0; 8]),
        Err(WriteError::Block(BlockError::Short { .. }))
    ));
}

#[test]
fn full_leaves_the_block_as_it_was() {
    let mut buf = [0u8; HEADER_LEN + 2 + CRC_LEN + 6];
    let mut w = BlockWriter::new(&mut buf, BlockKind::Records, id(0), &[C1]).unwrap();
    w.push(TrackNum::new(1), at(C1, 0), &[b"abc"]).unwrap();
    assert_eq!(
        w.push(TrackNum::new(1), at(C1, 0), &[b"ab"]),
        Err(PushError::Full)
    );
    assert_eq!(
        w.push_with(TrackNum::new(1), at(C1, 0), |_| None),
        Err(PushError::Full)
    );
    let len = w.finish();
    assert_eq!(Block::read(&buf[..len]).unwrap().record_count, 1);
}

#[test]
fn push_with_big_item_moves_down() {
    let mut buf = vec![0u8; 4096];
    let mut w = BlockWriter::new(&mut buf, BlockKind::Records, id(0), &[C1]).unwrap();
    let item: Vec<u8> = (0..200).map(|i| i as u8).collect();
    w.push_with(TrackNum::new(1), at(C1, 10), |b| {
        b[..200].copy_from_slice(&item);
        Some(200)
    })
    .unwrap();
    let len = w.finish();
    let block = Block::read(&buf[..len]).unwrap();
    let r = block.records().next().unwrap().unwrap();
    assert_eq!(r.item, &item[..]);
    assert_eq!(len, HEADER_LEN + 2 + 1 + 1 + 2 + 200 + CRC_LEN);
}

#[test]
fn corrupt_blocks_are_refused() {
    let mut buf = [0u8; 64];
    let mut w = BlockWriter::new(&mut buf, BlockKind::Records, id(0), &[]).unwrap();
    w.push_describe(&[b"x"]).unwrap();
    let len = w.finish();
    let mut bad = buf;
    bad[HEADER_LEN + 3] ^= 1;
    assert_eq!(Block::read(&bad[..len]).unwrap_err(), BlockError::BadCrc);
    assert_eq!(
        Block::read(&buf[..len - 1]).unwrap_err(),
        BlockError::Short { needed: len }
    );
    let mut bad = buf;
    bad[0] = b'X';
    assert_eq!(Block::read(&bad[..len]).unwrap_err(), BlockError::BadMagic);
}

const HOST: ClockNum = ClockNum::new(1);
const WALL: ClockNum = ClockNum::new(2);
/// The host counter's next epoch.
const EPOCH: ClockNum = ClockNum::new(3);

/// The epoch link: tick 0 of [EPOCH] is tick 4e18 of [HOST].
const EPOCH_START: [MapSegment; 1] = [MapSegment::offset(0, 4_000_000_000_000_000_000)];

const SEGMENTS: [MapSegment; 2] = [
    MapSegment {
        a0: 0,
        a1: Some(1_000_000),
        b0: 1_763_164_800_000_000_000,
        slope: Ratio64::ONE,
        error: 500,
    },
    MapSegment {
        a0: 1_000_000,
        a1: None,
        b0: 1_763_164_800_001_000_020,
        slope: Ratio64::new(1_000_000_007, 999_999_931).unwrap(),
        error: 800,
    },
];

/// Name path of the sample track: `link[{3}].tx_frames`, `link` keyed, its key one byte.
const NAME_PATH: [Segment<'static>; 3] = [
    Segment::Name("link"),
    Segment::Key(&[3]),
    Segment::Name("tx_frames"),
];

/// One record of every kind: a producer, a bundle, a host clock and the wall clock with a map between them, a track
/// with dimensions, a key and an item type, and a typed item.
fn sample_describe() -> Vec<Describe<'static>> {
    const PATH: [UNib32; 3] = [UNib32(1), UNib32(3), UNib32(0)];
    const KEYS: [KeySegment<'static>; 1] = [KeySegment {
        at: UNib32(1),
        bytes: RefVec::Slice { slice: &[3] },
    }];
    const DIMS: [Dimension<'static>; 2] = [
        Dimension {
            name: "input",
            value: "cam1",
        },
        Dimension {
            name: "producer",
            value: "vad",
        },
    ];
    let item_type = TypeRef {
        bundle: RefVec::Slice {
            slice: &[0xaa, 0xbb],
        },
        krate: FullVersion::new("eventfull", Version::new(0, 4, 0)),
        path: "run::RunRecord",
    };
    vec![
        Describe::Item(TypedItem {
            ty: item_type.clone(),
            value: RefVec::Slice { slice: &[1, 2, 3] },
        }),
        Describe::Track(TrackDef {
            number: TrackNum::new(4),
            key: track_key(api_root_id("ww_tap"), &NAME_PATH),
            clock: HOST,
            dimensions: RefVec::Slice { slice: &DIMS },
            items: crate::Items::WireWeaver {
                api: FullVersion::new("ww_tap", Version::new(0, 1, 0)),
                bundle: RefVec::Slice { slice: &[1, 2] },
                path: RefVec::Slice { slice: &PATH },
                keys: RefVec::Slice { slice: &KEYS },
                item: Some(item_type),
            },
            label: "link[3].tx.frames",
            repeat: crate::TrackRepeat::STATE,
            kind: TrackKind::Events(EventsDef::INSTANTS),
            relations: RefVec::Slice { slice: &[] },
            owner: [0; 32],
        }),
        Describe::Map(ClockMap {
            from: HOST,
            to: WALL,
            segments: RefVec::Slice { slice: &SEGMENTS },
        }),
        Describe::Clock(ClockDef {
            number: WALL,
            key: UNIX_CLOCK_KEY,
            rate: Magnitude::pow10(9).into(),
            resolution: Magnitude::pow10(-6).into(),
            origin: ClockOrigin::UnixEpoch,
            label: "wall",
        }),
        Describe::Clock(ClockDef {
            number: HOST,
            key: clock_key(api_root_id("host"), &[], 17),
            rate: Magnitude::pow10(9).into(),
            resolution: Magnitude::ratio(1, 32768).into(),
            origin: ClockOrigin::Boot,
            label: "host",
        }),
        Describe::Bundle {
            hash: RefVec::Slice { slice: &[1, 2] },
            bytes: RefVec::Slice { slice: &[9; 40] },
        },
        Describe::Producer(Producer { name: "test" }),
        Describe::Map(ClockMap {
            from: EPOCH,
            to: HOST,
            segments: RefVec::Slice {
                slice: &EPOCH_START,
            },
        }),
        Describe::Clock(ClockDef {
            number: EPOCH,
            key: ClockKey::new(clock_key(api_root_id("host"), &[], 17).get() ^ 1),
            rate: Magnitude::pow10(9).into(),
            resolution: Magnitude::ratio(1, 32768).into(),
            origin: ClockOrigin::Epoch,
            label: "host epoch 1",
        }),
    ]
}

#[test]
fn describe_records_round_trip() {
    for d in sample_describe() {
        let mut scratch = [0u8; 256];
        let bytes = d.to_ww_bytes(&mut scratch).unwrap();
        let back = Describe::from_ww_bytes(bytes).unwrap();
        assert_eq!(back, d);
        let owned = DescribeOwned::from_ww_bytes_owned(bytes).unwrap();
        assert_eq!(owned.to_ww_bytes_owned().unwrap(), bytes);
    }
}

#[test]
fn map_segments() {
    use Rounding::{Ceil, Floor, NearestEven};
    let [exact, fitted] = SEGMENTS;
    assert_eq!(exact.map(-1, Floor), None);
    assert_eq!(exact.map(10, Floor), Some(1_763_164_800_000_000_010));
    assert_eq!(exact.map(1_000_000, Floor), None);
    assert_eq!(fitted.map(1_000_000, Floor), Some(fitted.b0));
    // 999_999_931 ticks of a 1_000_000_007 / 999_999_931 slope are 1_000_000_007
    let whole = 1_000_000 + 999_999_931;
    for r in [Floor, Ceil, NearestEven] {
        assert_eq!(fitted.map(whole, r), Some(fitted.b0 + 1_000_000_007));
    }
    // 10^9 ticks are 1000000076.000005776..: the rounding is the caller's
    let a = 1_000_000 + 1_000_000_000;
    assert_eq!(fitted.map(a, Floor), Some(fitted.b0 + 1_000_000_076));
    assert_eq!(fitted.map(a, Ceil), Some(fitted.b0 + 1_000_000_077));
    assert_eq!(fitted.map(a, NearestEven), Some(fitted.b0 + 1_000_000_076));
    // a result past i64 is refused
    assert_eq!(MapSegment::offset(0, i64::MAX).map(1, Floor), None);
    assert_eq!(MapSegment::new(5, Some(5), 0, Ratio64::ONE, 0), None);

    // the same bytes with plain parts: a slope that isn't reduced, a zero part or an empty range is refused
    #[derive_shrink_wrap(borrowed, derive(Debug))]
    struct Raw {
        a0: i64,
        a1: Option<i64>,
        b0: i64,
        num: UVlq64,
        den: UVlq64,
        error: u64,
    }
    let raw = |a1, num, den| {
        let mut scratch = [0u8; 64];
        let r = Raw {
            a0: 5,
            a1,
            b0: 7,
            num: UVlq64(num),
            den: UVlq64(den),
            error: 3,
        };
        r.to_ww_bytes(&mut scratch).unwrap().to_vec()
    };
    let good = MapSegment::from_ww_bytes(&raw(Some(9), 3, 2)).unwrap();
    assert_eq!(
        good,
        MapSegment::new(5, Some(9), 7, Ratio64::new(3, 2).unwrap(), 3).unwrap()
    );
    for (a1, num, den) in [
        (Some(9), 2, 2),
        (Some(9), 0, 1),
        (Some(9), 1, 0),
        (Some(5), 1, 1),
    ] {
        assert!(
            MapSegment::from_ww_bytes(&raw(a1, num, den)).is_err(),
            "{a1:?} {num}/{den}"
        );
    }
}

#[test]
fn track_key_segments() {
    let root = api_root_id("ww_tap");
    let key = |segments: &[Segment<'_>]| track_key(root, segments);
    // tag and length keep the segments apart
    assert_ne!(
        key(&[Segment::Name("a"), Segment::Name("b")]),
        key(&[Segment::Name("ab")])
    );
    assert_ne!(key(&[Segment::Name("\u{3}")]), key(&[Segment::Key(&[3])]));
    assert_ne!(
        key(&[Segment::Index(3)]),
        key(&[Segment::Key(&[3, 0, 0, 0])])
    );
    assert_ne!(key(&[]), track_key(api_root_id("ww_tapx"), &[]));
    // a const fn: a firmware writer's keys are constants
    const LINK_RX: TrackKey = track_key(
        api_root_id("ww_tap"),
        &[
            Segment::Name("link"),
            Segment::Index(0),
            Segment::Name("rx_frames"),
        ],
    );
    assert_eq!(
        LINK_RX,
        key(&[
            Segment::Name("link"),
            Segment::Index(0),
            Segment::Name("rx_frames")
        ])
    );
    // the hash is part of the format: recordings keep their ids
    assert_eq!(api_root_id("ww_tap"), 0x18cd_d9fe_1470_3049);
    assert_eq!(key(&NAME_PATH).get(), 0xd060_2e4e_94a3_be63);
}

#[test]
fn track_identity() {
    let key = track_key(api_root_id("ww_tap"), &NAME_PATH);
    assert_eq!(track_id(key, []), key.id());
    let d = |name, value| Dimension { name, value };
    let a = track_id(key, [d("input", "cam1")]);
    assert_ne!(a, key.id());
    assert_ne!(a, track_id(key, [d("input", "cam2")]));
    // the separator keeps `ab=c` and `a=bc` apart
    assert_ne!(track_id(key, [d("ab", "c")]), track_id(key, [d("a", "bc")]));
    let Describe::Track(t) = &sample_describe()[1] else {
        unreachable!("the second sample is a track")
    };
    assert!(t.dimensions_canonical());
    assert_eq!(
        t.id(),
        track_id(key, [d("input", "cam1"), d("producer", "vad")])
    );
    let unsorted = [d("producer", "vad"), d("input", "cam1")];
    let t = TrackDef {
        dimensions: RefVec::Slice { slice: &unsorted },
        ..t.clone()
    };
    assert!(!t.dimensions_canonical());
    assert_eq!(
        open::check_definition(&Describe::Track(t)),
        Err(DescribeError::Dimensions {
            track: TrackNum::new(4)
        })
    );
}

/// `records` held as one batch (the producer, layout and typed items passed over) and written as blocks of
/// definitions from seq 0: the definitions held and the blocks.
fn defined(records: impl IntoIterator<Item = Describe<'static>>) -> (Definitions, Vec<Vec<u8>>) {
    let records: Vec<_> = records.into_iter().collect();
    let items: Vec<Vec<u8>> = records.iter().map(|d| open::encode(d).unwrap()).collect();
    let mut table = Definitions::new();
    relay(&mut table, &records).unwrap();
    let refs: Vec<RecordRef<'_>> = items
        .iter()
        .map(|item| RecordRef::definition(item))
        .collect();
    let mut blocks = vec![];
    let mut seq = 0;
    open::write_records(
        &refs,
        4096,
        0,
        || {
            seq += 1;
            id(seq - 1)
        },
        |b| {
            blocks.push(b.to_vec());
            Ok::<(), DescribeError>(())
        },
    )
    .unwrap();
    (table, blocks)
}

fn open_of(records: impl IntoIterator<Item = Describe<'static>>) -> Definitions {
    defined(records).0
}

/// The records of `records` a [Definitions] holds: not the producer, the layout or a typed item.
pub(crate) fn held(records: &[Describe<'static>]) -> Vec<Describe<'static>> {
    records
        .iter()
        .filter(|d| open::is_held(d))
        .cloned()
        .collect()
}

/// Hold `batch` in `defs` as a relay holds another producer's numbered definitions: written as one block of
/// definitions and taken with [Definitions::take_block], each track and clock under the number `batch` gives it.
///
/// The tests' tracks often share one identity under several numbers, which only a producer's own blocks can carry.
pub(crate) fn relay(
    defs: &mut Definitions,
    batch: &[Describe<'_>],
) -> Result<usize, DescribeError> {
    let items = batch
        .iter()
        .map(open::encode)
        .collect::<Result<Vec<_>, _>>()?;
    let refs: Vec<RecordRef<'_>> = items
        .iter()
        .map(|item| RecordRef::definition(item))
        .collect();
    let mut blocks = vec![];
    open::write_records(
        &refs,
        MAX_BLOCK_LEN,
        0,
        || id(0),
        |b| {
            blocks.push(b.to_vec());
            Ok::<(), DescribeError>(())
        },
    )?;
    assert!(blocks.len() <= 1, "the batch fits one block");
    match blocks.first() {
        Some(b) => defs.take_block(&Block::read(b).expect("a block just written reads")),
        None => Ok(0),
    }
}

/// Hold `batch` in `defs` as a producer defines it: each clock through [Definitions::define_clock_with_maps], an
/// epoch clock with the maps from it in the batch, then the other definitions, then each track through
/// [Definitions::define_track]. Each number given must be the one written in `batch`.
pub(crate) fn hold(defs: &mut Definitions, batch: &[Describe<'_>]) -> Result<(), DescribeError> {
    let maps_from = |from| -> Vec<(ClockNum, &[MapSegment])> {
        batch
            .iter()
            .filter_map(|d| match d {
                Describe::Map(m) if m.from == from => match &m.segments {
                    RefVec::Slice { slice } => Some((m.to, *slice)),
                    RefVec::Buf { .. } => None,
                },
                _ => None,
            })
            .collect()
    };
    let epochs: Vec<ClockNum> = batch
        .iter()
        .filter_map(|d| match d {
            Describe::Clock(c) if c.origin == ClockOrigin::Epoch => Some(c.number),
            _ => None,
        })
        .collect();
    for d in batch {
        if let Describe::Clock(c) = d {
            let maps = if epochs.contains(&c.number) {
                maps_from(c.number)
            } else {
                Vec::new()
            };
            let given = defs.define_clock_with_maps(c.spec(), &maps)?;
            assert_eq!(given, c.number, "clock {} is defined as {given}", c.number);
        }
    }
    let rest: Vec<Describe<'_>> = batch
        .iter()
        .filter(|d| match d {
            Describe::Clock(_) | Describe::Track(_) => false,
            Describe::Map(m) => !epochs.contains(&m.from),
            _ => true,
        })
        .cloned()
        .collect();
    defs.define_all(&rest)?;
    for d in batch {
        if let Describe::Track(t) = d {
            let given = defs.define_track(t.spec())?;
            assert_eq!(given, t.number, "track {} is defined as {given}", t.number);
        }
    }
    Ok(())
}

/// A definition is held in its slot whatever order the batch came in, and replaces the one there; a typed item, the
/// producer and the layout are not held.
#[test]
fn definitions_open_by_slot() {
    let forward = open_of(sample_describe());
    let backward = open_of(sample_describe().into_iter().rev());
    assert_eq!(forward, backward);
    let slots: Vec<Slot> = forward.definitions().map(|(s, _)| *s).collect();
    assert_eq!(
        slots
            .iter()
            .filter(|s| matches!(s, Slot::Clock { .. }))
            .count(),
        3
    );
    assert!(!slots.iter().any(|s| matches!(
        s,
        Slot::Own {
            kind: DefinitionKind::Item,
            ..
        }
    )));
    assert!(!slots.contains(&Slot::Producer));
    let mut replaced = forward.clone();
    let other = sample_describe()
        .into_iter()
        .find_map(|d| match d {
            Describe::Clock(c) if c.number == HOST => Some(ClockSpec {
                label: "other",
                ..c.spec()
            }),
            _ => None,
        })
        .unwrap();
    assert_eq!(replaced.define_clock(other), Ok(HOST));
    assert_eq!(replaced.len(), forward.len());
    let host = Slot::Clock { number: HOST };
    assert_ne!(replaced.get(&host), forward.get(&host));
    assert_eq!(
        replaced.define(&Describe::Producer(Producer { name: "p" })),
        Err(DescribeError::WriterOwned)
    );
}

/// A track or map names an open clock, in the batch or open already; definitions that break this are refused when
/// they are made (writing side point 1).
#[test]
fn definitions_need_their_clocks() {
    let no_wall: Vec<_> = sample_describe()
        .into_iter()
        .filter(|d| !matches!(d, Describe::Clock(c) if c.number == WALL))
        .collect();
    let unknown = Err(DescribeError::UnknownClock { clock: WALL });
    assert_eq!(check_defs(&Definitions::new(), &held(&no_wall)), unknown);
    // one at a time, a track before its clock is refused too
    let track = no_wall
        .iter()
        .find(|d| matches!(d, Describe::Track(_)))
        .unwrap();
    assert!(matches!(
        check_defs(&Definitions::new(), [track]),
        Err(DescribeError::UnknownClock { .. })
    ));
}

/// A record block of `records` on their clocks.
fn records_block(seq: BlockId, records: &[(u32, ClockTime, &[u8])]) -> Vec<u8> {
    let mut clocks: Vec<ClockNum> = records.iter().map(|r| r.1.clock).collect();
    clocks.sort();
    clocks.dedup();
    let mut buf = [0u8; 1024];
    let mut w = BlockWriter::new(&mut buf, BlockKind::Records, seq, &clocks).unwrap();
    for (track, at, item) in records {
        w.push(TrackNum::new(*track), *at, &[item]).unwrap();
    }
    let len = w.finish();
    buf[..len].to_vec()
}

/// A record's clock is its track's (T11): a record on another clock, or on a track not open, is refused; a state
/// track's items are put, never in a caller's block.
#[test]
fn records_are_on_their_tracks_clock() {
    let table = open_of(sample_with_wall_track());
    let check = |track, clock| {
        let b = records_block(id(0), &[(track, at(clock, 0), b"s")]);
        table.check_block(&Block::read(&b).unwrap())
    };
    assert_eq!(check(10, HOST), Ok(()));
    assert_eq!(
        check(10, WALL),
        Err(DescribeError::WrongClock {
            track: TrackNum::new(10),
            clock: WALL,
            track_clock: HOST
        })
    );
    assert_eq!(
        check(10, ClockNum::new(9)),
        Err(DescribeError::WrongClock {
            track: TrackNum::new(10),
            clock: ClockNum::new(9),
            track_clock: HOST
        })
    );
    assert_eq!(
        check(5, HOST),
        Err(DescribeError::UnknownTrack {
            track: TrackNum::new(5)
        })
    );
    assert_eq!(
        check(4, HOST),
        Err(DescribeError::StateRecord {
            track: TrackNum::new(4)
        })
    );
}

/// A caller's block holds no definitions: they are held in [Definitions] and put by the writer.
#[test]
fn definitions_are_not_in_a_callers_block() {
    let table = open_of(sample_with_wall_track());
    let (_, blocks) = defined([Describe::Producer(Producer { name: "p" })]);
    let item = Block::read(&blocks[0])
        .unwrap()
        .records()
        .next()
        .unwrap()
        .unwrap()
        .item
        .to_vec();
    let mut buf = [0u8; 256];
    let mut w = BlockWriter::new(&mut buf, BlockKind::Records, id(0), &[HOST]).unwrap();
    w.push_describe(&[&item]).unwrap();
    w.push(TrackNum::new(10), at(HOST, 0), &[b"s"]).unwrap();
    let len = w.finish();
    assert_eq!(
        table.check_block(&Block::read(&buf[..len]).unwrap()),
        Err(DescribeError::DefinitionBlock)
    );
}

fn map(from: ClockNum, to: ClockNum, segments: &'static [MapSegment]) -> Describe<'static> {
    Describe::Map(ClockMap {
        from,
        to,
        segments: RefVec::Slice { slice: segments },
    })
}

/// A map is strictly increasing over all its segments: refused on insert and on decode when a segment overlaps the
/// one before, follows one without an end, or starts at or below its last value.
#[test]
fn maps_increase_across_segments() {
    let fitted = SEGMENTS[1];
    assert!(segments_increasing(SEGMENTS));
    let last = SEGMENTS[0].last().unwrap();
    assert_eq!(last, SEGMENTS[0].b0 + 999_999);
    static AT_LAST: [MapSegment; 2] = [
        SEGMENTS[0],
        MapSegment {
            b0: 1_763_164_800_000_999_999,
            ..SEGMENTS[1]
        },
    ];
    static OVERLAP: [MapSegment; 2] = [
        SEGMENTS[0],
        MapSegment {
            a0: 999_999,
            ..SEGMENTS[1]
        },
    ];
    static AFTER_OPEN: [MapSegment; 2] = [SEGMENTS[1], SEGMENTS[1]];
    let check = open::check_definition;
    assert_eq!(check(&map(HOST, WALL, &SEGMENTS)), Ok(()));
    for bad in [&AT_LAST[..], &OVERLAP, &AFTER_OPEN] {
        let d = map(HOST, WALL, bad);
        assert_eq!(
            check(&d),
            Err(DescribeError::MapNotIncreasing {
                from: HOST,
                to: WALL
            })
        );
        let mut scratch = [0u8; 256];
        let bytes = d.to_ww_bytes(&mut scratch).unwrap();
        assert!(Describe::from_ww_bytes(bytes).is_err());
        assert!(DescribeOwned::from_ww_bytes_owned(bytes).is_err());
    }
    // one above the last value is fine
    static ABOVE: [MapSegment; 2] = [
        SEGMENTS[0],
        MapSegment {
            b0: 1_763_164_800_001_000_000,
            ..SEGMENTS[1]
        },
    ];
    assert_eq!(check(&map(HOST, WALL, &ABOVE)), Ok(()));
    assert_eq!(fitted.last(), None);
}

/// An epoch clock needs its epoch link: a map from it to the previous epoch's clock, one exact offset segment from
/// tick 0 to the end with `b0 > 0`. The reverse direction, a fitted segment, a bounded one or a second one don't do.
#[test]
fn epoch_clocks_need_their_link() {
    let without: Vec<_> = held(&sample_describe())
        .into_iter()
        .filter(|d| !matches!(d, Describe::Map(m) if m.from == EPOCH && m.to == HOST))
        .collect();
    let empty = Definitions::new();
    let unmapped = Err(DescribeError::EpochUnmapped { clock: EPOCH });
    assert_eq!(check_defs(&empty, &without), unmapped);
    let seg = MapSegment::offset(0, 4_000);
    static REVERSED: [MapSegment; 1] = [MapSegment::offset(4_000, 0)];
    static FITTED: [MapSegment; 1] = [MapSegment {
        error: 5,
        ..MapSegment::offset(0, 4_000)
    }];
    static SLOPED: [MapSegment; 1] = [MapSegment {
        slope: Ratio64::new(1001, 1000).unwrap(),
        ..MapSegment::offset(0, 4_000)
    }];
    static LATE: [MapSegment; 1] = [MapSegment::offset(1, 4_000)];
    static AT_ZERO: [MapSegment; 1] = [MapSegment::offset(0, 0)];
    static BOUNDED: [MapSegment; 1] = [MapSegment {
        a1: Some(10),
        ..MapSegment::offset(0, 4_000)
    }];
    static TWO: [MapSegment; 2] = [
        MapSegment {
            a1: Some(10),
            ..MapSegment::offset(0, 4_000)
        },
        MapSegment::offset(10, 5_000),
    ];
    for (from, to, segments) in [
        (HOST, EPOCH, &REVERSED[..]),
        (EPOCH, HOST, &FITTED),
        (EPOCH, HOST, &SLOPED),
        (EPOCH, HOST, &LATE),
        (EPOCH, HOST, &AT_ZERO),
        (EPOCH, HOST, &BOUNDED),
        (EPOCH, HOST, &TWO),
        (EPOCH, HOST, &[]),
    ] {
        let link = map(from, to, segments);
        assert_eq!(
            check_defs(&empty, without.iter().chain([&link])),
            unmapped,
            "{from} -> {to}: {segments:?}"
        );
    }
    let link = map(EPOCH, HOST, &EPOCH_START);
    assert_eq!(check_defs(&empty, without.iter().chain([&link])), Ok(()));
    // a link already open does
    let linked = open_of(sample_describe());
    let epoch = sample_describe()
        .into_iter()
        .find(|d| matches!(d, Describe::Clock(c) if c.number == EPOCH))
        .unwrap();
    assert_eq!(check_defs(&linked, [&epoch]), Ok(()));
    assert!(seg.is_exact_offset());
}

/// A clock's resolution is at least one of its ticks: `resolution · rate >= 1`.
#[test]
fn clock_resolution_at_least_a_tick() {
    let clock = |rate: Magnitude, resolution: Magnitude| {
        Describe::Clock(ClockDef {
            number: ClockNum::new(1),
            key: ClockKey::new(1),
            rate: rate.into(),
            resolution: resolution.into(),
            origin: ClockOrigin::Free,
            label: "c",
        })
    };
    let ns = Magnitude::pow10(9);
    // exactly one tick, more than one, a 32 kHz counter read in ns, an NTSC frame clock
    for (rate, resolution) in [
        (ns, Magnitude::pow10(-9)),
        (ns, Magnitude::pow10(-6)),
        (ns, Magnitude::ratio(1, 32768)),
        (
            Magnitude::ratio(60_000, 1001),
            Magnitude::ratio(1001, 60_000),
        ),
    ] {
        assert_eq!(
            open::check_definition(&clock(rate, resolution)),
            Ok(()),
            "{rate} {resolution}"
        );
    }
    for (rate, resolution) in [
        (ns, Magnitude::pow10(-10)),
        (
            Magnitude::ratio(60_000, 1001),
            Magnitude::ratio(1000, 60_000),
        ),
        (Magnitude::pow10(3), Magnitude::new(1, 4000, 0, 0, 1)),
    ] {
        assert_eq!(
            open::check_definition(&clock(rate, resolution)),
            Err(DescribeError::Resolution {
                clock: ClockNum::new(1)
            }),
            "{rate} {resolution}"
        );
    }
}

/// Records are written as blocks of at most the length asked, a longer record in a block of its own, all with the
/// flags and ids given; they read back in order.
#[test]
fn records_as_blocks() {
    let items: Vec<Vec<u8>> = sample_describe()
        .iter()
        .map(|d| open::encode(d).unwrap())
        .collect();
    let mut want: Vec<(u32, Option<ClockTime>, Vec<u8>)> =
        items.iter().map(|i| (0, None, i.clone())).collect();
    want.push((4, Some(at(HOST, -3)), b"session".to_vec()));
    want.push((9, Some(at(WALL, 5)), b"late".to_vec()));
    let refs: Vec<RecordRef<'_>> = want
        .iter()
        .map(|(track, at, item)| RecordRef {
            track: RecordTrack::from_u32(*track),
            at: *at,
            item,
        })
        .collect();
    for block_len_max in [64, 4096] {
        let mut blocks = vec![];
        open::write_records(
            &refs,
            block_len_max,
            FLAG_REPEAT,
            || id(10),
            |b| {
                blocks.push(b.to_vec());
                Ok::<(), DescribeError>(())
            },
        )
        .unwrap();
        assert_eq!(blocks.len() > 1, block_len_max == 64);
        let mut got = vec![];
        for b in &blocks {
            let b = Block::read(b).unwrap();
            assert_eq!(
                (b.kind, b.seq, b.is_repeat()),
                (BlockKind::Records, 10, true)
            );
            for r in b.records() {
                let r = r.unwrap();
                got.push((r.track.get(), r.at, r.item.to_vec()));
            }
        }
        assert_eq!(got, want);
    }
}

#[test]
fn resync_skips_bad_and_reports_cut() {
    let mut file = Vec::new();
    for seq in 0..3u64 {
        let mut buf = [0u8; 64];
        let mut w = BlockWriter::new(&mut buf, BlockKind::Records, id(seq), &[C1]).unwrap();
        w.push(TrackNum::new(1), at(C1, seq as i64), &[&[seq as u8; 5]])
            .unwrap();
        let len = w.finish();
        file.extend_from_slice(&buf[..len]);
    }
    let one = file.len() / 3;
    // damage the middle block, cut the last one
    file[one + HEADER_LEN + 2] ^= 0xff;
    file.truncate(file.len() - 3);
    let mut rd = FileBlocks::with_recovery(&file[..], Recovery::Resync);
    assert_eq!(rd.next_block().unwrap().unwrap().seq, 0);
    match rd.next_block().unwrap() {
        Err(FileError::Bad { offset, error, .. }) => {
            assert_eq!((offset, error), (one as u64, BlockError::BadCrc))
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        rd.next_block().unwrap(),
        Err(FileError::Truncated { .. })
    ));
    assert!(rd.next_block().is_none());
}

/// A later version's block and a kind this reader doesn't know are checked, skipped by their length and given out
/// without records, not taken for damage; a describe variant it doesn't know reads as `Unknown`.
#[test]
fn unknown_versions_kinds_and_describe_variants_are_skipped() {
    let mut file = Vec::new();
    for (seq, version, kind) in [
        (0u64, VERSION, 1u8),
        (1, VERSION + 1, 1),
        (2, VERSION, 200),
        (3, VERSION, 1),
        // parity: reserved, not read
        (4, VERSION, 5),
    ] {
        let mut buf = [0u8; 64];
        let mut w = BlockWriter::new(&mut buf, BlockKind::Records, id(seq), &[C1]).unwrap();
        w.push(TrackNum::new(1), at(C1, seq as i64), &[&[seq as u8; 5]])
            .unwrap();
        let len = w.finish();
        buf[4] = version;
        buf[5] = kind;
        // a later version's body: not records
        if version != VERSION {
            buf[HEADER_LEN] = 0xff;
        }
        let crc = CRC.checksum(&buf[..len - CRC_LEN]);
        buf[len - CRC_LEN..len].copy_from_slice(&crc.to_le_bytes());
        file.extend_from_slice(&buf[..len]);
    }
    let mut rd = FileBlocks::new(&file[..]);
    let mut seen = vec![];
    while let Some(b) = rd.next_block() {
        let b = b.unwrap();
        seen.push((b.seq, b.body_known(), b.records().count()));
    }
    assert_eq!(
        seen,
        [
            (0, true, 1),
            (1, false, 0),
            (2, false, 0),
            (3, true, 1),
            (4, false, 0)
        ]
    );
    assert_eq!(BlockKind::from_u8(200), BlockKind::Other(200));

    // a variant appended later, as a newer producer writes it
    #[derive_shrink_wrap(derive(Debug), ww_repr = u4)]
    enum NewerDescribe<'i> {
        Producer(Producer<'i>),
        Track(TrackDef<'i>),
        Bundle {
            hash: RefVec<'i, u8>,
            bytes: RefVec<'i, u8>,
        },
        Clock(ClockDef<'i>),
        Map(ClockMap<'i>),
        Item(TypedItem<'i>),
        Layout(Layout),
        Repeat {
            records: UNib32,
        },
        Index {
            first_seq: u64,
        },
    }
    let mut scratch = [0u8; 32];
    let bytes = NewerDescribe::Index { first_seq: 7 }
        .to_ww_bytes(&mut scratch)
        .unwrap();
    assert!(matches!(
        Describe::from_ww_bytes(bytes),
        Ok(Describe::Unknown(8))
    ));
}

/// `n` data blocks of different sizes back to back, and where each ends.
fn tape(n: u64) -> (Vec<u8>, Vec<usize>) {
    let mut file = Vec::new();
    let mut ends = vec![];
    for seq in 0..n {
        let mut buf = [0u8; 128];
        let mut w = BlockWriter::new(&mut buf, BlockKind::Records, id(seq), &[C1]).unwrap();
        for i in 0..=seq % 3 {
            w.push(
                TrackNum::new(1 + i as u32),
                at(C1, (seq * 10 + i) as i64),
                &[&[seq as u8; 7][..(seq as usize % 7) + 1]],
            )
            .unwrap();
        }
        let len = w.finish();
        file.extend_from_slice(&buf[..len]);
        ends.push(file.len());
    }
    (file, ends)
}

/// Clock `clock` and events tracks `tracks` on it, all streams: what test records, chunks and summaries need.
pub(crate) fn test_definitions(clock: ClockNum, tracks: &[u32]) -> Vec<Describe<'static>> {
    let Describe::Track(t) = &sample_describe()[1] else {
        unreachable!("the second sample is a track")
    };
    let mut out = vec![Describe::Clock(ClockDef {
        number: clock,
        key: ClockKey::new(u64::from(clock.get())),
        rate: Magnitude::pow10(9).into(),
        resolution: Magnitude::pow10(-9).into(),
        origin: ClockOrigin::Free,
        label: "test",
    })];
    for number in tracks {
        out.push(Describe::Track(TrackDef {
            number: TrackNum::new(*number),
            clock,
            repeat: crate::TrackRepeat::Stream,
            ..t.clone()
        }));
    }
    out
}

/// The id of every track of [test_definitions] and [test_series_track]: what their chunks' heads carry.
pub(crate) fn test_track_id() -> TrackId {
    let Describe::Track(t) = &sample_describe()[1] else {
        unreachable!("the second sample is a track")
    };
    t.id()
}

/// Series track `number` on `clock`: i16 samples on `channels` channels, one per `period` ticks.
pub(crate) fn test_series_track(
    clock: ClockNum,
    number: TrackNum,
    channels: u32,
    period: crate::Ratio64,
) -> Describe<'static> {
    let Describe::Track(t) = &sample_describe()[1] else {
        unreachable!("the second sample is a track")
    };
    Describe::Track(TrackDef {
        number,
        clock,
        items: crate::Items::numbers::<i16>(None),
        repeat: crate::TrackRepeat::Stream,
        kind: crate::TrackKind::Series(crate::GridDef::of::<i16>(channels, period)),
        ..t.clone()
    })
}

/// The definitions [tape]'s records need (clock C1, tracks 1 to 3) and its `n` blocks: what a writer takes.
fn defined_tape(n: u64) -> (Definitions, Vec<u8>, Vec<usize>) {
    let mut defs = Definitions::new();
    relay(&mut defs, &test_definitions(C1, &[1, 2, 3])).unwrap();
    let (data, ends) = tape(n);
    (defs, data, ends)
}

/// Seqs of the blocks a reader gives, and whether it reported a cut or damage.
fn read_all(bytes: &[u8], recovery: Recovery) -> (Vec<u64>, usize) {
    let mut rd = FileBlocks::with_recovery(bytes, recovery);
    let (mut seqs, mut errors) = (vec![], 0);
    while let Some(b) = rd.next_block() {
        match b {
            Ok(b) => seqs.push(b.seq),
            Err(_) => errors += 1,
        }
        assert!(errors < 10_000, "the walk ends");
    }
    (seqs, errors)
}

/// A capture cut at every byte reads as the blocks before the cut, the cut said when it is inside a block.
#[test]
fn prefix_of_a_capture_cut_anywhere() {
    let (file, ends) = tape(8);
    for at in 0..=file.len() {
        let whole = ends.iter().filter(|e| **e <= at).count() as u64;
        let at_boundary = at == 0 || ends.contains(&at);
        let (seqs, errors) = read_all(&file[..at], Recovery::Prefix);
        assert_eq!(seqs, (0..whole).collect::<Vec<_>>(), "cut at {at}");
        assert_eq!(errors, usize::from(!at_boundary), "cut at {at}");
        // the salvage reader doesn't panic either, and loses nothing before the cut
        let (seqs, _) = read_all(&file[..at], Recovery::Resync);
        assert_eq!(seqs, (0..whole).collect::<Vec<_>>(), "resync, cut at {at}");
    }
}

/// A flipped byte anywhere: the blocks before its block, then one cut; the salvage reader gets every other block.
#[test]
fn prefix_of_a_capture_with_a_flipped_byte() {
    let (file, ends) = tape(8);
    for at in 0..file.len() {
        for flip in [0x01u8, 0x80, 0xff] {
            let mut bad = file.clone();
            bad[at] ^= flip;
            let block = ends.iter().position(|e| at < *e).unwrap() as u64;
            let (seqs, errors) = read_all(&bad, Recovery::Prefix);
            assert_eq!(
                seqs,
                (0..block).collect::<Vec<_>>(),
                "flip {flip:#x} at {at}"
            );
            assert_eq!(errors, 1, "flip {flip:#x} at {at}");
            let (seqs, _) = read_all(&bad, Recovery::Resync);
            let others: Vec<u64> = (0..8).filter(|s| *s != block).collect();
            assert_eq!(seqs, others, "resync, flip {flip:#x} at {at}");
        }
    }
}

/// Blank bytes after the last block (zeros of preallocated space, 0xFF of erased NAND, both on an SD card) are the
/// end; anything else there is a cut.
#[test]
fn blank_at_the_end_is_the_end() {
    for blank in [
        vec![0u8; 100_000],
        vec![0xFF; 100_000],
        [[0u8; 512], [0xFF; 512]].concat(),
    ] {
        let (mut file, _) = tape(3);
        file.extend_from_slice(&blank);
        assert_eq!(read_all(&file, Recovery::Prefix), (vec![0, 1, 2], 0));
        let r = TapeReader::open(&file[..]).unwrap();
        assert_eq!(r.pieces().last().unwrap().cut, None, "blank is no damage");
        file.push(1);
        assert_eq!(read_all(&file, Recovery::Prefix), (vec![0, 1, 2], 1));
        let r = TapeReader::open(&file[..]).unwrap();
        assert!(r.pieces().last().unwrap().cut.is_some());
    }
}

/// Within a session a data block whose seq goes back is a cut; a block of definitions of another session starts that
/// session; the same (session, seq) again is a duplicate, not a cut: dropped with the same bytes, taken and reported
/// with other bytes.
#[test]
fn seq_order_and_duplicates() {
    let block = |track, id, item: &[u8]| {
        let mut buf = [0u8; 64];
        let mut w = BlockWriter::new(&mut buf, BlockKind::Records, id, &[C1]).unwrap();
        match track {
            RecordTrack::Definitions => w.push_describe(&[item]).unwrap(),
            RecordTrack::Data(track) => w.push(track, at(C1, 0), &[item]).unwrap(),
        }
        let len = w.finish();
        buf[..len].to_vec()
    };
    let other = |seq| BlockId::new(SessionTag::writable(77), seq);
    let walk = |file: &[u8]| {
        let mut rd = FileBlocks::new(file);
        let (mut got, mut errors) = (vec![], vec![]);
        while let Some(b) = rd.next_block() {
            match b {
                Ok(b) => got.push((b.seq, b.records().next().unwrap().unwrap().item.to_vec())),
                Err(e) => errors.push(e),
            }
        }
        (got, errors)
    };
    let file: Vec<u8> = [
        block(RecordTrack::Data(TrackNum::new(1)), id(5), b"a"),
        block(RecordTrack::Data(TrackNum::new(1)), id(6), b"b"),
        block(RecordTrack::Definitions, other(0), b"d"),
        block(RecordTrack::Data(TrackNum::new(1)), other(1), b"c"),
        block(RecordTrack::Data(TrackNum::new(1)), other(1), b"c"),
        block(RecordTrack::Data(TrackNum::new(1)), other(2), b"e"),
        block(RecordTrack::Data(TrackNum::new(1)), other(2), b"E"),
        block(RecordTrack::Data(TrackNum::new(1)), other(3), b"f"),
        block(RecordTrack::Data(TrackNum::new(1)), other(2), b"g"),
    ]
    .concat();
    let (got, errors) = walk(&file);
    let seqs: Vec<(u64, &[u8])> = got.iter().map(|(s, i)| (*s, i.as_slice())).collect();
    assert_eq!(
        seqs,
        [
            (5, &b"a"[..]),
            (6, b"b"),
            (0, b"d"),
            (1, b"c"),
            (2, b"e"),
            (2, b"E"),
            (3, b"f")
        ]
    );
    assert!(
        matches!(
            errors[..],
            [
                FileError::Replaced { seq: 2, .. },
                FileError::Cut {
                    why: Cut::SeqBackwards { seq: 2, after: 3 },
                    ..
                }
            ]
        ),
        "{errors:?}"
    );
    // a records block can't start a session on a file: another session's data block is stale
    let file = [
        block(RecordTrack::Data(TrackNum::new(1)), id(5), b"a"),
        block(RecordTrack::Data(TrackNum::new(1)), other(6), b"b"),
    ]
    .concat();
    let (got, errors) = walk(&file);
    assert_eq!(got.len(), 1);
    assert!(matches!(
        errors[..],
        [FileError::Cut {
            why: Cut::Stale { .. },
            ..
        }]
    ));
}

/// The synced bytes stay where the last sync left them: 0 before it, every byte written at it.
#[test]
fn synced_bytes_follow_each_sync() {
    let tapes = crate::MemPieces::<u64>::new();
    let (defs, file, ends) = defined_tape(2);
    let mut w = TapeWriter::create_on(tapes.create_whole(&0).unwrap(), SyncPolicy::None);
    w.write_block(&defs, &file[..ends[0]]).unwrap();
    assert_eq!(w.synced_bytes(), 0);
    w.sync().unwrap();
    let at_sync = w.bytes();
    assert_eq!(w.synced_bytes(), at_sync);
    w.write_block(&defs, &file[ends[0]..ends[1]]).unwrap();
    assert!(w.bytes() > at_sync);
    assert_eq!(w.synced_bytes(), at_sync);
    w.sync().unwrap();
    assert_eq!(w.synced_bytes(), w.bytes());
}

/// A writer puts each block after the definitions it needs, and a second session appended to the bytes starts with
/// its own layout and producer, so the prefix walk reads both.
#[test]
fn writer_appends_a_second_session() {
    let tapes = crate::MemPieces::<u64>::new();
    let (defs, file, ends) = defined_tape(3);
    let mut w = TapeWriter::create_on(tapes.create_whole(&0).unwrap(), SyncPolicy::EveryBlocks(2));
    let mut start = 0;
    for end in &ends {
        w.write_block(&defs, &file[start..*end]).unwrap();
        start = *end;
    }
    // the layout and the producer, then each block after the definitions it needs
    let written = w.sink().tape().to_vec().unwrap();
    let mut start = 0;
    for end in &ends {
        let b = &file[start..*end];
        assert!(written.windows(b.len()).any(|w| w == b));
        start = *end;
    }
    w.close().unwrap();
    // appended: the second session's blocks start with its layout and producer
    let mut w = TapeWriter::append_on(
        tapes.open_whole(&0).unwrap().unwrap(),
        SyncPolicy::default(),
    )
    .unwrap();
    w.set_producer("p");
    w.tick().unwrap();
    w.close().unwrap();
    let bytes = tapes.closed(&0).unwrap().unwrap().to_vec().unwrap();
    let (_, errors) = read_all(&bytes, Recovery::Prefix);
    assert_eq!(errors, 0);
    let mut rd = FileBlocks::new(&bytes[..]);
    let mut data = vec![];
    while let Some(b) = rd.next_block() {
        let b = b.unwrap();
        if b.kind == BlockKind::Records && !b.is_repeat() {
            data.push(b.seq);
        }
    }
    assert_eq!(data, [0, 1, 2]);
}

/// A capture passes the producer's blocks through with their own seqs: the checkpoints and puts the writer adds
/// between them take no seq of their own, so the prefix walk reads the whole file.
#[test]
fn pass_through_capture_reads_to_the_end() {
    let mut w = TapeWriter::create_on(crate::MemSink::new(), SyncPolicy::None);
    let tape = w.sink().tape();
    w.set_index_policy(IndexPolicy {
        bytes: exact_units::Bytes::new(1024),
        interval: None,
        ..IndexPolicy::default()
    })
    .expect("a policy with room for a piece head");
    w.set_near_policy(writer::NearPolicy {
        copy_spacing: exact_units::Bytes::new(2048),
        ..Default::default()
    });
    // the producer's session: its records numbered 0 and up
    let (defs, file, ends) = defined_tape(60);
    let mut start = 0;
    for end in &ends {
        w.write_block(&defs, &file[start..*end]).unwrap();
        start = *end;
    }
    w.close().unwrap();
    let bytes = tape.to_vec().unwrap();
    let mut rd = FileBlocks::new(&bytes[..]);
    let (mut kinds, mut counted, mut repeats) = (vec![], vec![], 0);
    while let Some(b) = rd.next_block() {
        let b = b.expect("no cut");
        kinds.push(b.kind);
        if b.is_repeat() {
            repeats += 1;
        } else if b.kind == BlockKind::Records {
            counted.push(b.seq);
        }
    }
    assert_eq!(counted, (0..60).collect::<Vec<_>>());
    assert!(
        kinds.iter().filter(|k| **k == BlockKind::Index).count() >= 3,
        "checkpoints and the final index"
    );
    assert!(repeats >= 2, "the definitions put, and copies on use");
    assert_eq!(kinds.last(), Some(&BlockKind::Tail));
}

#[test]
fn varints() {
    use varint::*;
    for v in [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX] {
        let mut b = [0u8; 10];
        let n = put_uvar(&mut b, v).unwrap();
        assert_eq!(n, uvar_len(v));
        assert_eq!(put_uvar(&mut b[..n - 1], v), None, "a short buffer");
        assert_eq!(Reader(&b[..n]).uvar(), Some(v));
    }
    for v in [0i64, -1, 1, -64, 64, i64::MIN, i64::MAX] {
        let mut b = [0u8; 10];
        let n = put_ivar(&mut b, v).unwrap();
        assert_eq!(n, ivar_len(v));
        assert_eq!(Reader(&b[..n]).ivar(), Some(v));
    }
    // 11 bytes of continuation: not a uvar
    assert_eq!(Reader(&[0x80; 11]).uvar(), None);
    assert_eq!(
        Reader(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02]).uvar(),
        None
    );
}

/// The sample definitions and track 9 on the wall clock, which the sample tape's records need.
fn sample_with_wall_track() -> Vec<Describe<'static>> {
    let mut records = sample_describe();
    let Describe::Track(t) = &records[1] else {
        unreachable!("the second sample is a track")
    };
    let on_wall = TrackDef {
        number: TrackNum::new(9),
        clock: WALL,
        repeat: crate::TrackRepeat::Stream,
        ..t.clone()
    };
    let on_host = TrackDef {
        number: TrackNum::new(10),
        repeat: crate::TrackRepeat::Stream,
        ..t.clone()
    };
    records.push(Describe::Track(on_wall));
    records.push(Describe::Track(on_host));
    records
}

/// A tape of `describe` defined in the order given (a typed item put), a state record put and a records block on two
/// clocks, written through [TapeWriter] into memory: the bytes before the writer's end.
fn write_sample_tape(describe: Vec<Describe<'static>>) -> Vec<u8> {
    let mut w = TapeWriter::create_on(crate::MemSink::new(), SyncPolicy::None);
    let tape = w.sink().tape();
    w.set_session(TEST_SESSION).unwrap();
    for d in &describe {
        match d {
            Describe::Producer(p) => w.set_producer(p.name),
            Describe::Layout(l) => w.set_layout(*l).unwrap(),
            _ => {}
        }
    }
    let mut defs = Definitions::new();
    relay(&mut defs, &describe).unwrap();
    for d in &describe {
        if let Describe::Item(item) = d {
            assert!(w.put_item(&defs, item).unwrap());
        }
    }
    assert!(w.put(&defs, TrackNum::new(4), 7, b"session").unwrap());
    assert!(
        !w.put(&defs, TrackNum::new(4), 8, b"session").unwrap(),
        "close behind: not again"
    );
    w.write_block(
        &defs,
        &records_block(
            w.next_id(),
            &[
                (10, at(HOST, 1_000), b"one"),
                (10, at(HOST, 1_250), b"two"),
                (9, at(WALL, 1_763_164_800_000_000_000), b"wall"),
            ],
        ),
    )
    .unwrap();
    let len = w.bytes() as usize;
    w.close().unwrap();
    let bytes = tape.to_vec().unwrap();
    // the writer's final index, close and tail follow; their golden file is golden/v1-index.hex
    bytes[..len].to_vec()
}

fn to_hex_lines(tape: &[u8]) -> String {
    let mut out = String::new();
    let mut rd = FileBlocks::new(tape);
    while let Some(b) = rd.next_block() {
        for byte in b.unwrap().bytes() {
            out.push_str(&format!("{byte:02x}"));
        }
        out.push('\n');
    }
    out
}

/// The same input gives the same bytes, and those bytes are the golden file's (`WW_TAPE_GOLDEN=update` rewrites it):
/// one block per line, in hex.
#[test]
fn deterministic_and_golden() {
    let a = write_sample_tape(sample_with_wall_track());
    let b = write_sample_tape(sample_with_wall_track());
    assert_eq!(a, b, "two writes of the same input");

    let hex = to_hex_lines(&a);
    let golden = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("golden/v1.hex");
    if std::env::var("WW_TAPE_GOLDEN").as_deref() == Ok("update") {
        std::fs::create_dir_all(golden.parent().unwrap()).unwrap();
        std::fs::write(&golden, &hex).unwrap();
    }
    let want =
        std::fs::read_to_string(&golden).expect("golden/v1.hex: run with WW_TAPE_GOLDEN=update");
    assert_eq!(
        hex, want,
        "the format changed: bytes differ from golden/v1.hex"
    );

    // and the golden file reads back to the same records: the definitions the data and items use (the writer's
    // layout among them) once each, the state record put, the data
    let mut describe = vec![];
    let mut times = vec![];
    let mut rd = FileBlocks::new(&a[..]);
    while let Some(b) = rd.next_block() {
        let b = b.unwrap();
        for r in b.records() {
            let r = r.unwrap();
            match r.at {
                None => describe.push(DescribeOwned::from_ww_bytes_owned(r.item).unwrap()),
                Some(t) => times.push((r.track.get(), t)),
            }
        }
    }
    let mut want: Vec<_> = sample_with_wall_track();
    want.push(Describe::Layout(Layout::UNALIGNED));
    let mut want: Vec<DescribeOwned> = want
        .iter()
        .map(|d| {
            let mut scratch = [0u8; 256];
            DescribeOwned::from_ww_bytes_owned(d.to_ww_bytes(&mut scratch).unwrap()).unwrap()
        })
        .collect();
    let key = |d: &DescribeOwned| format!("{d:?}");
    want.sort_by_key(key);
    describe.sort_by_key(key);
    let mut once = describe.clone();
    once.dedup();
    assert_eq!(once, describe, "each once");
    assert!(describe.iter().all(|d| want.contains(d)), "{describe:?}");
    let numbers: Vec<u32> = describe
        .iter()
        .filter_map(|d| match d {
            DescribeOwned::Track(t) => Some(t.number.get()),
            _ => None,
        })
        .collect();
    assert_eq!(
        numbers.len(),
        3,
        "the tracks with data or items: {numbers:?}"
    );
    assert_eq!(
        times,
        [
            (4, at(HOST, 7)),
            (10, at(HOST, 1_000)),
            (10, at(HOST, 1_250)),
            (9, at(WALL, 1_763_164_800_000_000_000)),
        ]
    );
}

/// Typed items hash as they compare, borrowed and owned (eventfull keys its items by them).
#[test]
fn typed_items_hash() {
    use std::collections::HashSet;
    use std::hash::BuildHasher;
    let ty = TypeRef {
        bundle: RefVec::Slice { slice: &[1, 2] },
        krate: FullVersion::new("eventfull", Version::new(0, 4, 0)),
        path: "run::RunRecord",
    };
    let item = |v: &'static [u8]| TypedItem {
        ty: ty.clone(),
        value: RefVec::Slice { slice: v },
    };
    let state = std::hash::RandomState::new();
    assert_eq!(state.hash_one(item(&[7])), state.hash_one(item(&[7])));
    assert_ne!(state.hash_one(item(&[7])), state.hash_one(item(&[8])));
    let mut scratch = [0u8; 128];
    let mut owned = |v| {
        TypedItemOwned::from_ww_bytes_owned(item(v).to_ww_bytes(&mut scratch).unwrap()).unwrap()
    };
    let set: HashSet<TypedItemOwned> = [owned(&[7]), owned(&[8]), owned(&[7])]
        .into_iter()
        .collect();
    assert_eq!(set.len(), 2);
}

/// A track with an empty bundle, path and keys, no item and a stream reads back: the empty bundle starts mid-byte,
/// after the API version's bits.
#[test]
fn bare_track_round_trips() {
    let d = Describe::Track(TrackDef {
        number: TrackNum::new(1),
        key: TrackKey::new(7),
        clock: ClockNum::new(1),
        dimensions: RefVec::Slice { slice: &[] },
        items: crate::Items::WireWeaver {
            api: FullVersion::new("ww_tap", Version::new(0, 1, 0)),
            bundle: RefVec::Slice { slice: &[] },
            path: RefVec::Slice { slice: &[] },
            keys: RefVec::Slice { slice: &[] },
            item: None,
        },
        label: "x",
        repeat: crate::TrackRepeat::Stream,
        kind: TrackKind::Events(EventsDef::INSTANTS),
        relations: RefVec::Slice { slice: &[] },
        owner: [0; 32],
    });
    let mut buf = [0u8; 256];
    let bytes = d.to_ww_bytes(&mut buf).unwrap();
    assert_eq!(Describe::from_ww_bytes(bytes), Ok(d.clone()));
    assert_eq!(
        DescribeOwned::from_ww_bytes_owned(bytes).map(|o| o.to_ww_bytes_owned()),
        Ok(Ok(bytes.to_vec()))
    );
}

/// Check `defs` as one batch against `table`.
fn check_defs<'a, 'd: 'a>(
    table: &Definitions,
    defs: impl IntoIterator<Item = &'a Describe<'d>>,
) -> Result<(), DescribeError> {
    let defs: Vec<&Describe<'_>> = defs.into_iter().collect();
    let items: Vec<Vec<u8>> = defs.iter().map(|d| open::encode(d).unwrap()).collect();
    table.check_batch(defs.iter().zip(&items).map(|(d, i)| (*d, i.as_slice())))
}

/// A chunk of `starts` on track 9 as block `seq`, its payload CRCs written (`crcs`) or not.
fn starts_chunk(seq: u64, starts: &[i64], crcs: bool) -> Vec<u8> {
    let enc = crate::times::EncodedTimes::encode(starts, None, None).unwrap();
    let cols: std::vec::Vec<_> = enc.columns().collect();
    let specs: std::vec::Vec<crate::ColumnSpec> = cols.iter().map(|c| c.0).collect();
    let head = enc
        .head
        .chunk_head(TrackId::new(9), TrackNum::new(1), ItemNum::new(0));
    let mut buf = std::vec![0u8; crate::chunk::chunk_len(&head, &specs, 0)];
    let n = crate::chunk::encode_chunk(&mut buf, id(seq), &head, &cols, 0, crcs).unwrap();
    buf.truncate(n);
    buf
}

/// A chunk block marked FLAG_PAYLOAD_UNCHECKED (Integrity::None) reads by its head, its payload taken as it is: every
/// read says Unprotected, no segment is bad, a flipped payload bit goes unnoticed and a flipped head bit doesn't. Its
/// CRCs filled in, it is the chunk a writer of Integrity::Full writes.
#[test]
fn a_chunk_with_its_payload_unchecked_reads_unprotected() {
    let starts = [1i64, 2, 3];
    let full = starts_chunk(1, &starts, true);
    let mut buf = starts_chunk(1, &starts, false);
    assert_eq!(buf.len(), full.len(), "the same layout");
    let b = Block::read(&buf).unwrap();
    assert_eq!(
        b.flags & crate::FLAG_PAYLOAD_UNCHECKED,
        crate::FLAG_PAYLOAD_UNCHECKED
    );
    assert_eq!(b.checked(), crate::Checked::Unprotected);
    assert!(b.is_intact());
    let c = b.chunk().unwrap();
    assert!(c.is_unchecked());
    assert_eq!(c.bad_segments().count(), 0);
    assert_eq!(
        c.head(),
        Block::read(&full).unwrap().chunk().unwrap().head()
    );
    assert_eq!(
        Block::salvage(&buf).unwrap().checked(),
        crate::Checked::Unprotected
    );
    assert_eq!(
        Block::read_unverified(&buf).unwrap().checked(),
        crate::Checked::Unprotected
    );
    assert!(crate::ChunkHead::read_head(&buf).is_ok());
    // the payload unchecked: a flip in it reads
    let payload_at = c.payload_offset();
    let mut flipped = buf.clone();
    flipped[payload_at] ^= 1;
    assert_eq!(
        Block::read(&flipped).unwrap().checked(),
        crate::Checked::Unprotected
    );
    // the head checked: a flip there doesn't
    let mut flipped = buf.clone();
    flipped[40] ^= 1;
    assert!(Block::read(&flipped).is_err());
    // the CRCs filled in: the bytes of a full chunk
    crate::chunk::fill_payload_crcs(&mut buf).unwrap();
    assert_eq!(buf, full);
    assert_eq!(
        Block::read(&buf).unwrap().checked(),
        crate::Checked::Verified
    );
}

/// A chunk marked FLAG_PAYLOAD_UNCHECKED that holds segment CRCs is no chunk any writer writes: refused, as damage.
#[test]
fn a_chunk_marked_unchecked_with_segment_crcs_is_refused() {
    let mut buf = starts_chunk(1, &[1, 2, 3], true);
    crate::restamp(
        &mut buf,
        id(1),
        crate::FLAG_PAYLOAD_UNCHECKED,
        crate::Reread::Whole,
    )
    .unwrap();
    assert_eq!(Block::read(&buf).err(), Some(crate::BlockError::BadCrc));
    assert_eq!(
        Block::salvage(&buf).unwrap().checked(),
        crate::Checked::Damaged
    );
}

/// A chunk marked FLAG_PAYLOAD_UNCHECKED between good blocks reads as a good block in both recoveries: nothing is
/// cut, refused, salvaged or reported bad.
#[test]
fn a_walk_reads_a_chunk_with_its_payload_unchecked() {
    let (before, _) = tape(3);
    let chunk = starts_chunk(3, &[1, 2, 3], false);
    let mut after = Vec::new();
    for seq in 4..6 {
        after.extend(records_block(id(seq), &[(1, at(C1, 50), b"a")]));
    }
    let bytes = [&before[..], &chunk, &after].concat();
    for recovery in [Recovery::Prefix, Recovery::Resync] {
        let mut rd = FileBlocks::with_recovery(&bytes[..], recovery);
        let (mut seqs, mut errors) = (vec![], vec![]);
        while let Some(b) = rd.next_block() {
            match b {
                Ok(b) => seqs.push(b.seq),
                Err(e) => errors.push(format!("{e:?}")),
            }
        }
        assert_eq!(seqs, [0, 1, 2, 3, 4, 5], "{recovery:?}");
        assert!(errors.is_empty(), "{recovery:?}: {errors:?}");
    }
}

/// Tracks 1 to `n` on [C1], each of its own id: what a test of tracks per piece needs; their ids by number.
fn distinct_tracks(n: u32) -> (Definitions, std::collections::BTreeMap<u32, TrackId>) {
    let mut out = test_definitions(C1, &(1..=n).collect::<Vec<_>>());
    let mut ids = std::collections::BTreeMap::new();
    for d in &mut out {
        if let Describe::Track(t) = d {
            t.key = TrackKey::new(0x7700 + u64::from(t.number.get()));
            ids.insert(t.number.get(), t.id());
        }
    }
    let mut defs = Definitions::new();
    hold(&mut defs, &out).unwrap();
    (defs, ids)
}

/// A records block numbered `id` of one record on each of `tracks`.
fn records_on(id: BlockId, tracks: &[u32]) -> Vec<u8> {
    let mut buf = vec![0u8; 4096];
    let mut w = BlockWriter::new(&mut buf, BlockKind::Records, id, &[C1]).unwrap();
    for t in tracks {
        w.push(TrackNum::new(*t), at(C1, i64::from(*t)), &[b"x"])
            .unwrap();
    }
    let len = w.finish();
    buf.truncate(len);
    buf
}

/// A piece takes data of at most its tracks-per-piece limit ([TRACKS_PER_PIECE_MAX], made small here): a block
/// that would bring in one more track is refused before a byte is written, by every call, and the piece closes.
#[test]
fn a_piece_takes_data_of_at_most_its_track_limit() {
    let (defs, ids) = distinct_tracks(6);
    for layout in [Layout::UNALIGNED, Layout::ALIGNED] {
        let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
        w.set_layout(layout).unwrap();
        w.tracks_max = 3;
        let tape = w.sink().tape();
        w.write_block(&defs, &records_on(w.next_id(), &[1, 2]))
            .unwrap();
        w.defer_block(&defs, records_on(w.next_id(), &[3])).unwrap();
        let len = tape.to_vec().unwrap().len();
        let refused = |r: Result<(), WriteError<MemError>>| {
            matches!(
                r,
                Err(WriteError::Input(InputError::TooManyTracks { max: 3 }))
            )
        };
        let b = records_on(w.next_id(), &[1, 4]);
        assert!(refused(w.write_block(&defs, &b).map(|_| ())));
        assert!(refused(w.adopt_block(&defs, &b).map(|_| ())));
        assert!(refused(w.defer_block(&defs, b)));
        let (head, starts) = chunk_on(5, ids[&5]);
        let cols = [(starts.0, &starts.1[..])];
        assert!(refused(w.write_chunk(&defs, &head, &cols).map(|_| ())));
        assert_eq!(tape.to_vec().unwrap().len(), len, "nothing written");
        // the tracks it has go on
        w.write_block(&defs, &records_on(w.next_id(), &[1, 2, 3]))
            .unwrap();
        let done = w.close().unwrap();
        assert_eq!(done.tracks.len(), 3);
    }
}

/// The next piece's head carries each track the chain carried or gave data, at most its limit ([HEAD_TRACKS_MAX],
/// made small here): a block bringing in a track past it is refused before a byte is written, and a head row of a
/// track with nothing to carry has its flags (byte 12) zero.
#[test]
fn a_piece_head_carries_at_most_its_track_limit() {
    use crate::piece::{PieceHead, PieceHeadBlock, PieceId, PieceIdent, TrackHash, TrackStart};
    let (defs, ids) = distinct_tracks(6);
    let owner = crate::OwnerId([3; 32]);
    let head = PieceHead {
        ident: PieceIdent {
            id: PieceId::first(&owner),
            owner,
            seq: 0,
        },
        prev: None,
        prev_session: SessionTag::NONE,
        integrity: crate::piece::Integrity::Full,
    };
    let carried = |n: u32| TrackStart {
        track_id: ids[&n],
        track_number: TrackNum::new(n),
        next_item: ItemNum::new(0),
        from_tick: i64::MIN,
        owner: crate::OwnerId::NONE,
        hash: TrackHash::START,
        incomplete: false,
        not_carried: false,
    };
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    w.head_rows_max = 3;
    w.begin_piece(&head, &[carried(5), carried(6)]).unwrap();
    let tape = w.sink().tape();
    // a carried track with data and one new track: 3 rows
    w.write_block(&defs, &records_on(w.next_id(), &[5, 1]))
        .unwrap();
    let len = tape.to_vec().unwrap().len();
    let b = records_on(w.next_id(), &[2]);
    assert!(matches!(
        w.write_block(&defs, &b),
        Err(WriteError::Input(InputError::TooManyCarried { max: 3 }))
    ));
    assert_eq!(tape.to_vec().unwrap().len(), len, "nothing written");
    w.write_block(&defs, &records_on(w.next_id(), &[1, 6]))
        .unwrap();
    w.close().unwrap();
    // the head rows as written: none carries the incomplete mark
    let bytes = tape.to_vec().unwrap();
    let h = Block::read(&bytes).unwrap();
    let rows: Vec<TrackStart> = PieceHeadBlock::read(&h).unwrap().tracks().collect();
    assert_eq!(rows.len(), 2);
    let at = HEADER_LEN + 120;
    for k in 0..rows.len() {
        assert_eq!(bytes[at + k * 96 + 12..at + k * 96 + 16], [0; 4]);
    }
}

/// A chunk of 2 items with raw starts on track `number` of id `id`.
fn chunk_on(number: u32, id: TrackId) -> (ChunkHead, (ColumnSpec, Vec<u8>)) {
    let starts: Vec<u8> = [10i64, 20].iter().flat_map(|t| t.to_le_bytes()).collect();
    let head = ChunkHead {
        kind: BlockKind::Events,
        track_id: id,
        track_number: TrackNum::new(number),
        first_item: ItemNum::new(0),
        count: 2,
        first_tick: 10,
        last_tick: 20,
        latest_end: 20,
        time_mode: 0,
        end_mode: EndMode::Absent,
        grid: None,
    };
    let spec = ColumnSpec {
        kind: ColumnKind::Starts,
        codec: Codec::RAW,
        param: 0,
        len: starts.len(),
    };
    (head, (spec, starts))
}

/// A close of [TRACKS_PER_PIECE_MAX] rows and [REMOVED_PIECES_MAX](crate::piece::REMOVED_PIECES_MAX) removed
/// pieces, padded as much as a close is, is a block a reader takes.
#[test]
fn a_close_of_the_most_tracks_fits_a_block() {
    let rows = TRACKS_PER_PIECE_MAX;
    let removed = crate::piece::REMOVED_PIECES_MAX;
    let padding = crate::piece::PIECE_ALIGN as usize - 1;
    assert!(crate::piece::close_len(rows, removed) + padding <= MAX_BLOCK_LEN);
    assert!(crate::piece::close_len(rows + 1, removed) + padding > MAX_BLOCK_LEN);
}

/// What verify holds doesn't grow with the piece: one entry per track list of its final index and the entries of
/// one block, for 16000 blocks as for 256000, both past the copies of the definitions; its steps grow with the blocks.
#[test]
fn verify_holds_entries_per_track_list_not_per_block() {
    let (defs, _) = distinct_tracks(3);
    let held = |n: u32| {
        let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
        let tape = w.sink().tape();
        for k in 0..n {
            let b = records_on(w.next_id(), &[1 + k % 3]);
            w.write_block(&defs, &b).unwrap();
        }
        w.close().unwrap();
        let bytes = tape.to_vec().unwrap();
        let v = crate::TapeReader::open(&bytes[..])
            .unwrap()
            .verify_counted(0)
            .unwrap();
        assert!(v.holds, "{n} blocks verify");
        (v.held, v.steps)
    };
    let ((small, steps_small), (large, steps_large)) = (held(16000), held(256_000));
    assert_eq!(small, large);
    assert!(large <= 8, "{large}");
    assert!(
        steps_large <= 17 * steps_small,
        "{steps_small} {steps_large}"
    );
}

/// A memory sink that refuses a piece start says why in its error and its text, once.
#[test]
fn a_memory_sink_refusing_a_base_says_why() {
    use crate::sink::BaseRefused;
    use core::error::Error;

    let mut one = MemSink::new();
    one.write_all(b"abc").unwrap();
    let e = one.rebase(1).unwrap_err();
    assert_eq!(e, MemError::Base(BaseRefused::OnePiece));
    assert!(e.source().is_none(), "the text says why: {e}");

    let tapes = MemPieces::<u64>::new();
    let mut whole = tapes.create_whole(&0).unwrap();
    whole.write_all(b"abc").unwrap();
    whole.rebase(3).unwrap();
    let e = whole.rebase(4).unwrap_err();
    assert_eq!(e, MemError::Base(BaseRefused::PastEnd));
    assert_eq!(
        e.to_string(),
        "a piece can't start there: the base is past the end of the bytes"
    );
}

/// The error and its causes as one line, as a log prints a chain: each cause after the one it caused.
pub(crate) fn chain_text(e: &dyn core::error::Error) -> String {
    let mut out = e.to_string();
    let mut cause = e.source();
    while let Some(c) = cause {
        out.push_str(": ");
        out.push_str(&c.to_string());
        cause = c.source();
    }
    out
}

/// A codec's error inside a samples, times or payloads error is said once: the message names the column as text,
/// `source()` is the codec's error.
#[test]
fn a_codec_error_is_the_source_of_its_column_error_said_once() {
    let column = crate::times::ColumnError::Corrupt("width over 64");
    let codec = ww_tape_codecs::samples::SampleError::Corrupt { group: 3 };
    let cases: [(&dyn core::error::Error, &str, String); 3] = [
        (
            &crate::samples::SampleError::Column {
                shape: crate::samples::SampleShape::new(
                    crate::samples::ElementDesc::of(crate::samples::Element::I16),
                    2,
                ),
                error: codec,
            },
            "a column of 2 × i16 doesn't code",
            codec.to_string(),
        ),
        (
            &crate::times::TimesError::Corrupt {
                kind: ColumnKind::Starts,
                error: column,
            },
            "the starts column breaks the format",
            column.to_string(),
        ),
        (
            &crate::payloads::PayloadsError::Corrupt {
                kind: ColumnKind::PayloadLengths,
                error: column,
            },
            "the payload lengths column breaks the format",
            column.to_string(),
        ),
    ];
    for (e, message, cause) in cases {
        assert_eq!(e.to_string(), message);
        assert_eq!(e.source().map(ToString::to_string), Some(cause.clone()));
        let chain = chain_text(e);
        assert_eq!(chain.matches(cause.as_str()).count(), 1, "{chain}");
    }
}
