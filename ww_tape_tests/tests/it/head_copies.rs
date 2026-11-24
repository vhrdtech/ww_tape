//! The layout and the producer copied as track 0's definitions are (TRACK-6): a piece whose first blocks are lost
//! still names them, and a reader takes the gap rules from the copy on.

use shrink_wrap::prelude::*;
use ww_tape::TrackNum;
use ww_tape::index::IndexPolicy;
use ww_tape::piece::{OwnerId, PieceHead, PieceId, PieceIdent};
use ww_tape::writer::NearPolicy;
use ww_tape::{
    Describe, FileBlocks, FileError, Layout, Medium, MemSink, SessionTag, Slot, SyncPolicy,
    TapeReader, TapeWriter,
};

use crate::definitions::{base_defs, on_tracks, record_blocks, writes_of};

/// The producer the pieces name.
const PRODUCER: &str = "head copies";

/// A first piece of `blocks` record blocks of track 9 under `layout`, copies `copy_spacing` apart, checkpoints every
/// 2 KiB; closed with `close`, else left as a crash leaves it, its last blocks after the newest checkpoint.
fn piece(copy_spacing: u64, layout: Layout, blocks: i64, close: bool) -> Vec<u8> {
    piece_with(copy_spacing, layout, blocks, close, IndexPolicy::default())
}

/// [piece] under `index`'s caps.
fn piece_with(
    copy_spacing: u64,
    layout: Layout,
    blocks: i64,
    close: bool,
    index: IndexPolicy,
) -> Vec<u8> {
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    let tape = w.sink().tape();
    w.set_index_policy(IndexPolicy {
        bytes: exact_units::Bytes::new(2048),
        interval: None,
        ..index
    })
    .unwrap();
    w.set_near_policy(NearPolicy {
        copy_spacing: exact_units::Bytes::new(copy_spacing),
        ..NearPolicy::default()
    });
    w.set_layout(layout).unwrap();
    w.set_producer(PRODUCER);
    let owner = OwnerId([0x4c; 32]);
    w.begin_piece(
        &PieceHead {
            ident: PieceIdent {
                id: PieceId::first(&owner),
                owner,
                seq: 0,
            },
            prev: None,
            prev_session: SessionTag::NONE,
            integrity: ww_tape::Integrity::Full,
            unlisted_keys_unknown: false,
        },
        &[],
    )
    .unwrap();
    for k in 0..blocks {
        if !close && k + 5 == blocks {
            w.set_index_policy(IndexPolicy::NONE).unwrap();
        }
        w.write_block(
            base_defs(),
            &on_tracks(w.next_id(), &[(9, k, &[k as u8; 40])]),
        )
        .unwrap();
    }
    if close {
        w.close().unwrap();
    }
    tape.to_vec().unwrap()
}

/// Where the first record block of data starts: what lies before it is the piece head and its puts.
fn first_data(bytes: &[u8]) -> usize {
    record_blocks(bytes)
        .iter()
        .find(|b| !b.repeat)
        .unwrap()
        .offset as usize
}

/// `bytes` with everything before the first block of data zeroed, as lost sectors read.
fn head_lost(bytes: &[u8]) -> Vec<u8> {
    let mut d = bytes.to_vec();
    d[..first_data(bytes)].fill(0);
    d
}

/// Every call that puts a record uses the layout and the producer: each gets a copy at least the copy spacing after
/// its first record, as track 0's definitions do, and none with the spacing never reached.
#[test]
fn the_layout_and_the_producer_are_copied_past_the_spacing() {
    let bytes = piece(1024, Layout::UNALIGNED, 100, true);
    for slot in [Slot::Layout, Slot::Producer] {
        let at = writes_of(&bytes, slot);
        assert_eq!(
            at.len(),
            2,
            "{slot:?}: the first record and one copy: {at:?}"
        );
        assert!(at[1] - at[0] >= 1024, "{slot:?}: {at:?}");
    }
    let once = piece(u64::MAX, Layout::UNALIGNED, 100, true);
    for slot in [Slot::Layout, Slot::Producer] {
        assert_eq!(writes_of(&once, slot).len(), 1, "{slot:?}");
    }
    assert!(
        TapeReader::open(&bytes[..])
            .unwrap()
            .verify(0)
            .unwrap()
            .holds
    );
}

