//! The ww_tape file reader on any bytes: it never panics, every block it gives is a good one (or, in the salvage walk,
//! a chunk with a good head), and the salvage walk gives every block of the prefix walk, in order. Describe records of a good block decode or fail, never panic.
//! A good records block coded with each body codec decodes to the same records.
//! Walked as a raw medium with the gap rules of an SD card too: every step still consumes bytes, and the salvage walk
//! gives every block of the prefix walk there too. A content-hash walk ([TapeReader::verify]) of each piece reads a
//! bounded multiple of the piece, never its square.
use shrink_wrap::DeserializeShrinkWrap;
use std::cell::Cell;
use ww_tape::index::is_structural;
use ww_tape::piece::{WALK_MISS_BASE_MAX, WALK_MISS_PER_BYTE_MAX};
use ww_tape::reader::{ByteSource, OutOfRange};
use ww_tape::{
    Block, BodyCodec, Describe, FileBlocks, FileError, Layout, Medium, Recovery, TapeReader,
    code_body, coded_len_max,
};

/// Bytes counted as a reader reads them.
struct Counted<'a> {
    bytes: &'a [u8],
    read: Cell<u64>,
}

impl ByteSource for Counted<'_> {
    type Error = OutOfRange;

    fn size(&self) -> Result<u64, OutOfRange> {
        self.bytes.size()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), OutOfRange> {
        self.read.set(self.read.get() + buf.len() as u64);
        self.bytes.read_at(offset, buf)
    }
}

/// Verify every piece of `data`: each walk reads its piece once, a header per place, the misses' budget and one
/// miss past it, at most.
fn verify_reads_linear(data: &[u8]) {
    let src = Counted {
        bytes: data,
        read: Cell::new(0),
    };
    let Ok(r) = TapeReader::open(&src) else {
        return;
    };
    for (i, p) in r.pieces().iter().enumerate() {
        let n = p.end.saturating_sub(p.start);
        src.read.set(0);
        let _ = r.verify(i);
        let bound = (3 + WALK_MISS_PER_BYTE_MAX) * n + WALK_MISS_BASE_MAX;
        assert!(
            src.read.get() <= bound,
            "verify of piece {i} read {} bytes over {n}, at most {bound}",
            src.read.get()
        );
    }
}

pub(crate) fn walk(data: &[u8], recovery: Recovery, medium: Medium) -> Vec<Vec<u8>> {
    let mut rd = FileBlocks::with_recovery(data, recovery).on(medium);
    if medium == Medium::Raw {
        rd = rd.with_layout(&Layout::UNALIGNED.on_sd_card());
    }
    let mut blocks = vec![];
    let mut steps = 0usize;
    // Resync: the data blocks handed out by session and seq, where each starts (far fewer than RESYNC_SEQS_MAX here)
    let mut ids = std::collections::BTreeMap::new();
    while let Some(b) = rd.next_block() {
        let taken = match &b {
            Ok(b) if recovery == Recovery::Resync && !is_structural(b.kind) && !b.is_repeat() => {
                Some(((b.session, b.seq), b.len(), b.crc()))
            }
            _ => None,
        };
        if let Err(FileError::Conflict {
            first,
            session,
            seq,
            block,
            ..
        }) = &b
        {
            // the conflict names a block of its id handed out before, with other bytes
            let got = Block::read(block).map(|b| (b.len(), b.crc()));
            let (at, len, crc) = *ids
                .get(&(*session, *seq))
                .expect("a conflict is of an id handed out before");
            assert_eq!(at, *first, "a conflict names the block handed out first");
            assert_ne!(got.ok(), Some((len, crc)), "a conflict is of other bytes");
        }
        if let Ok(b) = &b {
            let bytes = b.bytes().to_vec();
            if b.is_intact() {
                assert!(Block::read(&bytes).is_ok());
            } else {
                assert_eq!(recovery, Recovery::Resync);
                assert!(Block::salvage(&bytes).is_ok_and(|b| !b.is_intact()));
            }
            let mut plain = Vec::new();
            let decoded = b.decode(&mut plain);
            for r in decoded.iter().flat_map(|d| d.records()).flatten() {
                if r.at.is_none() {
                    _ = Describe::from_ww_bytes(r.item);
                }
            }
            round_trip(b);
            blocks.push(bytes);
        } else if let Err(FileError::Conflict { block, .. }) = b {
            // a second good block of a seq with other bytes: reported, read, never handed out as a block
            assert_eq!(recovery, Recovery::Resync);
            blocks.push(block);
        }
        if let Some((id, len, crc)) = taken {
            // a block of an id handed out before with other bytes is a conflict, never handed out
            let first = *ids.entry(id).or_insert((rd.offset(), len, crc));
            assert_eq!(
                (first.1, first.2),
                (len, crc),
                "a Resync walk hands out {id:?} twice with other bytes"
            );
        }
        steps += 1;
        assert!(steps <= data.len() + 1, "every step consumes bytes");
    }
    blocks
}

/// Code a plain records block's body with each codec: it reads back to the same records.
fn round_trip(b: &Block<'_>) {
    if b.body_codec().is_some() || !b.is_intact() {
        return;
    }
    let records: Vec<_> = b.records().collect();
    for codec in [BodyCodec::STORED, BodyCodec::LZ4] {
        let mut out = vec![0u8; coded_len_max(b.len())];
        let Ok(n) = code_body(b.bytes(), codec, &mut out) else {
            continue;
        };
        let coded = Block::read(&out[..n]).expect("a coded block reads");
        assert_eq!(
            (coded.seq, coded.session, coded.record_count),
            (b.seq, b.session, b.record_count)
        );
        let mut plain = Vec::new();
        let decoded = coded.decode(&mut plain).expect("a coded block decodes");
        assert_eq!(decoded.records().collect::<Vec<_>>(), records);
    }
}

pub fn reader(data: &[u8]) {
    verify_reads_linear(data);
    for medium in [Medium::File, Medium::Raw] {
        let prefix = walk(data, Recovery::Prefix, medium);
        let salvage = walk(data, Recovery::Resync, medium);
        // the salvage walk loses no block the prefix walk reads; it reads more: past damage, every block of a seq
        // where the prefix walk keeps the later of two (Recovery::Resync has no seq check), and blocks inside a gap
        // the prefix walk steps over to the next resume place
        let mut rest = salvage.iter();
        for b in &prefix {
            assert!(
                rest.any(|s| s == b),
                "the salvage walk misses a block the prefix walk reads"
            );
        }
    }
}
