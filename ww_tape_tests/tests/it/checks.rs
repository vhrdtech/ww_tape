//! What a writer refuses of a caller's blocks and settings, and what a reader refuses of a file's version.

use crate::definitions::{base, begin, on_tracks, options, track_id, write_records};
use crate::pieces::{chunk, column};
use ww_tape::TrackNum;
use ww_tape::{
    ALIGNMENT, Describe, InputError, Layout, MemSink, SyncPolicy, TapeReader, TapeWriter,
    WriteError,
};

const CRC: crc::Crc<u32> = crc::Crc::<u32>::new(&crc::CRC_32_ISCSI);

/// `block` with header byte `at` set to `value`, its CRC made good again.
pub(crate) fn patched(block: &[u8], at: usize, value: u8) -> Vec<u8> {
    let mut b = block.to_vec();
    b[at] = value;
    let n = b.len();
    let crc = CRC.checksum(&b[..n - 4]);
    b[n - 4..].copy_from_slice(&crc.to_le_bytes());
    b
}

/// `bytes` with every block made one of `version`.
pub(crate) fn of_version(bytes: &[u8], version: u8) -> Vec<u8> {
    let mut out = Vec::new();
    while out.len() < bytes.len() {
        let at = out.len();
        let n = ww_tape::block_len(bytes[at..at + 32].try_into().unwrap()).unwrap();
        out.extend(patched(&bytes[at..at + n], 4, version));
    }
    out
}

/// A caller's block of the reserved kind 2, of another version, of a kind the writer makes itself, or with a body
/// of a codec this writer doesn't know is refused without a byte written (writing side point 9).
#[test]
fn a_callers_block_the_writer_does_not_know_is_refused() {
    let mut w = begin(options(u64::MAX));
    write_records(&mut w, 0, 1);
    let tape = w.sink().tape();
    let len = tape.to_vec().unwrap().len();
    let good = on_tracks(w.next_id(), &[(1, 5, b"x")]);
    let unknown_codec = {
        let mut b = good.clone();
        // a coded body whose codec byte is one no reader knows
        b[6] = 1;
        b[32] = 0xee;
        patched(&b, 6, 1)
    };
    for (what, block) in [
        ("kind 2", patched(&good, 5, ww_tape::RESERVED_KIND)),
        ("version 2", patched(&good, 4, ww_tape::VERSION + 1)),
        ("an index", patched(&good, 5, 9)),
        ("a codec it doesn't know", unknown_codec),
    ] {
        let e = w.write_block(&block).unwrap_err();
        assert!(
            matches!(
                e,
                WriteError::Input(InputError::Unknown { .. })
                    | WriteError::Block(_)
                    | WriteError::Index(_)
            ),
            "{what}: {e:?}"
        );
    }
    assert_eq!(tape.to_vec().unwrap().len(), len, "nothing written");
    w.write_block(&good).unwrap();
}

/// `block` with header flags `flags`, its CRCs made good again: a chunk's head CRC (bytes 0..28 and 32 up to the
/// payload) too.
fn flagged(block: &[u8], flags: u16) -> Vec<u8> {
    let mut b = block.to_vec();
    b[6..8].copy_from_slice(&flags.to_le_bytes());
    if matches!(b[5], 3 | 4) {
        let payload_at = u32::from_le_bytes(b[32..36].try_into().unwrap()) as usize;
        let mut d = CRC.digest();
        d.update(&b[..28]);
        d.update(&b[32..payload_at]);
        let head = d.finalize();
        b[28..32].copy_from_slice(&head.to_le_bytes());
    }
    let n = b.len();
    let crc = CRC.checksum(&b[..n - 4]);
    b[n - 4..].copy_from_slice(&crc.to_le_bytes());
    b
}