/// A piece whose head and first puts are lost names its layout and its producer from their copies, as it does its
/// track; without copies they are all lost with the first blocks.
#[test]
fn a_piece_whose_first_blocks_are_lost_names_its_layout_and_producer() {
    for (spacing, found) in [(1024, true), (u64::MAX, false)] {
        let damaged = head_lost(&piece(spacing, Layout::UNALIGNED, 100, true));
        let r = TapeReader::open(&damaged[..]).unwrap();
        assert!(
            !r.verify(0).unwrap().holds,
            "the content hash finds the loss"
        );
        let at = r.definitions_at(0, u64::MAX).unwrap();
        for slot in [
            Slot::Layout,
            Slot::Producer,
            Slot::Track {
                number: TrackNum::new(9),
            },
        ] {
            assert_eq!(
                at.items.contains_key(&slot),
                found,
                "spacing {spacing}: {slot:?}"
            );
        }
        if found {
            let item = |slot| Describe::from_ww_bytes(&at.items[&slot].item).unwrap();
            assert_eq!(item(Slot::Layout), Describe::Layout(Layout::UNALIGNED));
            assert!(matches!(item(Slot::Producer), Describe::Producer(p) if p.name == PRODUCER));
        }
    }
}

/// `bytes` with a blank sector put in before the record block of data `nth` from the end, and that sector's
/// offset and length.
fn blank_before_last(bytes: &[u8], nth: usize) -> (Vec<u8>, u64, u64) {
    let data: Vec<u64> = record_blocks(bytes)
        .iter()
        .filter(|b| !b.repeat)
        .map(|b| b.offset)
        .collect();
    let at = data[data.len() - nth] as usize;
    let mut out = bytes[..at].to_vec();
    out.extend_from_slice(&[0u8; 512]);
    out.extend_from_slice(&bytes[at..]);
    (out, at as u64, 512)
}

/// An unclosed piece whose first blocks are lost is read from its newest checkpoint: a blank sector after it is a gap
/// the data goes on after, by the gap rules of the layout's copy, where the layout allows gaps; without a copy those
/// rules are lost and the sector ends the piece.
#[test]
fn the_gap_rules_come_from_the_layouts_copy() {
    let sd = Layout::UNALIGNED.on_sd_card();
    for (spacing, gap) in [(1024, true), (u64::MAX, false)] {
        let (file, offset, len) = blank_before_last(&piece(spacing, sd, 100, false), 2);
        let file = head_lost(&file);
        let r = TapeReader::open(&file[..]).unwrap();
        let p = &r.pieces()[0];
        assert!(
            matches!(p.found, ww_tape::reader::Found::Checkpoint { .. }),
            "{:?}",
            p.found
        );
        match gap {
            true => assert_eq!(
                (p.gaps.as_slice(), p.cut, p.end),
                (&[(offset, len)][..], None, file.len() as u64),
                "spacing {spacing}"
            ),
            false => assert!(
                p.gaps.is_empty() && p.end == offset,
                "spacing {spacing}: {p:?}"
            ),
        }
    }
}

/// The gaps of a Prefix walk of a raw medium `card` from `from` on, a medium layer's hint, with the gap rules of
/// `layout` until a layout record says them; and where the last block it read starts.
fn raw_walk(card: &[u8], from: usize, layout: Option<&Layout>) -> (Vec<(u64, u64)>, u64) {
    let mut rd = FileBlocks::new(&card[from..])
        .on(Medium::Raw)
        .starting_at(from as u64);
    if let Some(l) = layout {
        rd = rd.with_layout(l);
    }
    let (mut gaps, mut last) = (vec![], 0);
    while let Some(b) = rd.next_block() {
        match b {
            Err(FileError::Gap { offset, len }) => gaps.push((offset, len)),
            Err(e) => panic!("{e:?}"),
            Ok(_) => last = rd.offset(),
        }
    }
    (gaps, last)
}

