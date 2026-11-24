use super::*;
use crate::ClockNum;
use crate::ItemNum;
use crate::tests::id;
use crate::{
    Block, BlockError, BlockId, BlockWriter, ClockTime, FileBlocks, FileError, Layout, Recovery,
    SessionTag, SyncPolicy, TapeWriter, set_id,
};

fn raw(kind: ColumnKind, param: u16, bytes: &[u8]) -> (ColumnSpec, &[u8]) {
    (
        ColumnSpec {
            kind,
            codec: Codec::RAW,
            param,
            len: bytes.len(),
        },
        bytes,
    )
}

fn i64s(v: impl IntoIterator<Item = i64>) -> Vec<u8> {
    v.into_iter().flat_map(i64::to_le_bytes).collect()
}

/// An events chunk of `n` items with raw starts, durations and payloads.
fn events(first_item: u64, n: u32) -> (ChunkHead, Vec<Vec<u8>>) {
    let starts: Vec<i64> = (0..n)
        .map(|i| 1_000 + 10 * i64::from(i) + first_item as i64)
        .collect();
    let head = ChunkHead {
        kind: BlockKind::Events,
        track_id: TrackId::new(0x1122_3344_5566_7788),
        track_number: TrackNum::new(4),
        first_item: ItemNum::new(first_item),
        count: n,
        first_tick: starts[0],
        last_tick: *starts.last().unwrap(),
        latest_end: *starts.last().unwrap() + 4,
        time_mode: 0,
        end_mode: EndMode::Durations,
        grid: None,
        tile: crate::TileCoords::NONE,
        level: 0,
    };
    let cols = vec![
        i64s(starts),
        i64s((0..n).map(|i| i64::from(i % 5))),
        (0..n).flat_map(|i| (i % 3).to_le_bytes()).collect(),
        (0..n).flat_map(|i| [b'a' + (i % 26) as u8; 3]).collect(),
    ];
    (head, cols)
}

fn event_columns(cols: &[Vec<u8>]) -> Vec<(ColumnSpec, &[u8])> {
    vec![
        raw(ColumnKind::Starts, 0, &cols[0]),
        raw(ColumnKind::Ends, 0, &cols[1]),
        raw(ColumnKind::PayloadLengths, 0, &cols[2]),
        raw(ColumnKind::Payloads, 0, &cols[3]),
    ]
}

/// A series chunk of `n` i16 samples on two channels.
fn series(first: u64, n: u32) -> (ChunkHead, Vec<Vec<u8>>) {
    let head = ChunkHead {
        kind: BlockKind::Series,
        track_id: TrackId::new(0xabcd),
        track_number: TrackNum::new(7),
        first_item: ItemNum::new(first),
        count: n,
        first_tick: first as i64 * 20,
        last_tick: (first + u64::from(n) - 1) as i64 * 20,
        latest_end: (first + u64::from(n) - 1) as i64 * 20,
        time_mode: 0,
        end_mode: EndMode::Absent,
        grid: Some(crate::SeriesGrid::start(0, 0, crate::Magnitude::ONE)),
        tile: crate::TileCoords::NONE,
        level: 0,
    };
    let ch = |c: u64| -> Vec<u8> {
        (0..u64::from(n))
            .flat_map(|i| (((first + i) * (c + 3)) as i16).to_le_bytes())
            .collect()
    };
    (head, vec![ch(0), ch(1)])
}

fn series_columns(cols: &[Vec<u8>]) -> Vec<(ColumnSpec, &[u8])> {
    vec![
        raw(ColumnKind::Samples, 0, &cols[0]),
        raw(ColumnKind::Samples, 1, &cols[1]),
    ]
}

fn encode(head: &ChunkHead, cols: &[(ColumnSpec, &[u8])], seq: u64, padding: usize) -> Vec<u8> {
    let specs: Vec<ColumnSpec> = cols.iter().map(|c| c.0).collect();
    let mut buf = vec![0u8; chunk_len(head, &specs, padding)];
    let n = write_chunk(&mut buf, id(seq), head, cols, padding).unwrap();
    assert_eq!(n, buf.len());
    buf
}

#[test]
fn crc_combine_is_the_crc_of_both() {
    let data: Vec<u8> = (0..5000u32).map(|i| (i * 7 + i / 13) as u8).collect();
    let whole = CRC.checksum(&data);
    for split in [0, 1, 3, 64, 1000, 4999, 5000] {
        let (a, b) = data.split_at(split);
        assert_eq!(
            crc32c_combine(CRC.checksum(a), CRC.checksum(b), b.len() as u64),
            whole,
            "split at {split}"
        );
    }
}

