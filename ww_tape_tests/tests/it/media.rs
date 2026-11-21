//! Tapes on media: sessions, stale blocks of earlier lives, blank and torn stretches the data goes on after.

use ww_tape::piece::{OwnerId, PieceHead, PieceId, PieceIdent, piece_head_len, write_piece_head};
use ww_tape::{
    BlockId, BlockKind, BlockWriter, ClockTime, Cut, Describe, FileBlocks, FileError, Layout,
    Medium, OpenOptions, Recovery, SessionTag, SyncPolicy, TapeReader,
};
use ww_tape::{ClockNum, RecordTrack, TrackNum};

const OLD: SessionTag = SessionTag::writable(0x0bad_0001);
const NEW: SessionTag = SessionTag::writable(0x600d_0002);

fn records(id: BlockId) -> Vec<u8> {
    let mut buf = [0u8; 256];
    let mut b = BlockWriter::new(&mut buf, BlockKind::Records, id, &[ClockNum::new(1)]).unwrap();
    b.push(
        TrackNum::new(9),
        ClockTime::new(ClockNum::new(1), id.seq as i64),
        &[&[id.seq as u8; 40]],
    )
    .unwrap();
    let len = b.finish();
    buf[..len].to_vec()
}

fn piece_head(session: SessionTag, prev_session: SessionTag) -> Vec<u8> {
    let mut buf = vec![0u8; piece_head_len(0)];
    let head = PieceHead {
        ident: PieceIdent {
            id: PieceId([session.get() as u8; 16]),
            owner: OwnerId([1; 32]),
            seq: 0,
        },
        prev: None,
        prev_session,
        integrity: ww_tape::Integrity::Full,
    };
    let n = write_piece_head(&mut buf, BlockId::new(session, 0), &head, &[]).unwrap();
    buf.truncate(n);
    buf
}

/// Blocks of the definitions the tests' data needs ([crate::definitions::base]) as a producer on a medium writes
/// them, of at most `max` bytes each, numbered from `first` on.
fn describe_blocks_max(layout: Layout, first: BlockId, max: usize) -> Vec<Vec<u8>> {
    let items: Vec<Vec<u8>> = crate::definitions::base(layout)
        .iter()
        .map(|d| ww_tape::open::encode(d).unwrap())
        .collect();
    let refs: Vec<ww_tape::RecordRef<'_>> = items
        .iter()
        .map(|item| ww_tape::RecordRef::definition(item))
        .collect();
    let mut id = first;
    let mut blocks = vec![];
    ww_tape::open::write_records(
        &refs,
        max,
        0,
        || {
            let this = id;
            id = id.next();
            this
        },
        |b| {
            blocks.push(b.to_vec());
            Ok::<(), ww_tape::DescribeError>(())
        },
    )
    .unwrap();
    blocks
}

/// A session on a medium: its piece head naming `prev_session`, a block of definitions with `layout`, then `n`
/// records blocks.
fn session(session: SessionTag, prev_session: SessionTag, layout: Layout, n: u64) -> Vec<u8> {
    let mut out = piece_head(session, prev_session);
    let defs = describe_blocks_max(layout, BlockId::new(session, 0), 4096);
    out.extend(defs.concat());
    let mut id = BlockId::new(session, defs.len() as u64);
    for _ in 0..n {
        out.extend(records(id));
        id = id.next();
    }
    out
}

/// What a walk gives: (session, seq) of each records block of data (not of definitions), and the errors.
fn walk(bytes: &[u8], medium: Medium) -> (Vec<(SessionTag, u64)>, Vec<FileError>) {
    let mut rd = FileBlocks::new(bytes).on(medium);
    let (mut got, mut errors) = (vec![], vec![]);
    while let Some(b) = rd.next_block() {
        match b {
            Ok(b)
                if b.kind == BlockKind::Records
                    && b.first_track() != Some(RecordTrack::Definitions) =>
            {
                got.push((b.session, b.seq))
            }
            Ok(_) => {}
            Err(e) => errors.push(e),
        }
    }
    (got, errors)
}

fn seqs(session: SessionTag, seqs: core::ops::Range<u64>) -> Vec<(SessionTag, u64)> {
    seqs.map(|s| (session, s)).collect()
}

/// A card written once to its end, then again from its start by a shorter session: the old blocks behind the new end
/// of data are never data, whether the new session ends mid-way through an old block or on an old piece head.
#[test]
fn stale_blocks_behind_the_end_are_not_data() {
    let sd = Layout::UNALIGNED.on_sd_card();
    let old = session(OLD, SessionTag::NONE, sd, 200);
    let new = session(NEW, SessionTag::NONE, sd, 20);
    // the new session overwrote the start of the old one, ending mid-way through an old block
    let mut card = new.clone();
    card.extend_from_slice(&old[new.len() + 13..]);
    let (got, errors) = walk(&card, Medium::Raw);
    assert_eq!(got, seqs(NEW, 1..21));
    assert!(
        matches!(errors[..], [FileError::Cut { offset, why: Cut::Bad(_) }] if offset == new.len() as u64),
        "{errors:?}"
    );
    // the old life starts right where the new one ends: its piece head names no session that ended there
    let card = [new.clone(), old].concat();
    let (got, errors) = walk(&card, Medium::Raw);
    assert_eq!(got, seqs(NEW, 1..21));
    assert!(
        matches!(errors[..], [FileError::Cut { offset, why: Cut::Stale { session } }] if offset == new.len() as u64 && session == OLD),
        "{errors:?}"
    );
}

/// A file takes any piece head as the start of a session: concatenated pieces read whole.
#[test]
fn a_file_takes_any_piece_head_as_a_session_start() {
    let sd = Layout::UNALIGNED.on_sd_card();
    let old = session(OLD, SessionTag::NONE, sd, 200);
    let new = session(NEW, SessionTag::NONE, sd, 20);
    let (got, errors) = walk(&[new, old].concat(), Medium::File);
    assert_eq!(got, [seqs(NEW, 1..21), seqs(OLD, 1..201)].concat());
    assert!(errors.is_empty(), "{errors:?}");
}

