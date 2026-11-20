//! Pieces removed on purpose: a record appended to the last piece of a chain, read by `check_chain`.

use crate::pieces::chain_in_memory;
use crate::reclose::close;
use ww_tape::piece::{ChainLink, ContentHash, PieceHead, PieceId, PieceIdent, RemovedPiece};
use ww_tape::sink::TapeSink;
use ww_tape::{
    InputError, MemSink, MemTapes, OwnerId, RemovedRefusal, SessionTag, SyncPolicy, TapeReader,
    TapeWriter, WriteError, WriterOptions,
};

/// The bytes of each piece of a chain in memory, in order.
fn pieces() -> Vec<Vec<u8>> {
    let mem = MemTapes::new();
    let keys = chain_in_memory(&mem);
    assert!(keys.len() >= 4, "{} pieces", keys.len());
    keys.iter()
        .map(|k| mem.get(k).unwrap().unwrap().to_vec().unwrap())
        .collect()
}

/// What a removed piece's record says of the piece `bytes`.
fn removed_of(bytes: &[u8]) -> RemovedPiece {
    let r = TapeReader::open(bytes).unwrap();
    let s = r.pieces()[0].summary().unwrap();
    RemovedPiece {
        seq: s.ident.seq,
        id: s.ident.id,
        hash: s.end.unwrap().1,
    }
}

/// A writer on the closed piece `bytes`, written on after its close.
fn reopen(bytes: &[u8]) -> TapeWriter<MemSink> {
    let mut sink = MemSink::new();
    sink.write_all(bytes).unwrap();
    let options = WriterOptions {
        sync: SyncPolicy::None,
        ..Default::default()
    };
    TapeWriter::resume_on(sink, options).unwrap().0
}

/// The last piece `last` with `removed` named, appended after its close.
fn record(last: &[u8], removed: &[RemovedPiece]) -> Vec<u8> {
    let mut w = reopen(last);
    w.record_removed(removed).unwrap();
    close(w)
}

/// Pieces 1 and 2 removed, the record appended to the last piece: the chain links over them, whole, and the last
/// piece verifies with both closes. Closed again with nothing new, it keeps its bytes; a later record names the earlier
/// pieces again.
#[test]
fn a_record_appended_to_the_last_piece_links_over_the_hole() {
    let p = pieces();
    let last = p.len() - 1;
    let gone = [removed_of(&p[1]), removed_of(&p[2])];
    let recorded = record(&p[last], &gone);
    assert!(recorded.len() > p[last].len());
    assert_eq!(
        recorded[..p[last].len()],
        p[last][..],
        "nothing before the append moves"
    );

    let r = TapeReader::open(&recorded[..]).unwrap();
    assert_eq!(r.closes(0).unwrap().len(), 2);
    assert_eq!(r.pieces()[0].removed, gone);
    assert!(r.verify(0).unwrap().holds);

    let mut rest = p[0].clone();
    for q in &p[3..last] {
        rest.extend_from_slice(q);
    }
    rest.extend_from_slice(&recorded);
    let c = TapeReader::open(&rest[..]).unwrap().chain();
    assert_eq!(c.links[0], ChainLink::Removed { pieces: 2 });
    assert!(
        c.links[1..].iter().all(|l| *l == ChainLink::Linked),
        "{:?}",
        c.links
    );
    assert_eq!(c.removed, gone);
    assert!(c.is_whole());

    // the same pieces named again, or nothing: no byte written
    let mut w = reopen(&recorded);
    assert_eq!(w.removed(), gone);
    w.record_removed(&gone[1..]).unwrap();
    assert_eq!(close(w), recorded);

    // without the record the same pieces are a gap
    let mut bare = p[0].clone();
    for q in &p[3..] {
        bare.extend_from_slice(q);
    }
    let c = TapeReader::open(&bare[..]).unwrap().chain();
    assert_eq!(c.links[0], ChainLink::Gap { missing: 2 });
    assert!(!c.is_whole());
}

/// A later record keeps the pieces named before: two appends, each naming one more piece.
#[test]
fn a_later_record_keeps_the_earlier_rows() {
    let p = pieces();
    let last = p.len() - 1;
    let once = record(&p[last], &[removed_of(&p[2])]);
    let twice = record(&once, &[removed_of(&p[1])]);
    let r = TapeReader::open(&twice[..]).unwrap();
    assert_eq!(r.closes(0).unwrap().len(), 3);
    assert_eq!(
        r.pieces()[0].removed,
        [removed_of(&p[1]), removed_of(&p[2])]
    );
    assert!(r.verify(0).unwrap().holds);
}

/// The chain's first pieces removed and named: removed, not head cut, and whole.
#[test]
fn a_head_cut_named_reads_as_removed() {
    let p = pieces();
    let last = p.len() - 1;
    let gone = [removed_of(&p[0]), removed_of(&p[1])];
    let mut rest: Vec<u8> = p[2..last].concat();
    rest.extend_from_slice(&record(&p[last], &gone));
    let c = TapeReader::open(&rest[..]).unwrap().chain();
    assert!(!c.head_cut);
    assert_eq!(c.head_removed, 2);
    assert!(c.is_whole());

    let unnamed = p[2..].concat();
    let c = TapeReader::open(&unnamed[..]).unwrap().chain();
    assert!(c.head_cut);
    assert_eq!(c.head_removed, 0);
}