#[test]
fn events_chunk_round_trip() {
    let (head, cols) = events(100, 50);
    let columns = event_columns(&cols);
    let bytes = encode(&head, &columns, 9, 0);
    let block = Block::read(&bytes).unwrap();
    assert_eq!(
        (block.kind, block.seq, block.record_count),
        (BlockKind::Events, 9, 50)
    );
    assert!(block.is_intact());
    assert_eq!(block.records().count(), 0, "a chunk block has no records");
    let chunk = block.chunk().unwrap();
    assert_eq!(chunk.head(), head);
    assert_eq!(chunk.column_count(), 4);
    for (c, (spec, b)) in chunk.columns().zip(&columns) {
        assert_eq!(
            (c.kind, c.codec, c.param, c.bytes),
            (spec.kind, spec.codec, spec.param, *b)
        );
    }
    assert_eq!(
        chunk.find(ColumnKind::Payloads, 0).unwrap().bytes,
        &cols[3][..]
    );
    assert_eq!(chunk.segment_count(), 1);
    assert_eq!(chunk.bad_segments().count(), 0);
    // 16 zero bytes after the last column, still in the block
    let p = chunk.payload();
    assert!(p[p.len() - PAYLOAD_PADDING..].iter().all(|b| *b == 0));
    // head: header, fixed part, 4 columns, 1 segment CRC
    assert_eq!(chunk.payload_offset(), 32 + 52 + 4 * 8 + 4);
}

/// A series chunk's head carries its grid and scale; an events chunk has none, and the columns start right after the
/// fixed head.
#[test]
fn series_head_carries_its_grid() {
    let (mut head, cols) = series(10, 5);
    head.grid = Some(crate::SeriesGrid::start(
        3,
        -7,
        crate::Magnitude::pow10(-6).mul(crate::Magnitude::PI),
    ));
    head.first_tick = 7 * 20 - 7;
    head.last_tick = 11 * 20 - 7;
    head.latest_end = head.last_tick;
    let bytes = encode(&head, &series_columns(&cols), 1, 0);
    let chunk = Block::read(&bytes).unwrap().chunk().unwrap();
    assert_eq!(chunk.head(), head);
    assert_eq!(chunk.column(1).unwrap().bytes, &cols[1][..]);
    let scale_len = usize::from(bytes[100]);
    assert!(
        scale_len > 2,
        "a scale with pi takes more than a power of 10"
    );
    assert_eq!(&bytes[84..92], &3u64.to_le_bytes());
    assert_eq!(&bytes[92..100], &(-7i64).to_le_bytes());
    // the column entries follow the scale
    assert_eq!(bytes[101 + scale_len], ColumnKind::Samples.to_u8());

    // a series chunk without a grid, a grid after the first sample, an events chunk with one
    let mut buf = vec![0u8; 4096];
    let write = |h: ChunkHead, buf: &mut [u8]| ChunkWriter::new(buf, id(0), &h, &[], 0).err();
    let columns: Vec<ColumnSpec> = series_columns(&cols).iter().map(|c| c.0).collect();
    for bad in [
        ChunkHead { grid: None, ..head },
        ChunkHead {
            grid: Some(crate::SeriesGrid::start(11, 0, crate::Magnitude::ONE)),
            ..head
        },
    ] {
        assert!(matches!(
            ChunkWriter::new(&mut buf, id(0), &bad, &columns, 0),
            Err(ChunkError::Invalid(_))
        ));
    }
    let (events_head, _) = events(0, 3);
    assert!(
        write(
            ChunkHead {
                grid: head.grid,
                ..events_head
            },
            &mut buf
        )
        .is_some()
    );

    // a scale length of 0 or past SCALE_LEN_MAX, or one that cuts the scale: a bad head, even with a good head CRC
    for len in [0u8, 1, SCALE_LEN_MAX as u8 + 1] {
        let mut b = bytes.clone();
        b[100] = len;
        let at = u32::from_le_bytes(b[32..36].try_into().unwrap()) as usize;
        let crc = head_crc(&b, at).unwrap();
        b[28..32].copy_from_slice(&crc.to_le_bytes());
        assert!(Chunk::read_head(&b).is_err(), "scale length {len}");
    }
}

#[test]
fn chunk_heads_are_checked_on_write() {
    let (head, cols) = events(0, 3);
    let columns = event_columns(&cols);
    let mut buf = vec![0u8; 4096];
    let bad = |h: ChunkHead, c: &[(ColumnSpec, &[u8])]| {
        let mut b = vec![0u8; 4096];
        matches!(
            write_chunk(&mut b, id(0), &h, c, 0),
            Err(ChunkError::Invalid(_))
        )
    };
    assert!(bad(ChunkHead { count: 0, ..head }, &columns));
    assert!(bad(
        ChunkHead {
            last_tick: head.first_tick - 1,
            ..head
        },
        &columns
    ));
    assert!(bad(
        ChunkHead {
            first_item: ItemNum::new(u64::MAX),
            ..head
        },
        &columns
    ));
    assert!(bad(
        ChunkHead {
            kind: BlockKind::Records,
            ..head
        },
        &columns
    ));
    // the latest end: at least the last tick, and the last tick in a chunk of instants
    assert!(bad(
        ChunkHead {
            latest_end: head.last_tick - 1,
            ..head
        },
        &columns
    ));
    assert!(bad(
        ChunkHead {
            end_mode: EndMode::Absent,
            ..head
        },
        &columns
    ));
    assert!(!bad(
        ChunkHead {
            end_mode: EndMode::Absent,
            latest_end: head.last_tick,
            ..head
        },
        &columns
    ));
    assert!(bad(head, &[]));
    assert!(bad(head, &[raw(ColumnKind::Samples, 0, &cols[0])]));
    let (shead, scols) = series(0, 3);
    assert!(bad(shead, &[raw(ColumnKind::Starts, 0, &scols[0])]));
    assert!(bad(
        ChunkHead {
            latest_end: shead.last_tick + 1,
            ..shead
        },
        &series_columns(&scols)
    ));
    assert!(bad(
        ChunkHead {
            end_mode: EndMode::Durations,
            ..shead
        },
        &series_columns(&scols)
    ));
    assert!(matches!(
        write_chunk(&mut buf[..100], id(0), &head, &columns, 0),
        Err(ChunkError::Full { .. })
    ));
}