/// On a raw medium, a piece head naming the session just ended starts the next session.
#[test]
fn a_piece_head_naming_the_ended_session_starts_the_next() {
    let sd = Layout::UNALIGNED.on_sd_card();
    let new = session(NEW, SessionTag::NONE, sd, 20);
    let next = session(OLD, NEW, sd, 3);
    let (got, errors) = walk(&[new, next].concat(), Medium::Raw);
    assert_eq!(got, [seqs(NEW, 1..21), seqs(OLD, 1..4)].concat());
    assert!(errors.is_empty(), "{errors:?}");
}

/// A records block of another session is stale on a file too; the salvage walk reports and skips it.
#[test]
fn a_records_block_of_another_session_is_stale_on_a_file() {
    let sd = Layout::UNALIGNED.on_sd_card();
    let new = session(NEW, SessionTag::NONE, sd, 20);
    let file = [
        new.clone(),
        records(BlockId::new(OLD, 99)),
        records(BlockId::new(NEW, 21)),
    ]
    .concat();
    let (got, errors) = walk(&file, Medium::File);
    assert_eq!(got, seqs(NEW, 1..21));
    assert!(matches!(
        errors[..],
        [FileError::Cut {
            why: Cut::Stale { .. },
            ..
        }]
    ));
    let mut rd = FileBlocks::with_recovery(&file[..], Recovery::Resync);
    let (mut got, mut stale) = (vec![], 0);
    while let Some(b) = rd.next_block() {
        match b {
            Ok(b)
                if b.kind == BlockKind::Records
                    && b.first_track() != Some(RecordTrack::Definitions) =>
            {
                got.push(b.seq)
            }
            Ok(_) => {}
            Err(FileError::Stale { session, .. }) if session == OLD => stale += 1,
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!((got, stale), ((1..22).collect::<Vec<_>>(), 1));
}

/// `bytes` with `stretch` put in at `at`, padded with its first byte so the block after it starts on a multiple of
/// 512 when `align` is set; the stretch's offset and length.
fn with_stretch(bytes: &[u8], at: usize, stretch: &[u8], align: bool) -> (Vec<u8>, u64, u64) {
    let mut out = bytes[..at].to_vec();
    out.extend_from_slice(stretch);
    if align {
        while !out.len().is_multiple_of(512) {
            out.push(stretch[0]);
        }
    }
    let len = (out.len() - at) as u64;
    out.extend_from_slice(&bytes[at..]);
    (out, at as u64, len)
}

/// Where the records block `seq` of a [session] starts.
fn block_start(bytes: &[u8], seq: u64) -> usize {
    let mut rd = FileBlocks::new(bytes).on(Medium::Raw);
    while let Some(Ok(b)) = rd.next_block() {
        if b.kind == BlockKind::Records && b.seq == seq {
            return rd.offset() as usize;
        }
    }
    panic!("no block {seq}");
}

/// A blank sector (erased or trimmed) followed by the session's next block is a gap, not the end, where the layout
/// allows gaps; on a file it stays the end.
#[test]
fn a_blank_sector_followed_by_the_sessions_next_block_is_a_gap() {
    let sd = Layout::UNALIGNED.on_sd_card();
    for blank in [0x00u8, 0xFF] {
        let card = session(NEW, SessionTag::NONE, sd, 10);
        let at = block_start(&card, 5);
        let (card, offset, len) = with_stretch(&card, at, &[blank; 512], false);
        let (got, errors) = walk(&card, Medium::Raw);
        assert_eq!(got, seqs(NEW, 1..11));
        assert!(
            matches!(errors[..], [FileError::Gap { offset: o, len: l }] if o == offset && l == len),
            "{errors:?}"
        );
        let file = session(NEW, SessionTag::NONE, Layout::UNALIGNED, 10);
        let (file, offset, _) = with_stretch(&file, block_start(&file, 5), &[blank; 512], false);
        let (got, errors) = walk(&file, Medium::File);
        assert_eq!(got, seqs(NEW, 1..5));
        assert!(
            matches!(errors[..], [FileError::Cut { offset: o, why: Cut::Bad(_) }] if o == offset),
            "{errors:?}"
        );
    }
}

/// A torn unit (garbage) followed by the session's next block on the next resume boundary is a gap; an older block of
/// the session there, or one past the gap's reach, is not, and the stretch is the end of data.
#[test]
fn a_torn_unit_then_the_next_block_on_a_boundary_is_a_gap() {
    let sd = Layout::UNALIGNED.on_sd_card();
    let card = session(NEW, SessionTag::NONE, sd, 10);
    let at = block_start(&card, 5);
    let (torn, offset, len) = with_stretch(&card, at, &[0x5A; 700], true);
    let (got, errors) = walk(&torn, Medium::Raw);
    assert_eq!(got, seqs(NEW, 1..11));
    assert!(
        matches!(errors[..], [FileError::Gap { offset: o, len: l }] if o == offset && l == len),
        "{errors:?}"
    );
    // the block on the boundary is an older one of the session: stale, the end of data is the stretch's start
    let mut stale = card[..at].to_vec();
    stale.extend_from_slice(&[0x5A; 300]);
    while !stale.len().is_multiple_of(512) {
        stale.push(0x5A);
    }
    stale.extend(records(BlockId::new(NEW, 2)));
    let (got, errors) = walk(&stale, Medium::Raw);
    assert_eq!(got, seqs(NEW, 1..5));
    assert!(matches!(errors[..], [FileError::Cut { offset: o, .. }] if o == at as u64));
    // a stretch longer than the gap's reach is the end
    let (far, _, _) = with_stretch(
        &card,
        at,
        &vec![0x5A; ww_tape::SD_GAP_MAX as usize + 1024],
        true,
    );
    let (got, errors) = walk(&far, Medium::Raw);
    assert_eq!(got, seqs(NEW, 1..5));
    assert!(matches!(errors[..], [FileError::Cut { offset: o, .. }] if o == at as u64));
}

/// A closed file of session `session` (definitions with `layout`, `n` records blocks, final index, close, tail).
fn closed(session: SessionTag, layout: Layout, n: u64) -> Vec<u8> {
    let dir = std::env::temp_dir().join(format!(
        "ww_tape_media_{}_{}",
        session.get(),
        std::process::id()
    ));
    _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("old.wwt");
    let mut w = ww_tape_fs::writer::create(&path, SyncPolicy::None).unwrap();
    w.set_layout(layout).unwrap();
    w.set_session(session).unwrap();
    for _ in 0..n {
        w.write_block(crate::definitions::base_defs(), &records(w.next_id()))
            .unwrap();
    }
    w.close().unwrap();
    let bytes = std::fs::read(&path).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    bytes
}

fn raw() -> OpenOptions {
    OpenOptions {
        medium: Medium::Raw,
        ..Default::default()
    }
}

/// On a raw medium the old life's tail sits at the end; opened as a file it would pass for the newest piece. Opened
/// as a raw medium, the walk goes forward from the start and ends where the new session ends.
#[test]
fn a_raw_medium_opens_forward_and_never_takes_a_stale_tail() {
    let sd = Layout::UNALIGNED.on_sd_card();
    let old = closed(OLD, sd, 200);
    let new = session(NEW, SessionTag::NONE, sd, 20);
    // the new session overwrote the start of the old file in place
    let mut card = new.clone();
    card.extend_from_slice(&old[new.len()..]);
    let as_file = TapeReader::open(&card[..]).unwrap();
    assert!(
        as_file.pieces().iter().any(|p| p.tail.is_some()),
        "the stale tail fools a file reader"
    );
    let r = TapeReader::open_with(&card[..], raw()).unwrap();
    assert_eq!(r.pieces().len(), 1);
    let p = &r.pieces()[0];
    assert_eq!(
        (p.start, p.end, p.cut),
        (0, new.len() as u64, Some(new.len() as u64))
    );
    assert!(p.tail.is_none());
    assert_eq!(
        p.head.as_ref().unwrap().0.ident.id,
        PieceId([NEW.get() as u8; 16])
    );
    // from a hint: the walk starts there
    let hint = block_start(&card, 10) as u64;
    let r = TapeReader::open_with(
        &card[..],
        OpenOptions {
            start: hint,
            ..raw()
        },
    )
    .unwrap();
    assert_eq!(
        (r.pieces()[0].start, r.pieces()[0].end),
        (hint, new.len() as u64)
    );
    assert_eq!(listed_blocks(&r), 11, "records 10 to 20");
}

/// Stale data of another version after the end of a shorter new session is stale, the end of the data, not a piece
/// of that version: the walk judges a block's session before its version (reading side review 3, point 3).
#[test]
fn stale_data_of_another_version_ends_a_raw_medium() {
    let sd = Layout::UNALIGNED.on_sd_card();
    let old = closed(OLD, sd, 200);
    let new = session(NEW, SessionTag::NONE, sd, 20);
    // the new session ends where a block of the old one starts
    let at = block_start(&old, 30);
    let mut card = new.clone();
    card.extend_from_slice(&crate::checks::of_version(&old[at..], ww_tape::VERSION + 1));
    let r = TapeReader::open_with(&card[..], raw()).unwrap();
    assert_eq!(r.pieces().len(), 1);
    let p = &r.pieces()[0];
    assert_eq!(p.version, None);
    assert_eq!((p.end, p.cut), (new.len() as u64, Some(new.len() as u64)));
    assert_eq!(listed_blocks(&r), 21);
}

/// The blocks piece 0 of `r` lists: a block of definitions has an entry per record.
fn listed_blocks<S: ww_tape::reader::ByteSource>(r: &TapeReader<S>) -> usize {
    let mut offsets: Vec<u64> = r.entries(0).unwrap().iter().map(|e| e.offset).collect();
    offsets.dedup();
    offsets.len()
}

/// The positional reader reads past a gap as the walk does, and lists it.
#[test]
fn the_positional_reader_reads_past_a_gap() {
    let sd = Layout::UNALIGNED.on_sd_card();
    let card = session(NEW, SessionTag::NONE, sd, 10);
    let at = block_start(&card, 5);
    let (card, offset, len) = with_stretch(&card, at, &[0x5A; 700], true);
    let r = TapeReader::open_with(&card[..], raw()).unwrap();
    let p = &r.pieces()[0];
    assert_eq!(p.gaps, [(offset, len)]);
    assert_eq!((p.end, p.cut), (card.len() as u64, None));
    // a block of definitions and 10 records blocks
    assert_eq!(listed_blocks(&r), 11);
}

/// A block written twice (a rewrite after a failed write, two copies merged) is a duplicate, not a cut, for the
/// positional reader too: the same bytes are dropped, other bytes replace the earlier copy.
#[test]
fn a_duplicate_block_is_not_a_cut() {
    let card = session(NEW, SessionTag::NONE, Layout::UNALIGNED, 6);
    let at = block_start(&card, 4);
    let again = card[block_start(&card, 3)..at].to_vec();
    let dup = [&card[..at], &again, &card[at..]].concat();
    let r = TapeReader::open_with(&dup[..], raw()).unwrap();
    let p = &r.pieces()[0];
    assert_eq!((p.end, p.cut), (dup.len() as u64, None));
    assert!(p.replaced.is_empty());
    assert_eq!(
        listed_blocks(&r),
        7,
        "describe and 6 records, the copy dropped"
    );
    let (got, errors) = walk(&dup, Medium::Raw);
    assert_eq!((got, errors.len()), (seqs(NEW, 1..7), 0));
    // other bytes under the same id: the later copy wins, reported
    let other = {
        let mut buf = [0u8; 256];
        let mut b = BlockWriter::new(
            &mut buf,
            BlockKind::Records,
            BlockId::new(NEW, 3),
            &[ClockNum::new(1)],
        )
        .unwrap();
        b.push(
            TrackNum::new(9),
            ClockTime::new(ClockNum::new(1), 3),
            &[b"rewritten"],
        )
        .unwrap();
        let len = b.finish();
        buf[..len].to_vec()
    };
    let rewritten = [&card[..at], &other, &card[at..]].concat();
    let r = TapeReader::open_with(&rewritten[..], raw()).unwrap();
    let p = &r.pieces()[0];
    assert_eq!((p.cut, p.replaced.as_slice()), (None, &[at as u64][..]));
    assert_eq!(listed_blocks(&r), 7);
    let (got, errors) = walk(&rewritten, Medium::Raw);
    assert_eq!(got, [seqs(NEW, 1..4), seqs(NEW, 3..7)].concat());
    assert!(
        matches!(errors[..], [FileError::Replaced { seq: 3, .. }]),
        "{errors:?}"
    );
}

/// A block written again after padding to the next sector (a rewrite after a failed program) resumes the data like
/// its next block does: same bytes dropped, other bytes taking its place, for both readers.
#[test]
fn a_duplicate_after_a_gap_resumes_the_data() {
    let sd = Layout::UNALIGNED.on_sd_card();
    let card = session(NEW, SessionTag::NONE, sd, 10);
    let (from, at) = (block_start(&card, 3), block_start(&card, 4));
    let other = {
        let mut buf = [0u8; 256];
        let mut b = BlockWriter::new(
            &mut buf,
            BlockKind::Records,
            BlockId::new(NEW, 3),
            &[ClockNum::new(1)],
        )
        .unwrap();
        b.push(
            TrackNum::new(9),
            ClockTime::new(ClockNum::new(1), 3),
            &[b"rewritten"],
        )
        .unwrap();
        let len = b.finish();
        buf[..len].to_vec()
    };
    for (again, replaced) in [(card[from..at].to_vec(), false), (other, true)] {
        let mut padded = card[..at].to_vec();
        while !padded.len().is_multiple_of(512) {
            padded.push(0xFF);
        }
        let gap = (at as u64, (padded.len() - at) as u64);
        let bytes = [&padded[..], &again, &card[at..]].concat();
        let (got, errors) = walk(&bytes, Medium::Raw);
        let want = if replaced {
            [seqs(NEW, 1..4), seqs(NEW, 3..11)].concat()
        } else {
            seqs(NEW, 1..11)
        };
        assert_eq!(got, want);
        assert!(
            matches!(errors[0], FileError::Gap { offset, len } if (offset, len) == gap),
            "{errors:?}"
        );
        assert_eq!(errors.len(), 1 + usize::from(replaced), "{errors:?}");
        let r = TapeReader::open_with(&bytes[..], raw()).unwrap();
        let p = &r.pieces()[0];
        assert_eq!((p.end, p.cut), (bytes.len() as u64, None));
        assert_eq!(p.gaps, [gap]);
        assert_eq!(p.replaced.len(), usize::from(replaced));
    }
}

/// A layout whose gap holds more than [ww_tape::GAP_PROBES_MAX] resume places, or is longer than
/// [ww_tape::GAP_MAX_MAX], is not one a reader takes: its describe record doesn't decode, and a walk gives it no gap.
#[test]
fn a_layout_past_the_gap_limits_is_refused() {
    use exact_units::Bytes;
    let layout = |gap_max: u32, resume_align: u32| Layout {
        gap_max: Bytes::new(gap_max),
        resume_align: Bytes::new(resume_align),
        ..Layout::UNALIGNED
    };
    assert!(Layout::UNALIGNED.on_sd_card().is_valid());
    assert!(
        layout(
            ww_tape::GAP_MAX_MAX,
            ww_tape::GAP_MAX_MAX / ww_tape::GAP_PROBES_MAX
        )
        .is_valid()
    );
    for bad in [
        layout(u32::MAX, 1),
        layout(ww_tape::GAP_MAX_MAX + 512, 512),
        layout(ww_tape::GAP_PROBES_MAX + 1, 1),
        layout(512, 0),
    ] {
        assert!(!bad.is_valid(), "{bad:?}");
        let d = Describe::Layout(bad);
        let mut scratch = [0u8; 256];
        let item = shrink_wrap::SerializeShrinkWrap::to_ww_bytes(&d, &mut scratch)
            .unwrap()
            .to_vec();
        let mut buf = [0u8; 256];
        let mut b =
            BlockWriter::new(&mut buf, BlockKind::Records, BlockId::new(NEW, 0), &[]).unwrap();
        b.push_describe(&[&item]).unwrap();
        let len = b.finish();
        let mut bytes = buf[..len].to_vec();
        let parsed = ww_tape::Block::read(&bytes).unwrap();
        let rec = parsed.records().next().unwrap().unwrap();
        assert!(
            <Describe<'_> as shrink_wrap::DeserializeShrinkWrap>::from_ww_bytes(rec.item).is_err()
        );
        // a byte after it: no gap rules, the end of data at once
        bytes.push(0x41);
        let (got, errors) = walk(&bytes, Medium::Raw);
        assert!(got.is_empty());
        assert!(
            matches!(errors[..], [FileError::Cut { offset: o, .. }] if o == len as u64),
            "{errors:?}"
        );
    }
}

/// Counts what a positional reader reads.
struct Counted<'a> {
    bytes: &'a [u8],
    reads: core::cell::Cell<u64>,
    read_bytes: core::cell::Cell<u64>,
}

impl ww_tape::reader::ByteSource for Counted<'_> {
    type Error = ww_tape::reader::OutOfRange;

    fn size(&self) -> Result<u64, Self::Error> {
        self.bytes.size()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), Self::Error> {
        self.reads.set(self.reads.get() + 1);
        self.read_bytes
            .set(self.read_bytes.get() + buf.len() as u64);
        self.bytes.read_at(offset, buf)
    }
}