/// A raw walk of a piece whose first blocks are lost starts past them, at a medium layer's hint, knowing no gap rules:
/// it takes them at the layout's copy, so a blank sector after the copy is a gap. Before the copy only the rules the
/// medium layer gives read past one; without them the blank sector ends the data.
#[test]
fn a_raw_walk_takes_the_gap_rules_at_the_copy() {
    let sd = Layout::UNALIGNED.on_sd_card();
    let bytes = piece(1024, sd, 100, true);
    let from = first_data(&bytes);
    let copy = writes_of(&bytes, Slot::Layout)[1] as usize;
    let (card, offset, len) = blank_before_last(&bytes, 2);
    assert!(offset as usize > copy);
    let end = record_blocks(&bytes).last().unwrap().offset;
    let (gaps, last) = raw_walk(&card, from, None);
    assert_eq!(gaps, [(offset, len)]);
    assert!(last > end, "the walk reads on to the close");
    let early = record_blocks(&bytes)
        .iter()
        .find(|b| !b.repeat && b.offset > from as u64)
        .unwrap()
        .offset as usize;
    assert!(early < copy);
    let mut card = bytes[..early].to_vec();
    card.extend_from_slice(&[0u8; 512]);
    card.extend_from_slice(&bytes[early..]);
    let (gaps, last) = raw_walk(&card, from, None);
    assert!(
        gaps.is_empty() && last < early as u64,
        "the blank sector ends the data"
    );
    let (gaps, last) = raw_walk(&card, from, Some(&sd));
    assert_eq!(gaps, [(early as u64, 512)]);
    assert!(last > end);
}

/// A piece whose state entries are full writes no copy: the call that would carry it is never refused for it, and the
/// piece holds the layout and the producer once.
#[test]
fn a_full_piece_skips_the_copy_and_refuses_nothing() {
    // the layout, the producer, the clock and track 9, and the copies of those two: no room for the copies of the
    // layout and the producer
    let full = IndexPolicy {
        state_entries_max: exact_units::Count::new(6),
        ..IndexPolicy::default()
    };
    let bytes = piece_with(1024, Layout::UNALIGNED, 100, true, full);
    for slot in [Slot::Layout, Slot::Producer] {
        assert_eq!(writes_of(&bytes, slot).len(), 1, "{slot:?}");
    }
    let r = TapeReader::open(&bytes[..]).unwrap();
    let data = record_blocks(&bytes).iter().filter(|b| !b.repeat).count();
    assert_eq!(data, 100, "every block of data written");
    assert!(r.verify(0).unwrap().holds);
}

/// A piece written by the writer before TRACK-6 (`data/before-head-copies/`, the recipe `piece(1024,
/// Layout::UNALIGNED, 100, true)` at main 3be9dcb, its track definitions in the TRACK-4 shape): it reads and verifies
/// as before, its layout and producer put once; the same recipe now adds their copies.
#[test]
fn a_piece_written_before_head_copies_reads() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/it/data/before-head-copies/head-copies.wwt");
    let bytes = std::fs::read(path).unwrap();
    let r = TapeReader::open(&bytes[..]).unwrap();
    assert_eq!(r.pieces().len(), 1);
    assert!(r.verify(0).unwrap().holds);
    let at = r.definitions_at(0, u64::MAX).unwrap();
    for slot in [
        Slot::Layout,
        Slot::Producer,
        Slot::Track {
            number: TrackNum::new(9),
        },
    ] {
        assert!(at.items.contains_key(&slot), "{slot:?}");
    }
    for slot in [Slot::Layout, Slot::Producer] {
        assert_eq!(writes_of(&bytes, slot).len(), 1, "{slot:?}");
    }
    let now = piece(1024, Layout::UNALIGNED, 100, true);
    assert_ne!(bytes, now);
    assert_eq!(
        bytes[..first_data(&bytes)],
        now[..first_data(&now)],
        "the head and its puts are the same"
    );
}
