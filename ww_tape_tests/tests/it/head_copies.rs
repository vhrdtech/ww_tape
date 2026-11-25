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

/// A first piece of `blocks` record blocks of track 9 under `layout`, copies `copy_spacing` apart, the first too,
/// checkpoints every 2 KiB; closed with `close`, else left as a crash leaves it, its last blocks after the newest checkpoint.
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
    let spacing = exact_units::Bytes::new(copy_spacing);
    let near = NearPolicy {
        copy_first: spacing,
        copy_spacing: spacing,
        ..NearPolicy::default()
    };
    piece_near(near, layout, blocks, close, index)
}

/// [piece] under `near` and `index`'s caps.
fn piece_near(
    near: NearPolicy,
    layout: Layout,
    blocks: i64,
    close: bool,
    index: IndexPolicy,
) -> Vec<u8> {
    let (mut w, tape) = begin(near, layout, index);
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

/// A writer on a first piece under `near`, `layout` and `index`'s caps, checkpoints every 2 KiB, and its tape.
fn begin(
    near: NearPolicy,
    layout: Layout,
    index: IndexPolicy,
) -> (TapeWriter<MemSink>, ww_tape::MemPiece) {
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    let tape = w.sink().tape();
    w.set_index_policy(IndexPolicy {
        bytes: exact_units::Bytes::new(2048),
        interval: None,
        ..index
    })
    .unwrap();
    w.set_near_policy(near);
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
    (w, tape)
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

/// Every call that puts a record uses the layout and the producer: each gets its two copies, each at least the copy
/// spacing after the record before it, as track 0's definitions do, and none with the spacing never reached.
#[test]
fn the_layout_and_the_producer_are_copied_past_the_spacing() {
    let bytes = piece(1024, Layout::UNALIGNED, 100, true);
    for slot in [Slot::Layout, Slot::Producer] {
        let at = writes_of(&bytes, slot);
        assert_eq!(
            at.len(),
            3,
            "{slot:?}: the first record and two copies: {at:?}"
        );
        assert!(
            at[1] - at[0] >= 1024 && at[2] - at[1] >= 1024,
            "{slot:?}: {at:?}"
        );
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
        if gap {
            assert_eq!(
                (p.gaps.as_slice(), p.cut, p.end),
                (&[(offset, len)][..], None, file.len() as u64),
                "spacing {spacing}"
            );
        } else {
            assert!(
                p.gaps.is_empty() && p.end == offset,
                "spacing {spacing}: {p:?}"
            );
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
/// piece holds the layout and the producer once, the clock and the track twice.
#[test]
fn a_full_piece_skips_the_copy_and_refuses_nothing() {
    // the layout, the producer, the clock and track 9, and the first copies of those two: no room for the copies of
    // the layout and the producer, nor for the second copies
    let full = IndexPolicy {
        state_entries_max: exact_units::Count::new(6),
        ..IndexPolicy::default()
    };
    let bytes = piece_with(1024, Layout::UNALIGNED, 100, true, full);
    for slot in [Slot::Layout, Slot::Producer] {
        assert_eq!(writes_of(&bytes, slot).len(), 1, "{slot:?}");
    }
    let track = Slot::Track {
        number: TrackNum::new(9),
    };
    assert_eq!(writes_of(&bytes, track).len(), 2);
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
    // the puts differ only by the layout's policy, which now asks for two copies
    let head = |b: &[u8]| u32::from_le_bytes(b[8..12].try_into().unwrap()) as usize;
    assert_eq!(
        bytes[..head(&bytes)],
        now[..head(&now)],
        "the head is the same"
    );
    assert_eq!(
        first_data(&bytes),
        first_data(&now),
        "the puts take the same bytes"
    );
}

/// Under the default policies a piece far under 1 MiB, its first 4 KiB lost, still names every definition, and a raw
/// walk from past the loss with no gap rules of a medium layer takes them at the layout's first copy, 4 KiB on: a
/// blank sector after it is a gap the data goes on after.
#[test]
fn the_first_4_kib_lost_leave_every_definition_and_the_gap_rules() {
    let sd = Layout::UNALIGNED.on_sd_card();
    let bytes = piece_near(NearPolicy::default(), sd, 100, true, IndexPolicy::default());
    assert!(bytes.len() < 64 << 10, "{} bytes", bytes.len());
    let copy = writes_of(&bytes, Slot::Layout)[1] as usize;
    assert!(
        (4096..8192).contains(&copy),
        "the layout's first copy at {copy}"
    );
    let mut lost = bytes.clone();
    lost[..4096].fill(0);
    let whole = TapeReader::open(&bytes[..])
        .unwrap()
        .definitions_at(0, u64::MAX)
        .unwrap();
    let at = TapeReader::open(&lost[..])
        .unwrap()
        .definitions_at(0, u64::MAX)
        .unwrap();
    assert_eq!(at.items.len(), whole.items.len());
    assert!(whole.items.keys().all(|s| at.items.contains_key(s)));
    // the walk starts at the first block past the loss, as a medium layer's hint puts it
    let from = record_blocks(&bytes)
        .iter()
        .find(|b| b.offset >= 4096)
        .unwrap()
        .offset as usize;
    let (card, offset, len) = blank_before_last(&bytes, 2);
    assert!(offset as usize > copy);
    let mut card = card;
    card[..4096].fill(0);
    let (gaps, last) = raw_walk(&card, from, None);
    assert_eq!(gaps, [(offset, len)]);
    let end = record_blocks(&bytes).last().unwrap().offset;
    assert!(last > end, "the walk reads on to the close");
}

/// A deferred block writes no run, so no first copy without a use (format.md, "Puts and copies"): track 8, defined
/// by the first block and used by no deferred one, keeps its one record however far the deferred blocks of track 9
/// run past `copy_first`, past [DEFERRED_BYTES_MAX](ww_tape::writer::DEFERRED_BYTES_MAX) too, so that an aligned
/// writer places some at once; the next block written at once brings its copy.
#[test]
fn deferred_blocks_write_no_first_copies() {
    let track = Slot::Track {
        number: TrackNum::new(8),
    };
    for layout in [Layout::UNALIGNED, Layout::ALIGNED] {
        let (mut w, tape) = begin(NearPolicy::default(), layout, IndexPolicy::default());
        w.write_block(
            base_defs(),
            &on_tracks(w.next_id(), &[(8, 0, &[0; 40]), (9, 0, &[0; 40])]),
        )
        .unwrap();
        for k in 1..400 {
            w.defer_block(
                base_defs(),
                on_tracks(w.next_id(), &[(9, k, &[k as u8; 4000])]),
            )
            .unwrap();
        }
        w.flush_deferred().unwrap();
        let bytes = tape.to_vec().unwrap();
        assert!(
            bytes.len() > 16 << 10,
            "{layout:?}: the case: {} bytes past the first block",
            bytes.len()
        );
        assert_eq!(writes_of(&bytes, track).len(), 1, "{layout:?}");
        w.write_block(base_defs(), &on_tracks(w.next_id(), &[(9, 400, &[0; 40])]))
            .unwrap();
        w.close().unwrap();
        let bytes = tape.to_vec().unwrap();
        assert_eq!(
            writes_of(&bytes, track).len(),
            2,
            "{layout:?}: the copy comes with the block"
        );
    }
}

/// Copies take state entries, and a full piece skips them, never a call: a chain with room for 16 entries puts state
/// tracks 20 to 23, then writes past `copy_first` on track 9, so every record is due its first copy. The copies that
/// find the piece full are left out and no call is refused. A later definition, track 24, finds no room: the chain
/// cuts, and the next piece holds every definition and item in force.
#[test]
fn copies_take_state_entries_and_never_refuse_a_call() {
    use ww_tape::{ChainOptions, CutPolicy, PieceChain, PieceTapes};
    let room = 16_u64;
    let mem = ww_tape::MemPieces::new();
    let o = ChainOptions {
        cut: CutPolicy::default(),
        writer: ww_tape::WriterOptions {
            index: IndexPolicy {
                state_entries_max: exact_units::Count::new(room),
                ..IndexPolicy::NONE
            },
            near: NearPolicy::default(),
            ..crate::definitions::options(1024)
        },
        tapes: PieceTapes::Each,
    };
    let mut batch = crate::definitions::base(Layout::UNALIGNED)[2..].to_vec();
    batch.extend((20..=24).map(crate::definitions::state));
    let defs = crate::definitions::held(&batch);
    let mut chain = PieceChain::create_on(mem.clone(), OwnerId([0x5e; 32]), 0, &[], o).unwrap();
    for t in 20..=23 {
        chain.put(&defs, TrackNum::new(t), 0, &[t as u8]).unwrap();
    }
    for k in 0..200 {
        let id = chain.writer().unwrap().next_id();
        chain
            .write_block(&defs, &on_tracks(id, &[(9, k, &[k as u8; 40])]))
            .unwrap();
    }
    assert_eq!(chain.seq(), 0, "the copies cut no piece");
    chain.put(&defs, TrackNum::new(24), 1, &[24]).unwrap();
    assert_eq!(chain.seq(), 1, "the new definition cut");
    chain.close().unwrap();
    let keys = mem.keys().unwrap();
    let first = mem.get(&keys[0]).unwrap().unwrap().to_vec().unwrap();
    let r = TapeReader::open(&first[..]).unwrap();
    let entries = r.state_entries(0).unwrap().len();
    let slots: Vec<Slot> = [Slot::Layout, Slot::Producer]
        .into_iter()
        .chain((20..=23).map(|t| Slot::Track {
            number: TrackNum::new(t),
        }))
        .chain((20..=23).map(|t| Slot::State {
            track: TrackNum::new(t),
        }))
        .chain([Slot::Track {
            number: TrackNum::new(9),
        }])
        .collect();
    let writes: Vec<usize> = slots.iter().map(|s| writes_of(&first, *s).len()).collect();
    assert_eq!(
        entries, room as usize,
        "the copies fill the piece's state entries"
    );
    // the layout's, the producer's and track 9's copies came first; those of the state tracks found the piece full
    assert_eq!(writes, [2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 2]);
    assert!(r.verify(0).unwrap().holds);
    let second = mem.get(&keys[1]).unwrap().unwrap().to_vec().unwrap();
    let r = TapeReader::open(&second[..]).unwrap();
    assert!(r.state_entries(0).unwrap().len() <= room as usize);
    let at = r.definitions_at(0, second.len() as u64).unwrap();
    assert_eq!(
        crate::definitions::tracks(&at),
        [9, 20, 21, 22, 23, 24],
        "the carry and the new definition"
    );
    for t in 20..=24 {
        let slot = Slot::State {
            track: TrackNum::new(t),
        };
        let state = r.state_at(0, slot, second.len() as u64).unwrap();
        assert_eq!(state.item.map(|s| s.item), Some(vec![t as u8]), "track {t}");
    }
}
