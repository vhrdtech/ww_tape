//! The reserved parts of the format read as the format page says: reserved block kinds in a file, reserved header flag
//! bits on blocks other than chunks, index entries longer than a reader knows, and a raw medium holding appended
//! pieces.

use std::path::Path;

use crate::definitions::base_defs;
use crate::pieces::{chunk, column, records};
use ww_tape::index::{IndexBlock, IndexKind, IndexPolicy};
use ww_tape::{
    BlockId, BlockKind, ChainOptions, CutPolicy, FileBlocks, Layout, MemSink, MemTapes, OwnerId,
    PieceChain, PieceFiles, SyncPolicy, TapeReader, TapeWriter, TrackHash, WriterOptions,
};
use ww_tape::{Medium, OpenOptions};

fn crc32c(bytes: &[u8]) -> u32 {
    crc::Crc::<u32>::new(&crc::CRC_32_ISCSI).checksum(bytes)
}

/// Make `block`'s CRC good again.
fn reseal(block: &mut [u8]) {
    let n = block.len();
    let crc = crc32c(&block[..n - 4]);
    block[n - 4..].copy_from_slice(&crc.to_le_bytes());
}

/// An unfinished piece of chunks of tracks 0 and 1 and record blocks, with a checkpoint every 1 KiB; its bytes.
fn unfinished() -> Vec<u8> {
    let mut w = TapeWriter::create_on(MemSink::new(Path::new("open")), SyncPolicy::None);
    let tape = w.sink().tape();
    w.set_index_policy(IndexPolicy {
        bytes: 1024,
        interval: None,
        ..IndexPolicy::default()
    })
    .unwrap();
    let defs = base_defs();
    for k in 0..12 {
        let (h, b) = chunk(k % 2, k / 2);
        w.write_chunk(defs, &h, &column(&b)).unwrap();
        let id = w.next_id();
        w.write_block(defs, &records(id, k as i64)).unwrap();
    }
    tape.to_vec().unwrap()
}

/// The session and the seq after the last counted block of `bytes`.
fn next_id(bytes: &[u8]) -> BlockId {
    let mut rd = FileBlocks::new(bytes);
    let mut last = None;
    while let Some(b) = rd.next_block() {
        let b = b.unwrap();
        last = Some((b.session, b.seq));
    }
    let (session, seq) = last.unwrap();
    BlockId::new(session, seq + 1)
}

/// A good block of `kind` with a body of `body` bytes, as block `id`.
fn raw_block(kind: u8, id: BlockId, body: usize) -> Vec<u8> {
    let len = 32 + body + 4;
    let mut b = vec![0u8; len];
    b[..4].copy_from_slice(b"WWTP");
    b[4] = 1;
    b[5] = kind;
    b[8..12].copy_from_slice(&(len as u32).to_le_bytes());
    b[16..24].copy_from_slice(&id.seq.to_le_bytes());
    b[24..28].copy_from_slice(&id.session.get().to_le_bytes());
    for (i, x) in b[32..32 + body].iter_mut().enumerate() {
        *x = i as u8;
    }
    reseal(&mut b);
    b
}

/// Track `t`'s range hash over all its items, as `pieces::chunk` numbers it.
fn hash_of<S: ww_tape::reader::ByteSource>(r: &TapeReader<S>, t: u64) -> ww_tape::RangeHash {
    r.range_hash(
        crate::pieces::chunk_id(t),
        OwnerId::NONE,
        0..u64::MAX,
        TrackHash::START,
    )
    .unwrap()
}