/// A series chunk over many segments: a flipped bit fails the block, and salvage loses exactly the segment it is in.
#[test]
fn flipped_bit_in_a_series_chunk_loses_one_segment() {
    // 2 channels × 300 000 samples × 2 bytes: 1.2 MB, 10 segments of 64 KiB
    let (head, cols) = series(0, 300_000);
    let bytes = encode(&head, &series_columns(&cols), 1, 0);
    let chunk = Block::read(&bytes).unwrap().chunk().unwrap();
    let (at, segs) = (chunk.payload_offset(), chunk.segment_count());
    assert_eq!(segs, (1_200_000 + PAYLOAD_PADDING).div_ceil(1 << 16));
    for offset in [0usize, 1, 65_535, 65_536, 600_000, 1_199_999, 1_200_010] {
        let mut bad = bytes.clone();
        bad[at + offset] ^= 0x10;
        assert_eq!(
            Block::read(&bad).unwrap_err(),
            BlockError::BadCrc,
            "flip at {offset}"
        );
        let b = Block::salvage(&bad).unwrap();
        assert!(!b.is_intact());
        let c = b.chunk().unwrap();
        assert_eq!(c.head(), head);
        assert_eq!(
            c.bad_segments().collect::<Vec<_>>(),
            [offset >> 16],
            "flip at {offset}"
        );
        let ch0 = c.column(0).unwrap();
        let ch1 = c.column(1).unwrap();
        // a channel is damaged where the bad segment overlaps it or the 16 bytes a codec may load past its end
        assert_eq!(
            c.column_damaged(&ch0),
            offset < 600_000 + PAYLOAD_PADDING || offset >> 16 == 599_999 >> 16
        );
        assert_eq!(
            c.column_damaged(&ch1),
            offset >= 600_000 || offset >> 16 == 600_000 >> 16
        );
        // the good segments are byte for byte what was written
        for s in 0..segs {
            if s != offset >> 16 {
                assert_eq!(c.segment(s), chunk.segment(s));
            }
        }
    }
    // the block CRC alone is damaged: every segment is good, the block is still not intact
    let mut bad = bytes.clone();
    let n = bad.len();
    bad[n - 1] ^= 1;
    let b = Block::salvage(&bad).unwrap();
    assert!(!b.is_intact());
    assert_eq!(b.chunk().unwrap().bad_segments().count(), 0);
}

/// The head alone reads as the whole block's head does: from the bytes up to the payload, which may be damaged or
/// absent; a flipped bit in those bytes is refused as by the whole read, and a block of another kind is not a chunk.
#[test]
fn head_reads_without_the_payload() {
    let (events_head, events_cols) = events(5, 20);
    let (mut series_head, series_cols) = series(10, 5);
    series_head.grid = Some(crate::SeriesGrid::start(
        3,
        -7,
        crate::Magnitude::pow10(-6).mul(crate::Magnitude::PI),
    ));
    series_head.first_tick = 7 * 20 - 7;
    series_head.last_tick = 11 * 20 - 7;
    series_head.latest_end = series_head.last_tick;
    for (head, bytes) in [
        (
            events_head,
            encode(&events_head, &event_columns(&events_cols), 1, 0),
        ),
        (
            series_head,
            encode(&series_head, &series_columns(&series_cols), 1, 0),
        ),
    ] {
        let payload_at = Block::read(&bytes)
            .unwrap()
            .chunk()
            .unwrap()
            .payload_offset();
        let n = ChunkHead::head_len(&bytes[..CHUNK_HEAD_PREFIX]).unwrap();
        assert_eq!(n, payload_at);
        assert_eq!(ChunkHead::read_head(&bytes[..n]).unwrap(), head);
        assert_eq!(
            ChunkHead::read_head(&bytes[..n - 1]).unwrap_err(),
            BlockError::Short { needed: n }
        );
        // the payload isn't read: damaged, the head still reads
        let mut bad = bytes.clone();
        bad[payload_at] ^= 1;
        assert_eq!(Block::read(&bad).unwrap_err(), BlockError::BadCrc);
        assert_eq!(ChunkHead::read_head(&bad).unwrap(), head);
        for at in (0..payload_at).filter(|at| !(8..12).contains(at)) {
            let mut bad = bytes.clone();
            bad[at] ^= 0x04;
            assert!(ChunkHead::read_head(&bad).is_err(), "flip at {at}");
        }
    }
    let mut records = [0u8; 64];
    let mut w = BlockWriter::new(&mut records, crate::BlockKind::Records, id(0), &[]).unwrap();
    w.push_describe(&[b"a describe record"]).unwrap();
    w.finish();
    assert_eq!(
        ChunkHead::head_len(&records[..CHUNK_HEAD_PREFIX]).unwrap_err(),
        BlockError::NotChunk
    );
}

