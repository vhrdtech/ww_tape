//! ww_tape chunk blocks on any bytes, and on chunks built from the input with one byte flipped: reading, reading
//! unverified and salvaging never panic, a block that reads also reads unverified, a salvaged chunk's head reads back, alone too, a byte flipped in the payload of a good chunk costs exactly the
//! segment it is in, and a block given another session and seq stays good.
use ww_tape::TrackId;
use ww_tape::TrackNum;
use ww_tape::chunk::{chunk_len, write_chunk};
use ww_tape::{
    Block, BlockId, BlockKind, ChunkHead, Codec, ColumnKind, ColumnSpec, EndMode, SessionTag,
    block_len, set_id,
};

/// Look at everything a block read or salvaged from `data` gives. The head read alone, from the bytes up to the
/// payload, gives the head a salvage gives, and fails where the salvage of a whole chunk block fails.
pub(crate) fn walk(data: &[u8]) {
    let head = ChunkHead::read_head(data);
    let salvaged = Block::salvage(data);
    match (&head, &salvaged) {
        (Ok(h), Ok(b)) => assert_eq!(Some(*h), b.chunk().map(|c| c.head())),
        (Ok(_), Err(_)) => assert!(
            block_len(data).is_ok_and(|n| n > data.len()),
            "a head that reads is of a chunk that salvages, when the block is whole"
        ),
        (Err(_), Ok(b)) => assert!(b.chunk().is_none(), "a salvaged chunk's head reads alone"),
        (Err(_), Err(_)) => {}
    }
    if let Ok(b) = salvaged {
        if b.is_intact() {
            assert!(Block::read(data).is_ok(), "an intact block reads");
        }
        if let Some(c) = b.chunk() {
            let _ = c.head();
            for col in c.columns() {
                let _ = c.column_damaged(&col);
            }
            let bad: Vec<usize> = c.bad_segments().collect();
            assert!(
                bad.iter().all(|s| *s < c.segment_count()),
                "a bad segment is one of the chunk's"
            );
            if b.is_intact() {
                assert!(bad.is_empty(), "an intact chunk has no bad segment");
            }
        }
    }
    // an unverified read checks a subset of what a read checks: it takes every block a read takes, alike
    let unverified = Block::read_unverified(data);
    if let Ok(u) = &unverified {
        let _ = u.chunk().map(|c| c.head());
        let _ = u.records().count();
    }
    if let Ok(b) = Block::read(data) {
        let u = unverified.expect("a block that reads reads unverified");
        assert_eq!((u.kind, u.seq, u.len()), (b.kind, b.seq, b.len()));
        let mut copy = b.bytes().to_vec();
        let id = BlockId::new(
            SessionTag::from_u32(b.session.get().wrapping_add(1)),
            b.seq.wrapping_add(1),
        );
        set_id(&mut copy, id).expect("a good block renumbers");
        let again = Block::read(&copy).expect("still good");
        assert_eq!((again.session, again.seq), (id.session, id.seq));
    }
}

pub fn chunk(data: &[u8]) {
    walk(data);
    // a chunk built from the input: [series?][columns][flip at u32][column bytes...]
    if data.len() < 8 {
        return;
    }
    let series = data[0] & 1 == 1;
    let n = 1 + (data[1] % 4) as usize;
    let flip = u32::from_le_bytes(data[2..6].try_into().expect("4 bytes sliced")) as usize;
    let body = &data[6..];
    let per = body.len() / n;
    let mut cols = vec![];
    for i in 0..n {
        let bytes = &body[i * per..(i + 1) * per];
        let kind = if series {
            ColumnKind::Samples
        } else {
            ColumnKind::from_u8(1 + i as u8)
        };
        cols.push((
            ColumnSpec {
                kind,
                codec: Codec::RAW,
                param: i as u16,
                len: bytes.len(),
            },
            bytes,
        ));
    }
    let head = ChunkHead {
        kind: if series {
            BlockKind::Series
        } else {
            BlockKind::Events
        },
        track_id: TrackId::new(7),
        track_number: TrackNum::new(1),
        first_item: ww_tape::ItemNum::new(0),
        count: 1,
        first_tick: 0,
        last_tick: 0,
        latest_end: 0,
        time_mode: 0,
        end_mode: if series {
            EndMode::Absent
        } else {
            EndMode::Durations
        },
        grid: series.then_some(ww_tape::SeriesGrid::start(0, 0, ww_tape::Magnitude::ONE)),
    };
    let specs: Vec<ColumnSpec> = cols.iter().map(|c| c.0).collect();
    let mut buf = vec![0u8; chunk_len(&head, &specs, 0)];
    let len = write_chunk(
        &mut buf,
        BlockId::new(SessionTag::writable(3), 3),
        &head,
        &cols,
        0,
    )
    .expect("a valid chunk");
    let block = Block::read(&buf[..len]).expect("written good");
    let chunk = block.chunk().expect("a chunk");
    assert_eq!(chunk.head(), head);
    let at = flip % len;
    let payload = chunk.payload_offset();
    buf[at] ^= 0x01;
    walk(&buf[..len]);
    assert!(Block::read(&buf[..len]).is_err(), "a flipped bit is seen");
    if at >= payload && at < len - 4 {
        let b = Block::salvage(&buf[..len]).expect("a payload flip leaves the head good");
        let bad: Vec<usize> = b
            .chunk()
            .expect("a salvaged chunk block is a chunk")
            .bad_segments()
            .collect();
        assert_eq!(bad, [(at - payload) >> 16]);
    }
}