/// Blocks of the reserved kinds 11 and 12 (a relation's postings and dictionary) in a piece are blocks like any:
/// they read, the index lists them, the blocks after them read, and they change no track's range hash.
#[test]
fn reserved_block_kinds_read_as_blocks_and_change_no_hash() {
    let base = unfinished();
    let id = next_id(&base);
    let mut bytes = base.clone();
    bytes.extend(raw_block(11, id, 40));
    bytes.extend(raw_block(12, id.next(), 24));
    bytes.extend(records(id.next().next(), 99));
    let mut rd = FileBlocks::new(&bytes[..]);
    let mut kinds = vec![];
    while let Some(b) = rd.next_block() {
        kinds.push(b.unwrap().kind);
    }
    assert_eq!(
        kinds[kinds.len() - 3..],
        [
            BlockKind::from_u8(11),
            BlockKind::from_u8(12),
            BlockKind::Records
        ]
    );
    let r = TapeReader::open(&bytes[..]).unwrap();
    assert_eq!(r.pieces().len(), 1);
    assert_eq!(
        r.pieces()[0].end,
        bytes.len() as u64,
        "the whole piece reads"
    );
    let listed: Vec<_> = r.entries(0).unwrap().iter().map(|e| e.kind).collect();
    assert_eq!(
        listed[listed.len() - 3..],
        [
            BlockKind::from_u8(11),
            BlockKind::from_u8(12),
            BlockKind::Records
        ]
    );
    let before = TapeReader::open(&base[..]).unwrap();
    for t in 0..2 {
        assert_eq!(hash_of(&r, t), hash_of(&before, t), "track {t}");
    }
}

/// Header flag bits a reader doesn't know, and bit 3 (`FLAG_PAYLOAD_UNCHECKED`, a chunk's only), on a records block
/// and a checkpoint index block, CRCs good: the blocks read as without them.
#[test]
fn reserved_header_flags_on_other_blocks_are_not_read() {
    let base = unfinished();
    let mut bytes = base.clone();
    let mut rd = FileBlocks::new(&base[..]);
    let (mut records_at, mut index_at) = (None, None);
    while let Some(b) = rd.next_block() {
        let b = b.unwrap();
        let (kind, len) = (b.kind, b.bytes().len());
        // the offset of the block just read
        let at = rd.offset() as usize;
        match kind {
            BlockKind::Records => records_at = Some((at, len)),
            BlockKind::Index => index_at = Some((at, len)),
            _ => {}
        }
    }
    let flags = ww_tape::FLAG_PAYLOAD_UNCHECKED | 0x8000 | 0x0100;
    for (at, len) in [records_at.unwrap(), index_at.unwrap()] {
        let b = &mut bytes[at..at + len];
        let f = u16::from_le_bytes([b[6], b[7]]) | flags;
        b[6..8].copy_from_slice(&f.to_le_bytes());
        if b[5] == BlockKind::Index.to_u8() {
            let head_end = IndexBlock::head_len(b).unwrap();
            let head = crc::Crc::<u32>::new(&crc::CRC_32_ISCSI);
            let mut d = head.digest();
            d.update(&b[..28]);
            d.update(&b[32..head_end]);
            let crc = d.finalize();
            b[28..32].copy_from_slice(&crc.to_le_bytes());
        }
        reseal(b);
    }
    let mut rd = FileBlocks::new(&bytes[..]);
    let mut n = 0;
    while let Some(b) = rd.next_block() {
        b.unwrap();
        n += 1;
    }
    let mut rd = FileBlocks::new(&base[..]);
    let mut m = 0;
    while let Some(b) = rd.next_block() {
        b.unwrap();
        m += 1;
    }
    assert_eq!(n, m);
    let (r, before) = (
        TapeReader::open(&bytes[..]).unwrap(),
        TapeReader::open(&base[..]).unwrap(),
    );
    assert_eq!(r.pieces()[0].end, before.pieces()[0].end);
    assert_eq!(r.entries(0).unwrap(), before.entries(0).unwrap());
    for t in 0..2 {
        assert_eq!(hash_of(&r, t), hash_of(&before, t), "track {t}");
    }
}