/// A damaged head is not salvaged: nothing in the block, its length neither, can be trusted.
#[test]
fn damaged_head_is_not_salvaged() {
    let (head, cols) = events(0, 20);
    let bytes = encode(&head, &event_columns(&cols), 1, 0);
    let payload_at = Block::read(&bytes)
        .unwrap()
        .chunk()
        .unwrap()
        .payload_offset();
    for at in 0..payload_at {
        if (8..12).contains(&at) {
            continue; // the length: the block is then short or long, not a chunk to salvage
        }
        let mut bad = bytes.clone();
        bad[at] ^= 0x04;
        assert!(Block::salvage(&bad).is_err(), "flip at {at}");
    }
}

#[test]
fn padding_puts_the_payload_on_the_alignment() {
    let (head, cols) = series(10, 1000);
    let columns = series_columns(&cols);
    let specs: Vec<ColumnSpec> = columns.iter().map(|c| c.0).collect();
    let payload = specs.iter().map(|s| s.len).sum();
    let head_len = chunk_head_len(&head, 2, payload);
    for offset in [0u64, 1, 100, 4096 - head_len as u64, 4097, 123_456] {
        let pad = chunk_padding(offset, head_len, 4096).unwrap();
        let bytes = encode(&head, &columns, 0, pad);
        let c = Block::read(&bytes).unwrap().chunk().unwrap();
        assert_eq!(
            (offset + c.payload_offset() as u64) % 4096,
            0,
            "at {offset}"
        );
        assert!(pad < 4096);
    }
    assert_eq!(chunk_padding(77, head_len, 0), Some(0));
    assert_eq!(
        chunk_padding(77, head_len, 3000),
        None,
        "not a power of two"
    );
    assert_eq!(
        chunk_padding(u64::MAX, head_len, 4096).map(|p| p < 4096),
        Some(true)
    );
}

#[test]
fn renumbered_blocks_stay_good() {
    let (head, cols) = series(0, 70_000);
    let mut bytes = encode(&head, &series_columns(&cols), 1, 33);
    let other = BlockId::new(SessionTag::writable(9), 42);
    set_id(&mut bytes, other).unwrap();
    let b = Block::read(&bytes).unwrap();
    assert_eq!((b.session, b.seq), (other.session, 42));
    let mut buf = [0u8; 64];
    let mut w = BlockWriter::new(&mut buf, BlockKind::Records, id(3), &[ClockNum::new(1)]).unwrap();
    w.push(
        TrackNum::new(1),
        ClockTime::new(ClockNum::new(1), 5),
        &[b"x"],
    )
    .unwrap();
    let len = w.finish();
    set_id(&mut buf[..len], other.next()).unwrap();
    let b = Block::read(&buf[..len]).unwrap();
    assert_eq!((b.session, b.seq), (other.session, 43));
}

/// Column lengths whose sum overflows a usize make a chunk too long, never a wrapped length.
#[test]
fn overflowing_column_lengths_are_too_long() {
    let (head, _) = events(0, 3);
    let spec = |kind| ColumnSpec {
        kind,
        codec: Codec::RAW,
        param: 0,
        len: usize::MAX / 2 + 1,
    };
    let specs = [spec(ColumnKind::Starts), spec(ColumnKind::Payloads)];
    assert_eq!(chunk_len(&head, &specs, 0), usize::MAX);
    assert_eq!(chunk_head_len(&head, usize::MAX, 0), usize::MAX);
    let mut buf = vec![0u8; 4096];
    assert!(matches!(
        ChunkWriter::new(&mut buf, id(0), &head, &specs, 0),
        Err(ChunkError::TooLong { len: usize::MAX })
    ));
}

/// A head out of range is refused by the checked restamp, which writes nothing.
#[test]
fn restamp_refuses_a_head_out_of_range() {
    let (head, cols) = events(0, 3);
    let good = encode(&head, &event_columns(&cols), 1, 0);
    let mut bytes = good.clone();
    assert_eq!(restamp(&mut bytes), Ok(()));
    assert_eq!(
        bytes, good,
        "a restamp of an unchanged header changes nothing"
    );
    let mut bad = good.clone();
    bad[80] = SEGMENT_SHIFT_MAX + 1;
    let before = bad.clone();
    assert!(matches!(restamp(&mut bad), Err(ChunkHeadError::Bound(_))));
    assert_eq!(bad, before);
    let mut bad = good.clone();
    bad[32..36].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(restamp(&mut bad), Err(ChunkHeadError::BadHeadCrc));
    assert_eq!(restamp(&mut [0u8; 8]), Err(ChunkHeadError::BadHeadCrc));
}

