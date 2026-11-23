//! Whether a reader checks the CRCs over payloads ([OpenOptions::check_payloads]): by default a memory tape a writer
//! of this process wrote is read unchecked and a file checked; either can be asked for.

use ww_tape::{
    BlockError, BlockKind, Checked, IndexEntry, MemPieces, OpenOptions, PayloadCheck, PieceTapes,
    ReadError, TapeReader, TapeSink,
};

use crate::range_hash::write;

/// The bytes of a chain in one tape, and the first chunk entry of its first piece with that piece's start.
fn chain() -> (Vec<u8>, IndexEntry, u64) {
    let (_, bytes) = write(PieceTapes::One, 0xc4ec);
    let r = TapeReader::open(&bytes[0][..]).unwrap();
    let entry = r
        .entries(0)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == BlockKind::Events)
        .unwrap();
    let start = r.pieces()[0].start;
    (bytes[0].clone(), entry, start)
}

/// `bytes` as a closed memory tape.
fn in_memory(bytes: &[u8]) -> ww_tape::MemPiece {
    let tapes = MemPieces::new();
    let mut sink = tapes.create_whole(&0).unwrap();
    sink.write_all(bytes).unwrap();
    sink.close().unwrap();
    tapes.get(&0).unwrap().unwrap()
}

/// `bytes` with the last payload byte of the block of `entry` flipped: the head stays good.
fn flipped(bytes: &[u8], entry: &IndexEntry, start: u64) -> Vec<u8> {
    let mut out = bytes.to_vec();
    // the block ends with its CRC (4 bytes) after padding and columns: a byte before the padding is payload
    let at = (start + entry.offset) as usize + entry.len as usize - 5 - 64;
    out[at] ^= 0x10;
    out
}

fn options(check: PayloadCheck) -> OpenOptions {
    OpenOptions {
        check_payloads: check,
        ..OpenOptions::default()
    }
}

#[test]
fn a_memory_tape_reads_unchecked_and_a_file_checked() {
    let (bytes, entry, _) = chain();
    let mut buf = vec![];
    let mem = TapeReader::open(in_memory(&bytes)).unwrap();
    assert!(!mem.checks_payloads());
    let b = mem.read_block(0, &entry, &mut buf).unwrap();
    assert_eq!(b.checked(), Checked::Unverified);
    let file = TapeReader::open(&bytes[..]).unwrap();
    assert!(file.checks_payloads());
    let b = file.read_block(0, &entry, &mut buf).unwrap();
    assert_eq!(b.checked(), Checked::Verified);
}

/// A flipped payload byte is caught by a checking reader, memory or file, and passes an unchecked one: the bytes
/// read are those stored.
#[test]
fn a_flipped_payload_byte_is_caught_only_when_checked() {
    let (bytes, entry, start) = chain();
    let bad = flipped(&bytes, &entry, start);
    let mut buf = vec![];
    for check in [PayloadCheck::Always, PayloadCheck::BySource] {
        let file = TapeReader::open_with(&bad[..], options(check)).unwrap();
        assert!(matches!(
            file.read_block(0, &entry, &mut buf),
            Err(ReadError::Block {
                error: BlockError::BadCrc,
                ..
            })
        ));
    }
    let mem = TapeReader::open_with(in_memory(&bad), options(PayloadCheck::Always)).unwrap();
    assert!(matches!(
        mem.read_block(0, &entry, &mut buf),
        Err(ReadError::Block {
            error: BlockError::BadCrc,
            ..
        })
    ));
    let m = TapeReader::open(in_memory(&bad)).unwrap();
    assert_eq!(
        m.read_block(0, &entry, &mut buf).unwrap().checked(),
        Checked::Unverified
    );
    let f = TapeReader::open_with(&bad[..], options(PayloadCheck::Never)).unwrap();
    assert_eq!(
        f.read_block(0, &entry, &mut buf).unwrap().checked(),
        Checked::Unverified
    );
}

/// An unchecked read still checks the head: a flipped head byte is refused.
#[test]
fn an_unchecked_read_still_checks_the_head() {
    let (bytes, entry, start) = chain();
    let mut bad = bytes.clone();
    // inside the chunk head, past the 32-byte block header
    bad[(start + entry.offset) as usize + 40] ^= 0x01;
    let mem = TapeReader::open(in_memory(&bad)).unwrap();
    let mut buf = vec![];
    assert!(matches!(
        mem.read_block(0, &entry, &mut buf),
        Err(ReadError::Block { .. })
    ));
}