/// A stretch whose every sector starts with a header of the session's next block, each 1 MiB long by its header but
/// none good: a reader reads each header once and checks at most [ww_tape::GAP_CHECKS_MAX] of them whole, then
/// takes the stretch's start as the end of data.
#[test]
fn a_gap_costs_one_header_per_resume_place() {
    let sd = Layout::UNALIGNED.on_sd_card();
    let card = session(NEW, SessionTag::NONE, sd, 6);
    let mut header = records(BlockId::new(NEW, 100))[..32].to_vec();
    let claimed = 1u32 << 20;
    header[8..12].copy_from_slice(&claimed.to_le_bytes());
    let mut bytes = card.clone();
    let at = bytes.len() as u64;
    // a bad block where the next would start, then a header on every sector
    bytes.extend_from_slice(&[0x5A; 100]);
    while bytes.len() < card.len() + ww_tape::SD_GAP_MAX as usize + (2 << 20) {
        if bytes.len().is_multiple_of(512) {
            bytes.extend_from_slice(&header);
        } else {
            bytes.push(0x5A);
        }
    }
    let src = Counted {
        bytes: &bytes,
        reads: 0.into(),
        read_bytes: 0.into(),
    };
    let r = TapeReader::open_with(&src, raw()).unwrap();
    let p = &r.pieces()[0];
    assert_eq!((p.end, p.cut), (at, Some(at)));
    let places = u64::from(ww_tape::SD_GAP_MAX / ww_tape::SD_RESUME_ALIGN) + 1;
    let whole = u64::from(ww_tape::GAP_CHECKS_MAX) * u64::from(claimed);
    let most = card.len() as u64 * 4 + places * 32 + whole + (1 << 20);
    assert!(
        src.read_bytes.get() <= most,
        "{} bytes read, at most {most}",
        src.read_bytes.get()
    );
    // the stream reader cuts at the same place
    let (got, errors) = walk(&bytes, Medium::Raw);
    assert_eq!(got, seqs(NEW, 1..7));
    assert!(
        matches!(errors[..], [FileError::Cut { offset: o, .. }] if o == at),
        "{errors:?}"
    );
}