/// A caller's records or chunk block with a header flag its kind doesn't take (reserved bits, `FLAG_WITH_NEXT`
/// without `FLAG_REPEAT`, `FLAG_PAYLOAD_UNCHECKED`) is refused without a byte written, by every call taking one. A
/// chunk of a unit (`FLAG_WITH_NEXT`) is written and adopted (`relayed_units_resume_without_duplicates`), never
/// deferred.
#[test]
fn a_callers_block_with_flags_its_kind_does_not_take_is_refused() {
    let mut w = begin(options(u64::MAX));
    write_records(&mut w, 0, 1);
    let tape = w.sink().tape();
    let len = tape.to_vec().unwrap().len();
    let records = on_tracks(w.next_id(), &[(1, 5, b"x")]);
    let (mut head, starts) = chunk(0, 0);
    head.track_id = track_id(1);
    let cols = column(&starts);
    let mut chunk_block = vec![0u8; ww_tape::chunk::chunk_len(&head, &[cols[0].0], 0)];
    let n = ww_tape::chunk::write_chunk(&mut chunk_block, w.next_id(), &head, &cols, 0).unwrap();
    chunk_block.truncate(n);
    for (what, block) in [("records", &records), ("chunk", &chunk_block)] {
        let theirs = u16::from_le_bytes([block[6], block[7]]);
        for bit in [2u16, 3, 4, 9, 15] {
            let b = flagged(block, theirs | 1 << bit);
            // a chunk of a unit is refused only where it would be deferred
            let calls = if what == "chunk" && bit == 2 {
                2..3
            } else {
                0..3
            };
            for call in calls {
                let r = match call {
                    0 => w.write_block(&b).map(|_| ()),
                    1 => w.w.adopt_block(&w.defs, &b).map(|_| ()),
                    _ => w.defer_block(b.clone()),
                };
                assert!(
                    matches!(r, Err(WriteError::Input(InputError::Flags { flags, .. })) if flags == theirs | 1 << bit)
                        || (what == "chunk" && bit == 3 && matches!(r, Err(WriteError::Block(_)))),
                    "{what}, bit {bit}, call {call}: {r:?}"
                );
            }
        }
    }
    assert_eq!(tape.to_vec().unwrap().len(), len, "nothing written");
    w.write_block(&records).unwrap();
    w.write_block(&chunk_block).unwrap();
}

/// The layout and the producer are the writer's own: put first, after the piece head or before the first block of a
/// file without one; another layout once a block is written is refused, and a container of definitions refuses them
/// (writing side review 2, point 7).
#[test]
fn the_layout_and_the_producer_are_the_writers() {
    for alignment in [0, ALIGNMENT] {
        let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
        w.set_alignment(alignment).unwrap();
        w.set_producer("checks");
        let tape = w.sink().tape();
        let mut defs = ww_tape::Definitions::new();
        crate::definitions::hold(&mut defs, &base(Layout::UNALIGNED)[2..]).unwrap();
        assert_eq!(
            crate::definitions::hold(
                &mut defs,
                core::slice::from_ref(&Describe::Layout(Layout::ALIGNED))
            ),
            Err(ww_tape::DescribeError::WriterOwned)
        );
        w.write_block(&defs, &on_tracks(w.next_id(), &[(1, 0, b"x")]))
            .unwrap();
        let flipped = if alignment == 0 { ALIGNMENT } else { 0 };
        assert!(matches!(
            w.set_alignment(flipped),
            Err(WriteError::Input(InputError::Layout { .. }))
        ));
        let bytes = tape.to_vec().unwrap();
        let first = &crate::definitions::record_blocks(&bytes)[0];
        let b = ww_tape::Block::read(&first.bytes).unwrap();
        let slots: Vec<ww_tape::Slot> = b
            .records()
            .map(|r| {
                let r = r.unwrap();
                ww_tape::Slot::of(r.track, r.item, b.is_keyed()).unwrap()
            })
            .collect();
        assert_eq!(slots, [ww_tape::Slot::Layout, ww_tape::Slot::Producer]);
        let r = TapeReader::open(&bytes[..]).unwrap();
        let layout = r
            .state_at(0, ww_tape::Slot::Layout, u64::MAX)
            .unwrap()
            .item
            .unwrap();
        let Ok(Describe::Layout(l)) =
            <Describe<'_> as shrink_wrap::DeserializeShrinkWrap>::from_ww_bytes(&layout.item)
        else {
            panic!("a layout");
        };
        assert_eq!(l.alignment.get(), alignment);
    }
}