/// Definitions, small records blocks deferred into the gaps, and chunks, through [TapeWriter].
fn write_tape(layout: Layout) -> Vec<u8> {
    let mut w = TapeWriter::create_on(crate::MemSink::new(), SyncPolicy::None);
    let tape = w.sink().tape();
    w.set_session(crate::tests::TEST_SESSION).unwrap();
    w.set_alignment(layout.alignment.get()).unwrap();
    w.set_producer("test");
    w.set_layout(layout).unwrap();
    let mut defs = crate::Definitions::new();
    let mut held = crate::tests::test_definitions(ClockNum::new(1), &[2, 4]);
    held.push(crate::tests::test_series_track(
        ClockNum::new(1),
        TrackNum::new(7),
        2,
        crate::Ratio64::int(20).unwrap(),
    ));
    crate::tests::relay(&mut defs, &held).unwrap();
    let small = |n: i64| {
        let mut buf = [0u8; 128];
        let mut b =
            BlockWriter::new(&mut buf, BlockKind::Records, id(0), &[ClockNum::new(1)]).unwrap();
        b.push(
            TrackNum::new(2),
            ClockTime::new(ClockNum::new(1), n),
            &[&[n as u8; 20]],
        )
        .unwrap();
        let len = b.finish();
        buf[..len].to_vec()
    };
    let (mut sh, sc) = series(0, 900);
    sh.track_id = crate::tests::test_track_id();
    w.write_chunk(&defs, &sh, &series_columns(&sc)).unwrap();
    w.defer_block(&defs, small(1)).unwrap();
    w.defer_block(&defs, small(2)).unwrap();
    let (mut eh, ec) = events(0, 40);
    eh.track_id = crate::tests::test_track_id();
    w.write_chunk(&defs, &eh, &event_columns(&ec)).unwrap();
    w.defer_block(&defs, small(3)).unwrap();
    let (mut sh, sc) = series(900, 300);
    sh.track_id = crate::tests::test_track_id();
    w.write_chunk(&defs, &sh, &series_columns(&sc)).unwrap();
    w.defer_block(&defs, small(4)).unwrap();
    w.close().unwrap();
    tape.to_vec().unwrap()
}

/// A block a walk gave: offset, seq, kind, the chunk's payload offset in the file, intact.
type Seen = (u64, u64, BlockKind, Option<u64>, bool);

/// Every block of a file in order.
fn walk(bytes: &[u8], recovery: Recovery) -> (Vec<Seen>, usize) {
    let mut rd = FileBlocks::with_recovery(bytes, recovery);
    let (mut out, mut errors) = (vec![], 0);
    while let Some(b) = rd.next_block() {
        match b {
            Ok(b) => {
                let (seq, kind, intact) = (b.seq, b.kind, b.is_intact());
                let payload = b.chunk().map(|c| c.payload_offset() as u64);
                let off = rd.offset();
                out.push((off, seq, kind, payload.map(|p| p + off), intact));
            }
            Err(_) => errors += 1,
        }
        assert!(errors < 100_000, "the walk ends");
    }
    (out, errors)
}

/// The blocks [walk] gives that aren't marked FLAG_REPEAT: the puts passed over.
fn counted(bytes: &[u8]) -> Vec<Seen> {
    let reps = repeats(bytes);
    let (blocks, _) = walk(bytes, Recovery::Prefix);
    blocks
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !reps.contains(i))
        .map(|(_, b)| b)
        .collect()
}

/// Where the blocks marked FLAG_REPEAT (puts and copies) are among a file's blocks.
fn repeats(bytes: &[u8]) -> Vec<usize> {
    let mut rd = FileBlocks::new(bytes);
    let mut out = vec![];
    let mut i = 0;
    while let Some(b) = rd.next_block() {
        if b.unwrap().is_repeat() {
            out.push(i);
        }
        i += 1;
    }
    out
}