/// Counts the bytes a stream reader takes.
struct CountedRead<'a> {
    bytes: &'a [u8],
    read: u64,
}

impl std::io::Read for CountedRead<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.bytes.read(buf)?;
        self.read += n as u64;
        Ok(n)
    }
}

/// Erased space after the end of data on a raw medium is the end once it is longer than the gap: neither reader reads
/// the rest of the medium, and stale data after it is no cut.
#[test]
fn erased_space_past_the_gap_ends_a_raw_walk() {
    let sd = Layout::UNALIGNED.on_sd_card();
    let card = session(NEW, SessionTag::NONE, sd, 6);
    let at = card.len() as u64;
    let mut bytes = card.clone();
    bytes.resize(card.len() + (16 << 20), 0xFF);
    bytes.extend(records(BlockId::new(OLD, 9)));
    let src = Counted {
        bytes: &bytes,
        reads: 0.into(),
        read_bytes: 0.into(),
    };
    let r = TapeReader::open_with(&src, raw()).unwrap();
    let p = &r.pieces()[0];
    assert_eq!((p.end, p.cut), (at, None));
    let most = at * 4 + 2 * u64::from(ww_tape::SD_GAP_MAX) + (1 << 20);
    assert!(
        src.read_bytes.get() <= most,
        "{} bytes read, at most {most}",
        src.read_bytes.get()
    );
    let mut rd = CountedRead {
        bytes: &bytes,
        read: 0,
    };
    let mut fb = FileBlocks::new(&mut rd).on(Medium::Raw);
    let mut n = 0;
    while let Some(b) = fb.next_block() {
        b.unwrap();
        n += 1;
    }
    drop(fb);
    assert_eq!(
        n,
        walk(&card, Medium::Raw).0.len() + 2,
        "piece head, describe, records"
    );
    assert!(
        rd.read <= at + u64::from(ww_tape::SD_GAP_MAX) + (1 << 20),
        "{} bytes read",
        rd.read
    );
}