/// A checkpoint whose entries are longer than 120 bytes (a later writer's), the extra bytes not zero: a reader
/// takes each entry's first 120 bytes, and the piece reads as with 120-byte entries.
#[test]
fn index_entries_longer_than_a_reader_knows_read_end_to_end() {
    let base = unfinished();
    // the newest checkpoint, and the piece cut right after it
    let mut rd = FileBlocks::new(&base[..]);
    let mut last = None;
    while let Some(b) = rd.next_block() {
        let b = b.unwrap();
        if b.kind == BlockKind::Index {
            let block = b.bytes().to_vec();
            last = Some((rd.offset() as usize, block));
        }
    }
    let (at, block) = last.unwrap();
    let ix = IndexBlock::read(&ww_tape::Block::read(&block).unwrap()).unwrap();
    assert_eq!(ix.kind(), IndexKind::Checkpoint);
    let short = base[..at + block.len()].to_vec();
    let extra = 24;
    let entries_at = u32::from_le_bytes(block[32..36].try_into().unwrap()) as usize;
    let mut wide = block[..entries_at].to_vec();
    for e in block[entries_at..block.len() - 4].chunks(120) {
        wide.extend_from_slice(e);
        wide.extend(std::iter::repeat_n(0xa5, extra));
    }
    wide.extend([0; 4]);
    let len = wide.len() as u32;
    wide[8..12].copy_from_slice(&len.to_le_bytes());
    wide[58..60].copy_from_slice(&(120 + extra as u16).to_le_bytes());
    let head = crc::Crc::<u32>::new(&crc::CRC_32_ISCSI);
    let mut d = head.digest();
    d.update(&wide[..28]);
    d.update(&wide[32..entries_at]);
    let crc = d.finalize();
    wide[28..32].copy_from_slice(&crc.to_le_bytes());
    reseal(&mut wide);
    let mut long = base[..at].to_vec();
    long.extend(&wide);
    let (a, b) = (
        TapeReader::open(&short[..]).unwrap(),
        TapeReader::open(&long[..]).unwrap(),
    );
    assert!(matches!(
        b.pieces()[0].found,
        ww_tape::reader::Found::Checkpoint { .. }
    ));
    let (ea, eb) = (a.entries(0).unwrap(), b.entries(0).unwrap());
    assert_eq!(ea.len(), eb.len());
    // the index block itself is listed by neither; every block listed is the same
    assert_eq!(ea, eb);
    for t in 0..2 {
        assert_eq!(
            a.track_entries(0, crate::pieces::chunk_id(t)).unwrap(),
            b.track_entries(0, crate::pieces::chunk_id(t)).unwrap()
        );
        assert_eq!(hash_of(&a, t), hash_of(&b, t), "track {t}");
    }
}

/// A raw medium holding a chain's pieces appended in one file (an SD card's layout): the forward walk finds every
/// piece a file reader finds, each verifies, the chain is whole, and the range hashes are the same.
#[test]
fn a_raw_medium_reads_appended_pieces() {
    let options = ChainOptions {
        files: PieceFiles::One,
        writer: WriterOptions {
            sync: SyncPolicy::None,
            layout: Layout::UNALIGNED.on_sd_card(),
            index: IndexPolicy {
                bytes: 2048,
                interval: None,
                ..IndexPolicy::default()
            },
            producer: "reserved".to_owned(),
            ..WriterOptions::default()
        },
        cut: CutPolicy {
            chunks: 5,
            ..CutPolicy::default()
        },
    };
    let mem = MemTapes::new();
    let path = Path::new("card");
    let mut c =
        PieceChain::create_on(mem.clone(), path, OwnerId([8; 32]), 0, &[], options).unwrap();
    let defs = base_defs();
    for k in 0..16 {
        let (h, b) = chunk(k % 2, k / 2);
        c.write_chunk(defs, &h, &column(&b)).unwrap();
    }
    let done = c.finish().unwrap();
    assert!(done.len() >= 3, "{} pieces", done.len());
    let bytes = mem.get(&done[0].path).unwrap().unwrap().to_vec().unwrap();
    let file = TapeReader::open(&bytes[..]).unwrap();
    let raw = TapeReader::open_with(
        &bytes[..],
        OpenOptions {
            medium: Medium::Raw,
            ..OpenOptions::default()
        },
    )
    .unwrap();
    let starts = |r: &TapeReader<&[u8]>| {
        r.pieces()
            .iter()
            .map(|p| (p.start, p.end))
            .collect::<Vec<_>>()
    };
    assert_eq!(starts(&raw), starts(&file));
    assert_eq!(raw.pieces().len(), done.len());
    for i in 0..raw.pieces().len() {
        assert!(raw.verify(i).unwrap(), "piece {i}");
    }
    assert!(raw.chain().is_whole(), "{:?}", raw.chain());
    // the raw walk lists the blocks it reads, without their unit hashes: it reads the chunks, to the same hash
    for t in 0..2 {
        let (a, b) = (hash_of(&raw, t), hash_of(&file, t));
        assert_eq!(
            (a.hash, a.items, a.chunks),
            (b.hash, b.items, b.chunks),
            "track {t}"
        );
    }
}