#[test]
fn aligned_file_puts_payloads_on_pages_and_fills_gaps() {
    let file = write_tape(Layout::ALIGNED);
    let again = write_tape(Layout::ALIGNED);
    let plain = write_tape(Layout::UNALIGNED);
    assert_eq!(file, again, "same input, same bytes");
    let (_, errors) = walk(&file, Recovery::Prefix);
    assert_eq!(errors, 0);
    let blocks = counted(&file);
    // the final index, close and tail at the end carry the last counted seq
    let seqs: Vec<u64> = blocks.iter().map(|b| b.1).collect();
    assert_eq!(
        seqs,
        (0..7).chain([6; 3]).collect::<Vec<_>>(),
        "seqs ascend"
    );
    let kinds: Vec<BlockKind> = blocks.iter().map(|b| b.2).collect();
    use BlockKind::*;
    // the deferred blocks 1 and 2 fill the gap before the events chunk, 3 that before the second series chunk; 4 has
    // no chunk after it and goes at the end, then the writer's final index, close and tail. Between them, the puts
    // of the layout, the producer and the definitions each block needs.
    let want = [
        Series, Records, Records, Events, Records, Series, Records, Index, Close, Tail,
    ];
    assert_eq!(kinds, want);
    assert_eq!(repeats(&file)[0], 0, "the layout and the producer first");
    for (_, _, _, payload, _) in &blocks {
        if let Some(p) = payload {
            assert_eq!(p % 4096, 0);
        }
    }
    let kinds: Vec<BlockKind> = counted(&plain).iter().map(|b| b.2).collect();
    assert_eq!(kinds, want);
    assert!(plain.len() < file.len());
}

/// A file cut at every byte reads as its blocks before the cut, aligned or not.
#[test]
fn chunk_file_cut_anywhere_reads_its_longest_good_prefix() {
    for layout in [Layout::UNALIGNED, Layout::ALIGNED] {
        let file = write_tape(layout);
        let (blocks, _) = walk(&file, Recovery::Prefix);
        let ends: Vec<usize> = blocks
            .iter()
            .skip(1)
            .map(|b| b.0 as usize)
            .chain([file.len()])
            .collect();
        for at in 0..=file.len() {
            let whole = ends.iter().filter(|e| **e <= at).count();
            let (got, errors) = walk(&file[..at], Recovery::Prefix);
            assert_eq!(got.len(), whole, "{layout:?} cut at {at}");
            assert_eq!(
                errors,
                usize::from(at != 0 && !ends.contains(&at)),
                "cut at {at}"
            );
        }
    }
}

/// In an aligned file, a flipped bit in a series payload costs that chunk's one segment; a flipped bit in a head
/// costs that block, and the walk goes on at the next one.
#[test]
fn resync_salvages_chunks_in_a_file() {
    let file = write_tape(Layout::ALIGNED);
    let (blocks, _) = walk(&file, Recovery::Prefix);
    let series = *blocks.iter().find(|b| b.2 == BlockKind::Series).unwrap();
    let payload = series.3.unwrap() as usize;
    let mut bad = file.clone();
    bad[payload + 100] ^= 0x01;
    let (got, errors) = walk(&bad, Recovery::Resync);
    assert_eq!(errors, 0);
    assert_eq!(got.len(), blocks.len());
    assert_eq!(got.iter().filter(|b| !b.4).count(), 1);
    let mut rd = FileBlocks::with_recovery(&bad[..], Recovery::Resync);
    let mut lost = vec![];
    while let Some(b) = rd.next_block() {
        let b = b.unwrap();
        if !b.is_intact() {
            lost.extend(b.chunk().unwrap().bad_segments());
        }
    }
    assert_eq!(lost, [0]);
    // next_good leaves it out and says so
    let mut rd = FileBlocks::with_recovery(&bad[..], Recovery::Resync);
    let mut reported = 0;
    let mut good = 0;
    while rd
        .next_good(|e| {
            assert!(matches!(e, FileError::Bad { .. }));
            reported += 1;
        })
        .is_some()
    {
        good += 1;
    }
    assert_eq!((good, reported), (blocks.len() - 1, 1));

    // the head of the events chunk: that block is lost, every other is read
    let events = blocks[4];
    let mut bad = file.clone();
    bad[events.0 as usize + 40] ^= 0x01;
    let (got, errors) = walk(&bad, Recovery::Resync);
    assert_eq!(errors, 1);
    let seqs: Vec<u64> = got.iter().map(|b| b.1).collect();
    let want: Vec<u64> = blocks
        .iter()
        .map(|b| b.1)
        .filter(|s| *s != events.1)
        .collect();
    assert_eq!(seqs, want);
    // the prefix reader stops at the damaged chunk
    let (got, errors) = walk(&bad, Recovery::Prefix);
    assert_eq!((got.len(), errors), (4, 1));
}

/// The aligned sample tape is the golden file's (`WW_TAPE_GOLDEN=update` rewrites it): one block per line, in hex.
#[test]
fn chunks_golden() {
    let file = write_tape(Layout::ALIGNED);
    let mut hex = String::new();
    let mut rd = FileBlocks::new(&file[..]);
    while let Some(b) = rd.next_block() {
        hex.push_str(&crate::tests::hex(b.unwrap().bytes()));
        hex.push('\n');
    }
    let golden = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("golden/v1-chunks.hex");
    if std::env::var("WW_TAPE_GOLDEN").as_deref() == Ok("update") {
        std::fs::write(&golden, &hex).unwrap();
    }
    let want = std::fs::read_to_string(&golden)
        .expect("golden/v1-chunks.hex: run with WW_TAPE_GOLDEN=update");
    assert!(
        hex == want,
        "the format changed: bytes differ from golden/v1-chunks.hex"
    );
}