/// On a raw medium a piece whose first block of definitions is damaged and whose session ends there, with an earlier
/// life's definitions behind it, has no definitions: the stale ones are never taken for its.
#[test]
fn stale_definitions_are_not_the_pieces() {
    let set = |session: SessionTag| {
        let out = describe_blocks_max(Layout::UNALIGNED, BlockId::new(session, 0), 64);
        assert!(out.len() >= 2);
        out
    };
    let mut card = piece_head(NEW, SessionTag::NONE);
    let new = set(NEW);
    let damaged_at = card.len();
    card.extend(new.concat());
    card[damaged_at + 40] ^= 1;
    card.extend(set(OLD).concat());
    let r = TapeReader::open_with(&card[..], raw()).unwrap();
    assert_eq!(r.pieces()[0].cut, Some(damaged_at as u64));
    assert!(r.definitions_at(0, u64::MAX).unwrap().items.is_empty());
}

/// A file that allows gaps, read from its newest checkpoint: the stretch after it is read with the file's gap rules,
/// as a walk from the start reads it.
#[test]
fn a_gap_after_the_newest_checkpoint_is_read_past() {
    let dir = std::env::temp_dir().join(format!("ww_tape_media_ckpt_{}", std::process::id()));
    _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut w = ww_tape_fs::writer::create(&dir.join("c.wwt"), SyncPolicy::None).unwrap();
    w.set_index_policy(ww_tape::IndexPolicy {
        bytes: 2048,
        interval: None,
        ..ww_tape::IndexPolicy::default()
    })
    .expect("a policy with room for a piece head");
    w.set_layout(Layout::UNALIGNED.on_sd_card()).unwrap();
    for _ in 0..100 {
        w.write_block(crate::definitions::base_defs(), &records(w.next_id()))
            .unwrap();
    }
    // a few after the newest checkpoint
    w.set_index_policy(ww_tape::IndexPolicy::NONE)
        .expect("a policy with room for a piece head");
    for _ in 0..5 {
        w.write_block(crate::definitions::base_defs(), &records(w.next_id()))
            .unwrap();
    }
    let bytes = std::fs::read(w.sink().path()).unwrap();
    drop(w);
    std::fs::remove_dir_all(&dir).unwrap();
    // a blank sector before a records block after the newest checkpoint
    let mut rd = FileBlocks::new(&bytes[..]);
    let (mut index, mut recs) = (vec![], vec![]);
    while let Some(Ok(b)) = rd.next_block() {
        match b.kind {
            BlockKind::Index => index.push(rd.offset()),
            BlockKind::Records => recs.push(rd.offset()),
            _ => {}
        }
    }
    let last_index = *index.last().unwrap();
    let after = *recs.iter().find(|r| **r > last_index).unwrap();
    assert!(after > last_index && last_index > 0);
    let (file, offset, len) = with_stretch(&bytes, after as usize, &[0u8; 512], false);
    let (got, errors) = walk(&file, Medium::File);
    assert_eq!(got.len(), 105);
    assert!(matches!(errors[..], [FileError::Gap { .. }]), "{errors:?}");
    let r = TapeReader::open(&file[..]).unwrap();
    let p = &r.pieces()[0];
    assert!(
        matches!(p.found, ww_tape::reader::Found::Checkpoint { .. }),
        "{:?}",
        p.found
    );
    assert_eq!(
        (p.gaps.as_slice(), p.cut, p.end),
        (&[(offset, len)][..], None, file.len() as u64)
    );
}