/// A resume checks the layout and the producer of the options against those the piece names; a piece whose layout
/// record didn't survive takes the options' (writing side review 2, point 12).
#[test]
fn a_resume_checks_the_layout_and_producer_it_is_given() {
    let mut w = begin(options(u64::MAX));
    w.set_index_policy(ww_tape::index::IndexPolicy {
        bytes: exact_units::Bytes::new(512),
        interval: None,
        ..ww_tape::index::IndexPolicy::default()
    })
    .expect("a policy with room for a piece head");
    let tape = w.sink().tape();
    for k in 0..40 {
        w.write_block(&on_tracks(w.next_id(), &[(1, k, &[k as u8; 30])]))
            .unwrap();
    }
    let bytes = tape.to_vec().unwrap();
    drop(w);
    let resume = |bytes: &[u8], o: ww_tape::WriterOptions| {
        let mut sink = MemSink::new();
        ww_tape::TapeSink::write_all(&mut sink, bytes).unwrap();
        TapeWriter::resume_on(sink, o)
    };
    let mut o = options(u64::MAX);
    o.index.bytes = exact_units::Bytes::new(512);
    let (_, resumed) = resume(&bytes, o.clone()).unwrap();
    assert_eq!(resumed.layout, Some(Layout::UNALIGNED));
    let mut aligned = o.clone();
    aligned.layout = Layout::ALIGNED;
    assert!(matches!(
        resume(&bytes, aligned.clone()),
        Err(WriteError::Input(InputError::Layout { .. }))
    ));
    let mut other = o.clone();
    other.producer = "another".to_owned();
    assert!(matches!(
        resume(&bytes, other),
        Err(WriteError::Input(InputError::Producer { .. }))
    ));
    // the block of the layout and the producer damaged: the checkpoints still list it, the options say the layout
    let first = crate::definitions::record_blocks(&bytes)[0].offset as usize;
    let mut damaged = bytes.clone();
    damaged[first + 40] ^= 1;
    let (_, resumed) = resume(&damaged, aligned).unwrap();
    assert_eq!(resumed.layout, None);
}

/// A piece of another format version is reported where the reader meets it: alone in a file, or after a piece of
/// this version (reading side review 2, point 7); its lookups name the version, the piece before it reads.
#[test]
fn a_piece_of_another_version_is_reported_where_it_is_met() {
    let mut w = begin(options(u64::MAX));
    let tape = w.sink().tape();
    write_records(&mut w, 0, 2);
    w.w.close().unwrap();
    let ours = tape.to_vec().unwrap();
    // every block of another version, the tail too
    let theirs = of_version(&ours, ww_tape::VERSION + 1);
    let other = |r: &TapeReader<&[u8]>, i: usize| match r.entries(i) {
        Err(ww_tape::reader::ReadError::Version { found, offset }) => {
            assert_eq!(found, ww_tape::VERSION + 1);
            assert_eq!(offset, r.pieces()[i].start);
        }
        e => panic!("piece {i} of another version: {e:?}"),
    };
    // alone
    let r = TapeReader::open(&theirs[..]).unwrap();
    assert_eq!(r.pieces().len(), 1);
    assert_eq!(r.pieces()[0].version, Some(ww_tape::VERSION + 1));
    other(&r, 0);
    // after a closed piece of this version
    let both = [ours.clone(), theirs].concat();
    let r = TapeReader::open(&both[..]).unwrap();
    assert_eq!(r.pieces().len(), 2);
    assert_eq!(r.pieces()[0].version, None);
    assert!(r.verify(0).map(|v| v.holds).unwrap());
    assert_eq!(r.pieces()[1].start, ours.len() as u64);
    other(&r, 1);
}

/// A piece lists at most `state_entries_max` state entries, definitions and items alike: a put past it is refused
/// with nothing written, naming the cap and its use (writing side review 2, point 8).
#[test]
fn state_entries_are_capped() {
    let five = [crate::definitions::state(5)];
    let mut w = crate::definitions::begin_numbered(options(u64::MAX), Layout::UNALIGNED, &five);
    crate::definitions::define_all(&mut w, &five);
    let tape = w.sink().tape();
    let defs = w.defs.clone();
    assert!(w.w.put(&defs, TrackNum::new(5), 0, &[0]).unwrap());
    let used = w.state_entries();
    assert!(used > 1, "the definitions are listed too");
    w.set_index_policy(ww_tape::index::IndexPolicy {
        state_entries_max: exact_units::Count::new(u64::try_from(used).unwrap() + 2),
        ..ww_tape::index::IndexPolicy::default()
    })
    .expect("a policy with room for a piece head");
    for k in 1..3u8 {
        assert!(
            w.w.put(&defs, TrackNum::new(5), i64::from(k), &[k])
                .unwrap()
        );
    }
    assert_eq!(w.state_entries(), used + 2);
    let len = tape.to_vec().unwrap().len();
    match w.w.put(&defs, TrackNum::new(5), 3, &[3]) {
        Err(WriteError::Index(ww_tape::IndexError::StateEntries { max, used: now })) => {
            assert_eq!((max, now), (used + 2, used + 2));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(tape.to_vec().unwrap().len(), len, "nothing written");
}