/// `write_chunk` writes every byte of the block once: a buffer full of other bytes gives the same block as zeros,
/// for an events and a series chunk with padding.
#[test]
fn write_chunk_needs_no_zeroed_buffer() {
    let (eh, ec) = events(5, 30);
    let (sh, sc) = series(0, 40_000);
    for (head, columns) in [(eh, event_columns(&ec)), (sh, series_columns(&sc))] {
        let specs: Vec<ColumnSpec> = columns.iter().map(|c| c.0).collect();
        let len = chunk_len(&head, &specs, 777);
        let mut zeros = vec![0u8; len];
        let mut dirty = vec![0xa5u8; len];
        let a = write_chunk(&mut zeros, id(3), &head, &columns, 777).unwrap();
        let b = write_chunk(&mut dirty, id(3), &head, &columns, 777).unwrap();
        assert_eq!(a, b);
        assert_eq!(zeros, dirty);
        assert!(Block::read(&dirty).unwrap().is_intact());
    }
}

/// A scale that doesn't encode keeps the encoder's error as its source.
#[cfg(feature = "std")]
#[test]
fn a_scale_error_keeps_its_cause() {
    use core::error::Error as _;
    let e = ChunkError::Scale(shrink_wrap::Error::NestingTooDeep);
    assert!(e.source().is_some(), "the encoder's error is the source");
}

/// A grid chunk of tile (7, 3, 2, 5) reads back with its coordinates and samples; its head is the series chunk's
/// with byte 82 the count of fixed axes and 16 bytes of coordinates after the scale.
#[test]
fn a_grid_chunk_carries_its_tile_coordinates() {
    let (series, cols) = series(10, 50);
    let tile = TileCoords::new(&[7, 3, 2, 5]).unwrap();
    let head = ChunkHead { tile, ..series };
    let bytes = encode(&head, &series_columns(&cols), 1, 0);
    let plain = encode(&series, &series_columns(&cols), 1, 0);
    assert_eq!(bytes.len(), plain.len() + 16);
    assert_eq!(bytes[82], 4);
    assert_eq!(plain[82], 0);
    let b = Block::read(&bytes).unwrap();
    let c = b.chunk().unwrap();
    assert_eq!(c.head, head);
    assert_eq!(c.head.tile.as_slice(), [7, 3, 2, 5]);
    assert_eq!(ChunkHead::read_head(&bytes).unwrap(), head);
    assert_eq!(
        c.column(0).unwrap().bytes,
        Block::read(&plain)
            .unwrap()
            .chunk()
            .unwrap()
            .column(0)
            .unwrap()
            .bytes
    );
    let most = TileCoords::new(&[u32::MAX; GRID_FIXED_AXES_MAX]).unwrap();
    let head = ChunkHead {
        tile: most,
        ..series
    };
    let bytes = encode(&head, &series_columns(&cols), 1, 0);
    assert_eq!(ChunkHead::read_head(&bytes).unwrap().tile, most);
}

/// Byte 82 of a series chunk past GRID_FIXED_AXES_MAX is refused as a bound, its CRCs made good; an events chunk
/// takes no tile coordinates.
#[test]
fn tile_coordinates_are_bounded_and_series_only() {
    let (series, cols) = series(0, 20);
    let good = encode(&series, &series_columns(&cols), 1, 0);
    let mut bad = good.clone();
    bad[82] = u8::try_from(GRID_FIXED_AXES_MAX + 1).unwrap();
    assert!(
        ChunkHead::head_len(&bad).is_ok(),
        "the payload offset still reads"
    );
    assert!(matches!(restamp(&mut bad), Err(ChunkHeadError::Bound(_))));
    // a count that runs the coordinates into the column table: the CRC fails or the head is out of range
    let mut bad = good.clone();
    bad[82] = 1;
    assert!(Block::read(&bad).is_err());
    assert!(ChunkHead::read_head(&bad).is_err());

    let (events, cols) = events(0, 3);
    let head = ChunkHead {
        tile: TileCoords::new(&[1]).unwrap(),
        ..events
    };
    let specs: Vec<ColumnSpec> = event_columns(&cols).iter().map(|c| c.0).collect();
    let mut buf = vec![0u8; chunk_len(&head, &specs, 0) + 64];
    assert_eq!(
        write_chunk(&mut buf, id(1), &head, &event_columns(&cols), 0),
        Err(ChunkError::Invalid("an events chunk with tile coordinates"))
    );
}

/// An overview chunk of tile (1, 2) at level 3: three statistics columns of two channels each.
fn overview(level: u8) -> (ChunkHead, Vec<Vec<u8>>) {
    let (series, cols) = series(0, 4);
    let head = ChunkHead {
        tile: TileCoords::new(&[1, 2]).unwrap(),
        level,
        ..series
    };
    let mut stats = cols.clone();
    stats.push(cols[0].iter().rev().copied().collect());
    (head, stats)
}