/// A record the writer refuses leaves the writer as it was: a piece not before its own, one named with another hash,
/// a piece without a head.
#[test]
fn a_record_is_refused_where_no_chain_could_read_it() {
    let p = pieces();
    let last = p.len() - 1;
    let refusal = |e: WriteError<_>| match e {
        WriteError::Input(InputError::Removed(why)) => why,
        e => panic!("{e:?}"),
    };
    let mut w = reopen(&p[last]);
    let own = removed_of(&p[last]);
    assert_eq!(
        refusal(w.record_removed(&[own]).unwrap_err()),
        RemovedRefusal::NotBefore { seq: own.seq }
    );
    let one = removed_of(&p[1]);
    w.record_removed(&[one]).unwrap();
    let other = RemovedPiece {
        hash: ContentHash([1; 32]),
        ..one
    };
    assert_eq!(
        refusal(w.record_removed(&[removed_of(&p[2]), other]).unwrap_err()),
        RemovedRefusal::Conflict { seq: 1 }
    );
    assert_eq!(w.removed(), [one], "a refused call takes nothing");

    let mut plain = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    assert_eq!(
        refusal(plain.record_removed(&[one]).unwrap_err()),
        RemovedRefusal::NoChain
    );
}

/// The golden record: piece 3 of a chain, closed, written on after its close naming pieces 1 and 2 as removed, and
/// closed again; its close pins the bytes of the removed rows (`WW_TAPE_GOLDEN=update` rewrites it).
#[test]
fn golden_removed() {
    let o = crate::resume::options(0);
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    w.set_layout(o.layout).unwrap();
    w.set_producer(&o.producer);
    // a piece past a chain's first takes its session from the cut: fixed here, so the bytes are the same each run
    w.set_session(SessionTag::writable(0x7265_6d76)).unwrap();
    let owner = OwnerId([3; 32]);
    let id = |seq: u8| PieceId([seq + 1; 16]);
    let head = PieceHead {
        ident: PieceIdent {
            id: id(3),
            owner,
            seq: 3,
        },
        prev: Some((ContentHash([2; 32]), id(2))),
        prev_session: SessionTag::NONE,
        integrity: ww_tape::Integrity::Full,
    };
    w.begin_piece(&head, &[]).unwrap();
    crate::reclose::write(&mut w, 0..2);
    let closed = close(w);
    let (mut w, _) = crate::reclose::reopen(&closed, 0);
    let gone: Vec<RemovedPiece> = (1..3)
        .map(|seq| RemovedPiece {
            seq: u64::from(seq),
            id: id(seq),
            hash: ContentHash([seq; 32]),
        })
        .collect();
    w.record_removed(&gone).unwrap();
    let bytes = close(w);
    let r = TapeReader::open(&bytes[..]).unwrap();
    assert_eq!(r.pieces()[0].removed, gone);
    crate::reclose::golden(&bytes, "v1-removed.hex");
}

/// Files written before closes could name removed pieces (`data/before-removed/`, the fuzz seeds of that writer):
/// every close reads, with no rows, each piece verifies, and the chain of several pieces is whole.
#[test]
fn files_written_before_removed_rows_read() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/it/data/before-removed");
    for (name, closes) in [
        ("one-file-chain.wwt", None),
        ("closed-four-times.wwt", Some(4)),
        ("closed-with-a-close-summary.wwt", Some(3)),
        ("golden-v1-index-piece.wwt", None),
    ] {
        let bytes = std::fs::read(dir.join(name)).unwrap();
        let r = TapeReader::open(&bytes[..]).unwrap();
        for (i, p) in r.pieces().iter().enumerate() {
            assert!(
                p.close_error.is_none(),
                "{name} piece {i}: {:?}",
                p.close_error
            );
            assert!(p.close.is_some() && p.is_closed(), "{name} piece {i}");
            assert!(p.removed.is_empty(), "{name} piece {i}");
            assert!(r.verify(i).unwrap().holds, "{name} piece {i}");
        }
        if let Some(n) = closes {
            assert_eq!(r.closes(0).unwrap().len(), n, "{name}");
        }
        let c = r.chain();
        assert!(
            !matches!(c.end, ww_tape::piece::ChainEnd::Unclosed),
            "{name}: {c:?}"
        );
        if name == "one-file-chain.wwt" {
            assert!(c.is_whole(), "{name}: {c:?}");
        }
    }
}

/// Pieces 1 and 2 left out of the chain `p`, its last piece given as `last_bytes`.
fn without_1_2(p: &[Vec<u8>], last_bytes: &[u8]) -> Vec<u8> {
    let mut rest = p[0].clone();
    for q in &p[3..p.len() - 1] {
        rest.extend_from_slice(q);
    }
    rest.extend_from_slice(last_bytes);
    rest
}