/// On a raw medium a piece is one session: a producer's blocks of their own session after a writer's piece head read as stale
/// data, so the piece ends at its head; adopted into the writer's session they read whole, as they do in a file either way.
#[test]
fn a_raw_piece_is_one_session() {
    let dir = std::env::temp_dir().join(format!("ww_tape_media_adopt_{}", std::process::id()));
    _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // the producer's session: its definitions, which the relay holds, then records
    let producer = session(OLD, SessionTag::NONE, Layout::UNALIGNED.on_sd_card(), 10);
    let mut rd = FileBlocks::new(&producer[..]);
    let mut blocks = vec![];
    while let Some(Ok(b)) = rd.next_block() {
        if b.kind != BlockKind::PieceHead
            && b.first_track() != Some(ww_tape::RecordTrack::Definitions)
        {
            blocks.push(b.bytes().to_vec());
        }
    }
    for adopt in [false, true] {
        let path = dir.join(format!("{adopt}.wwt"));
        let mut w = ww_tape_fs::writer::create(&path, SyncPolicy::None).unwrap();
        let owner = OwnerId([3; 32]);
        let head = PieceHead {
            ident: PieceIdent {
                id: PieceId::first(&owner),
                owner,
                seq: 0,
            },
            prev: None,
            prev_session: SessionTag::NONE,
            integrity: ww_tape::Integrity::Full,
        };
        w.begin_piece(&head, &[]).unwrap();
        for b in &blocks {
            if adopt {
                w.adopt_block(crate::definitions::base_defs(), b).unwrap();
            } else {
                w.write_block(crate::definitions::base_defs(), b).unwrap();
            }
        }
        let bytes = std::fs::read(w.sink().path()).unwrap();
        drop(w);
        let (as_file, errors) = walk(&bytes, Medium::File);
        assert_eq!((as_file.len(), errors.len()), (10, 0));
        let (got, errors) = walk(&bytes, Medium::Raw);
        let r = TapeReader::open_with(&bytes[..], raw()).unwrap();
        if adopt {
            assert_eq!((got.len(), errors.len()), (10, 0), "{errors:?}");
            assert_eq!(r.pieces()[0].cut, None);
        } else {
            // the producer's session starts where the writer puts the layout again in it
            let mut rd = FileBlocks::new(&bytes[..]);
            let mut first_old = None;
            while let Some(Ok(b)) = rd.next_block() {
                if b.session == OLD && first_old.is_none() {
                    first_old = Some(rd.offset());
                }
            }
            let first_old = first_old.unwrap();
            assert!(first_old >= piece_head_len(0) as u64);
            assert!(got.is_empty());
            assert!(
                matches!(errors[..], [FileError::Cut { offset, why: Cut::Stale { session } }] if offset == first_old && session == OLD),
                "{errors:?}"
            );
            assert_eq!(r.pieces()[0].cut, Some(first_old));
        }
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Adopting keeps lost blocks visible: two blocks missing from the producer's stream leave a jump of two in the
/// writer's seqs; a producer restart (another session) leaves none.
#[test]
fn adopting_keeps_lost_blocks_visible() {
    let dir = std::env::temp_dir().join(format!("ww_tape_media_lost_{}", std::process::id()));
    _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut w = ww_tape_fs::writer::create(&dir.join("l.wwt"), SyncPolicy::None).unwrap();
    for seq in [0, 1, 2, 5, 6] {
        w.adopt_block(
            crate::definitions::base_defs(),
            &records(BlockId::new(OLD, seq)),
        )
        .unwrap();
    }
    // the producer restarted: its new session counts from 0 again
    for seq in [0, 1] {
        w.adopt_block(
            crate::definitions::base_defs(),
            &records(BlockId::new(NEW, seq)),
        )
        .unwrap();
    }
    let bytes = std::fs::read(w.sink().path()).unwrap();
    drop(w);
    std::fs::remove_dir_all(&dir).unwrap();
    let (got, errors) = walk(&bytes, Medium::Raw);
    assert!(errors.is_empty(), "{errors:?}");
    let seqs: Vec<u64> = got.iter().map(|(_, s)| *s).collect();
    assert_eq!(seqs, [0, 1, 2, 5, 6, 7, 8]);
    assert!(got.iter().all(|(r, _)| *r == got[0].0));
}

/// Every block of a walk with `recovery` on `medium`, as bytes; each one Resync gives reads whole.
fn blocks(bytes: &[u8], recovery: Recovery, medium: Medium) -> Vec<Vec<u8>> {
    let mut rd = FileBlocks::with_recovery(bytes, recovery).on(medium);
    let (mut out, mut steps) = (vec![], 0);
    while let Some(b) = rd.next_block() {
        if let Ok(b) = b {
            assert!(
                ww_tape::Block::read(b.bytes()).is_ok(),
                "{recovery:?} gives a block that doesn't read"
            );
            out.push(b.bytes().to_vec());
        } else if let Err(FileError::Conflict { block, .. }) = b {
            // the second of two blocks of one seq comes in the conflict report
            out.push(block);
        }
        steps += 1;
        assert!(steps <= bytes.len() + 1, "every step consumes bytes");
    }
    out
}

/// Whether the Resync walk of `bytes` gives every block the Prefix walk gives, in order, on `medium`: as a block, or
/// as the second block of a conflict.
fn resync_reads_the_prefix(bytes: &[u8], medium: Medium) -> Result<(), String> {
    let prefix = blocks(bytes, Recovery::Prefix, medium);
    let resync = blocks(bytes, Recovery::Resync, medium);
    let mut rest = resync.iter();
    for (i, b) in prefix.iter().enumerate() {
        if !rest.any(|r| r == b) {
            return Err(format!(
                "{medium:?}: Resync misses block {i} of {} Prefix reads",
                prefix.len()
            ));
        }
    }
    Ok(())
}

/// Whether `b` may start a session after one of `last` on `medium`.
fn may_start(b: &ww_tape::Block<'_>, last: SessionTag, medium: Medium) -> bool {
    match medium {
        Medium::File => {
            b.kind == BlockKind::PieceHead
                || (b.kind == BlockKind::Records
                    && b.first_track() == Some(RecordTrack::Definitions))
        }
        Medium::Raw => {
            b.kind == BlockKind::PieceHead
                && ww_tape::piece::PieceHeadBlock::read(b)
                    .is_ok_and(|h| h.head().prev_session == last)
        }
    }
}

/// Whether the Resync walk of `bytes` on `medium` passes over no block that reads whole: each one before where the
/// walk ends is handed out, reported stale, or lies inside a block handed out or reported stale before it; and a block
/// reported stale is of another session than the last one handed out and may not start a session there.
fn resync_misses_no_block(bytes: &[u8], medium: Medium) -> Result<(), String> {
    let mut rd = FileBlocks::with_recovery(bytes, Recovery::Resync).on(medium);
    // (offset, length) of each block handed out or reported stale
    let mut met = vec![];
    let mut last = None;
    while let Some(b) = rd.next_block() {
        match b {
            Ok(b) => {
                last = Some(b.session);
                let len = b.len() as u64;
                met.push((rd.offset(), len));
            }
            Err(FileError::Stale { offset, len, .. }) => {
                let at = offset as usize;
                let b = ww_tape::Block::read(&bytes[at..at + len as usize]).unwrap();
                let last = last.expect("a block is stale after a block handed out");
                if b.session == last || may_start(&b, last, medium) {
                    return Err(format!(
                        "{medium:?}: Resync calls the block at {at} stale after a block of {:08x}",
                        last.get()
                    ));
                }
                met.push((offset, len));
            }
            Err(FileError::Conflict { offset, block, .. }) => {
                met.push((offset, block.len() as u64))
            }
            Err(_) => {}
        }
    }
    let end = rd.offset();
    for at in 0..bytes.len() {
        let Some(len) = ww_tape::block_len(&bytes[at..]).ok() else {
            continue;
        };
        let x = at as u64;
        if x >= end || at + len > bytes.len() || ww_tape::Block::read(&bytes[at..at + len]).is_err()
        {
            continue;
        }
        if !met.iter().any(|&(o, l)| o == x || (o < x && x < o + l)) {
            return Err(format!("{medium:?}: Resync passes over the block at {at}"));
        }
    }
    Ok(())
}

/// A whole session that may start one inside a stretch the Prefix walk reads past as a gap: the Resync walk hands
/// out all its blocks and all of the session around it, none stale.
#[test]
fn resync_salvages_a_session_inside_a_gap() {
    let sd = Layout::UNALIGNED.on_sd_card();
    let card = session(NEW, SessionTag::NONE, sd, 10);
    let at = block_start(&card, 5);
    let inside = session(OLD, NEW, sd, 3);
    let (torn, _, _) = with_stretch(&card, at, &[[0x5A; 77].as_slice(), &inside].concat(), true);
    for medium in [Medium::File, Medium::Raw] {
        let (mut old, mut new, mut stale) = (0, 0, 0);
        let mut rd = FileBlocks::with_recovery(&torn[..], Recovery::Resync).on(medium);
        while let Some(b) = rd.next_block() {
            match b {
                Ok(b) if b.session == OLD => old += 1,
                Ok(b) if b.session == NEW => new += 1,
                Err(FileError::Stale { .. }) => stale += 1,
                _ => {}
            }
        }
        let count = |bytes: &[u8]| blocks(bytes, Recovery::Prefix, medium).len();
        assert_eq!(
            (old, new, stale),
            (count(&inside), count(&card), 0),
            "{medium:?}"
        );
        resync_misses_no_block(&torn, medium).unwrap();
    }
}

/// A fuzz input: a file of two sessions with torn blocks and a gap. Session 0bc0d313's definitions lie inside the
/// stretch the Prefix walk reads past as a gap, off the resume places; the Resync walk took them as a new session
/// and then skipped the first session's next block, past the gap, as stale.
#[test]
fn resync_reads_the_block_after_a_gap_holding_another_session() {
    let hex = "57575450010a00009c00000000000000000000000000000002000d60000000000202020202020202020202020202020201010101010101010101010101010101010101010101010101010101010101010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000060000000000000008e297a4457575450010100004500000002000000000000000000000002000d6000000000000008006d6564696105060014600000000000100000000200000000000201052aa15eba1357575450010100005100000001000000010000000000000000000000000000000000000000000000000000000000000000000000000060000000000000007d6ad47257575450010106004000000002000000000000000000000013d3c00b00000000000014600000000000000000010000000000000201052a0003000001dec8d0315757545001010600000000030000000000000002000d6000000000010109062803030303030303030303030303030303030303030303030303030303030303030303030303030303624adc215700000000000013d304040000000100000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000057570000000000000000000057575450010100005100000001000000030000000000000002000d6000000000010109062803030303030303030303030303030303030303030303030303030303030303030303030303030303624adc215700000000000013d3";
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect();
    let prefix = blocks(&bytes, Recovery::Prefix, Medium::File);
    let resync = blocks(&bytes, Recovery::Resync, Medium::File);
    assert_eq!(prefix.len(), 3);
    assert_eq!(resync.len(), 4);
    assert_eq!(resync[3], prefix[2], "the block past the gap");
    resync_reads_the_prefix(&bytes, Medium::File).unwrap();
}

/// A block of another session that may start one, inside a stretch the Prefix walk reads past as a gap: the Resync
/// walk hands it out, then goes on with the session's next block, where the Prefix walk resumes.
#[test]
fn resync_goes_on_with_the_session_past_a_gap() {
    let sd = Layout::UNALIGNED.on_sd_card();
    let card = session(NEW, SessionTag::NONE, sd, 10);
    let at = block_start(&card, 5);
    let others = [
        describe_blocks_max(sd, BlockId::new(OLD, 0), 4096).concat(),
        piece_head(OLD, SessionTag::NONE),
    ];
    for inside in others {
        let (torn, _, _) =
            with_stretch(&card, at, &[[0x5A; 77].as_slice(), &inside].concat(), true);
        for medium in [Medium::File, Medium::Raw] {
            let (got, _) = walk(&torn, medium);
            assert_eq!(got, seqs(NEW, 1..11), "{medium:?}");
            resync_reads_the_prefix(&torn, medium).unwrap();
        }
    }
}

/// A good block inside a gap whose bytes hold the block the Prefix walk resumes at: the Resync walk hands out both.
#[test]
fn resync_reads_the_block_a_gap_ends_at_inside_another() {
    let sd = Layout::UNALIGNED.on_sd_card();
    let card = session(NEW, SessionTag::NONE, sd, 10);
    let at = block_start(&card, 5);
    let resumed = records(BlockId::new(NEW, 5));
    let mut buf = [0u8; 512];
    let mut w = BlockWriter::new(
        &mut buf,
        BlockKind::Records,
        BlockId::new(NEW, 3),
        &[ClockNum::new(1)],
    )
    .unwrap();
    w.push(
        TrackNum::new(9),
        ClockTime::new(ClockNum::new(1), 3),
        &[&resumed],
    )
    .unwrap();
    let len = w.finish();
    let outer = buf[..len].to_vec();
    let inner = outer
        .windows(resumed.len())
        .position(|w| w == resumed.as_slice())
        .unwrap();
    let mut torn = card[..at].to_vec();
    torn.extend_from_slice(&[0x5A; 77]);
    while !(torn.len() + inner).is_multiple_of(512) {
        torn.push(0x5A);
    }
    torn.extend_from_slice(&outer);
    for medium in [Medium::File, Medium::Raw] {
        let (got, _) = walk(&torn, medium);
        assert_eq!(got, seqs(NEW, 1..6), "{medium:?}");
        resync_reads_the_prefix(&torn, medium).unwrap();
    }
}

/// A small xorshift generator: the same cases on every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Files of one or two sessions with random damage (flipped bytes, torn tails, blank or garbage stretches, some
/// holding a good block, some padded to a resume place): the Resync walk gives every block the Prefix walk gives,
/// in order, only blocks that read whole, and passes over none that reads whole.
#[test]
fn resync_reads_every_block_prefix_reads_in_damaged_files() {
    let small = Layout {
        gap_max: exact_units::Bytes::new(2048),
        resume_align: exact_units::Bytes::new(64),
        ..Layout::UNALIGNED
    };
    let layouts = [Layout::UNALIGNED, Layout::UNALIGNED.on_sd_card(), small];
    let mut rng = Rng(0x7e57_5a1e_0000_0001);
    for case in 0..20_000 {
        let layout = layouts[rng.below(layouts.len())];
        let align = layout.resume_align.get() as usize;
        let mut file = session(NEW, SessionTag::NONE, layout, 3 + rng.below(10) as u64);
        match rng.below(3) {
            0 => file.extend(session(OLD, NEW, layout, 1 + rng.below(6) as u64)),
            1 => {
                let first = BlockId::new(OLD, 0);
                file.extend(describe_blocks_max(layout, first, 4096).concat());
                file.extend(records(BlockId::new(OLD, 10)));
            }
            _ => {}
        }
        let pool = [
            describe_blocks_max(layout, BlockId::new(OLD, 0), 4096).concat(),
            piece_head(OLD, NEW),
            records(BlockId::new(NEW, rng.below(12) as u64)),
            records(BlockId::new(OLD, rng.below(12) as u64)),
            session(OLD, NEW, layout, rng.below(3) as u64),
        ];
        for _ in 0..1 + rng.below(4) {
            match rng.below(4) {
                0 => {
                    let at = rng.below(file.len());
                    file[at] ^= 1 << rng.below(8);
                }
                1 => file.truncate(rng.below(file.len()) + 1),
                _ => {
                    let at = rng.below(file.len());
                    let fill = [0x00, 0xFF, 0x5A][rng.below(3)];
                    let mut stretch = vec![fill; 1 + rng.below(300)];
                    if rng.below(2) == 0 {
                        stretch.extend_from_slice(&pool[rng.below(pool.len())]);
                        stretch.extend(vec![fill; rng.below(100)]);
                    }
                    let mut out = file[..at].to_vec();
                    out.extend_from_slice(&stretch);
                    if rng.below(2) == 0 {
                        while !out.len().is_multiple_of(align) {
                            out.push(fill);
                        }
                    }
                    out.extend_from_slice(&file[at..]);
                    file = out;
                }
            }
        }
        for medium in [Medium::File, Medium::Raw] {
            resync_reads_the_prefix(&file, medium).unwrap_or_else(|e| panic!("case {case}: {e}"));
            resync_misses_no_block(&file, medium).unwrap_or_else(|e| panic!("case {case}: {e}"));
        }
    }
}