fn overview_columns(cols: &[Vec<u8>]) -> Vec<(ColumnSpec, &[u8])> {
    vec![
        raw(ColumnKind::OverviewMin, 0, &cols[0]),
        raw(ColumnKind::OverviewMax, 0, &cols[1]),
        raw(ColumnKind::OverviewMean, 0, &cols[2]),
    ]
}

/// An overview chunk's level is byte 83 and comes back in its head and its index entry; the column kinds are 9, 10
/// and 11. A grid chunk of the cells as written keeps byte 83 at 0.
#[test]
fn an_overview_chunk_carries_its_level_in_byte_83() {
    let (head, cols) = overview(3);
    let bytes = encode(&head, &overview_columns(&cols), 1, 0);
    assert_eq!(bytes[83], 3);
    let b = Block::read(&bytes).unwrap();
    let c = b.chunk().unwrap();
    assert_eq!(c.head, head);
    assert_eq!(ChunkHead::read_head(&bytes).unwrap(), head);
    let kinds: Vec<u8> = c.columns().map(|c| c.kind.to_u8()).collect();
    assert_eq!(kinds, [9, 10, 11]);
    let entry = crate::index::IndexEntry::of_chunk(0, bytes.len(), &c.head).unwrap();
    assert_eq!(entry.level, 3);

    let (series, cols) = series(0, 4);
    let plain = ChunkHead {
        tile: TileCoords::new(&[1, 2]).unwrap(),
        ..series
    };
    let bytes = encode(&plain, &series_columns(&cols), 1, 0);
    assert_eq!(bytes[83], 0);
    assert_eq!(Block::read(&bytes).unwrap().chunk().unwrap().head.level, 0);
}

/// A writer refuses an overview level past GRID_OVERVIEW_LEVELS_MAX, one on a chunk of no fixed axes or an events
/// chunk, an overview with a samples column or without each statistic once, and a chunk as written with a
/// statistic.
#[test]
fn overview_chunks_are_checked_on_write() {
    let refused = |head: &ChunkHead, cols: &[(ColumnSpec, &[u8])]| {
        let specs: Vec<ColumnSpec> = cols.iter().map(|c| c.0).collect();
        let mut buf = vec![0u8; chunk_len(head, &specs, 0) + 64];
        match write_chunk(&mut buf, id(1), head, cols, 0) {
            Err(ChunkError::Invalid(why)) => why,
            other => panic!("{other:?}"),
        }
    };
    let (head, cols) = overview(GRID_OVERVIEW_LEVELS_MAX);
    encode(&head, &overview_columns(&cols), 1, 0);
    let past = ChunkHead {
        level: GRID_OVERVIEW_LEVELS_MAX + 1,
        ..head
    };
    assert_eq!(
        refused(&past, &overview_columns(&cols)),
        "an overview level over GRID_OVERVIEW_LEVELS_MAX"
    );
    let no_axes = ChunkHead {
        tile: TileCoords::NONE,
        ..head
    };
    assert_eq!(
        refused(&no_axes, &overview_columns(&cols)),
        "an overview level on a chunk of no fixed axes"
    );
    let (events, ecols) = events(0, 3);
    let events = ChunkHead { level: 1, ..events };
    assert_eq!(
        refused(&events, &event_columns(&ecols)),
        "an overview level on a chunk of no fixed axes"
    );
    let mut with_samples = overview_columns(&cols);
    with_samples.push(raw(ColumnKind::Samples, 0, &cols[0]));
    assert_eq!(
        refused(&head, &with_samples),
        "an overview chunk with a samples column"
    );
    let stats =
        "an overview chunk without one min, max and mean column each, or another chunk with one";
    let all = overview_columns(&cols);
    assert_eq!(refused(&head, &all[..2]), stats);
    let twice = [all[0], all[0], all[1], all[2]];
    assert_eq!(refused(&head, &twice), stats);
    let (series, scols) = series(0, 4);
    let mut plain = series_columns(&scols);
    plain.push(all[2]);
    assert_eq!(refused(&series, &plain), stats);
}

/// A read refuses byte 83 of a series chunk past GRID_OVERVIEW_LEVELS_MAX, or not 0 in a series of no fixed axes,
/// its CRCs made good.
#[test]
fn overview_levels_are_checked_on_read() {
    let (head, cols) = overview(1);
    let good = encode(&head, &overview_columns(&cols), 1, 0);
    let mut bad = good.clone();
    bad[83] = GRID_OVERVIEW_LEVELS_MAX + 1;
    restamp(&mut bad).unwrap();
    assert!(Block::read(&bad).map_or(true, |b| b.chunk().is_none()));
    assert!(ChunkHead::read_head(&bad).is_err());

    let (series, cols) = series(0, 4);
    let mut bad = encode(&series, &series_columns(&cols), 1, 0);
    bad[83] = 1;
    restamp(&mut bad).unwrap();
    assert!(Block::read(&bad).map_or(true, |b| b.chunk().is_none()));
    assert!(ChunkHead::read_head(&bad).is_err());
}