/// A record torn at any byte of its append reads as an unclosed tail: no damage, no rows (the close kept in force
/// names none), one close, and the hole a gap.
#[test]
fn a_torn_record_is_an_unclosed_tail() {
    let p = pieces();
    let last = p.len() - 1;
    let recorded = record(&p[last], &[removed_of(&p[1]), removed_of(&p[2])]);
    for cut in p[last].len() + 1..recorded.len() {
        let torn = &recorded[..cut];
        let r = TapeReader::open(torn).unwrap_or_else(|e| panic!("cut {cut}: {e:?}"));
        let info = &r.pieces()[0];
        assert!(
            info.close_error.is_none(),
            "cut {cut}: {:?}",
            info.close_error
        );
        assert!(
            info.removed.is_empty(),
            "cut {cut}: rows read from a torn record"
        );
        assert!(
            r.unclosed(0).unwrap().is_some(),
            "cut {cut}: no unclosed tail"
        );
        assert_eq!(r.closes(0).unwrap().len(), 1, "cut {cut}");
        let c = TapeReader::open(&without_1_2(&p, torn)[..])
            .unwrap()
            .chain();
        assert_eq!(
            c.links[0],
            ChainLink::Gap { missing: 2 },
            "cut {cut}: {:?}",
            c.links
        );
    }
}

/// A cut after a removal: the cut close names the rows again and the next piece none. With the old last piece
/// removed too, rows it alone held are lost unless the new record names them again from `ChainReport::removed`.
#[test]
fn a_cut_after_a_removal_keeps_the_rows_where_named() {
    let p = pieces();
    let gone = [removed_of(&p[1]), removed_of(&p[2])];
    let recorded = record(&p[p.len() - 1], &gone);
    let w = reopen(&recorded);
    let tape = w.sink().tape();
    let (_, next) = w
        .cut_on(crate::definitions::base_defs(), MemSink::new())
        .unwrap();
    let cut_last = tape.to_vec().unwrap();
    let next_bytes = close(next);
    let r = TapeReader::open(&cut_last[..]).unwrap();
    assert_eq!(
        r.pieces()[0].removed,
        gone,
        "the cut close names the rows again"
    );
    let r = TapeReader::open(&next_bytes[..]).unwrap();
    assert!(
        r.pieces()[0].removed.is_empty(),
        "the next piece names none"
    );

    let mut all = without_1_2(&p, &cut_last);
    all.extend_from_slice(&next_bytes);
    let c = TapeReader::open(&all[..]).unwrap().chain();
    assert_eq!(
        c.links[0],
        ChainLink::Removed { pieces: 2 },
        "{:?}",
        c.links
    );
    assert!(c.is_whole(), "{c:?}");
    let named = c.removed.clone();

    // the old last piece removed too and named alone: pieces 1 and 2 are a gap again
    let old_last = removed_of(&cut_last);
    let before = without_1_2(&p, &[]);
    let mut lone = before.clone();
    lone.extend_from_slice(&record(&next_bytes, &[old_last]));
    let c = TapeReader::open(&lone[..]).unwrap().chain();
    assert!(matches!(c.links[0], ChainLink::Gap { .. }), "{:?}", c.links);
    assert!(!c.is_whole());

    // named with the rows ChainReport::removed gave: whole
    let mut rows = named;
    rows.push(old_last);
    let mut full = before;
    full.extend_from_slice(&record(&next_bytes, &rows));
    let c = TapeReader::open(&full[..]).unwrap().chain();
    assert!(c.is_whole(), "{:?}", c.links);
}

/// The writer takes `REMOVED_PIECES_MAX` removed pieces and refuses one more.
#[test]
fn the_writer_names_at_most_removed_pieces_max() {
    let max = ww_tape::REMOVED_PIECES_MAX;
    let owner = OwnerId([5; 32]);
    let head = PieceHead {
        ident: PieceIdent {
            id: PieceId([9; 16]),
            owner,
            seq: max as u64 + 10,
        },
        prev: Some((ContentHash([1; 32]), PieceId([8; 16]))),
        prev_session: SessionTag::NONE,
        integrity: ww_tape::Integrity::Full,
    };
    let row = |seq: u64| RemovedPiece {
        seq,
        id: PieceId([1; 16]),
        hash: ContentHash([2; 32]),
    };
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    w.begin_piece(&head, &[]).unwrap();
    let all: Vec<RemovedPiece> = (0..max as u64).map(row).collect();
    w.record_removed(&all).unwrap();
    let refused = w.record_removed(&[row(max as u64)]).unwrap_err();
    assert!(
        matches!(
            refused,
            WriteError::Input(InputError::Removed(RemovedRefusal::TooMany { max: m })) if m == max
        ),
        "{refused:?}"
    );
    assert_eq!(w.removed().len(), max, "a refused call takes nothing");
    let bytes = close(w);
    let r = TapeReader::open(&bytes[..]).unwrap();
    assert_eq!(r.pieces()[0].removed.len(), max);
}
