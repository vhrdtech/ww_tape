//! The code of the book page "Pieces and chains" (`docs/pieces.md`), which includes the marked sections of this file:
//! a chain of files in a directory cut by size, its oldest piece deleted, and the pieces left read and checked; the
//! pieces renamed and mixed up, ordered again by a scan of their ends.

use std::error::Error;

use ww_tape::index::IndexPolicy;
use ww_tape::piece::{ChainLink, check_chain};
use ww_tape::{ChainOptions, CutPolicy, OwnerId, PieceTapes, TapeReader, WriterOptions};

use crate::definitions::base_defs;
use crate::pieces::{chunk, column, scratch};

#[test]
fn a_chain_of_files_cut_by_size_and_its_oldest_piece_deleted() -> Result<(), Box<dyn Error>> {
    let dir = scratch("chains_book");
    let owner = OwnerId([0xd7; 32]);
    let defs = base_defs();

    // --8<-- [start:create]
    let options = ChainOptions {
        cut: CutPolicy {
            bytes: exact_units::Bytes::new(16 << 10), // a new piece once one holds 16 KiB
            ..CutPolicy::default()
        },
        tapes: PieceTapes::Each, // a file per piece: 00000000.wwt, 00000001.wwt, ...
        writer: WriterOptions {
            index: IndexPolicy {
                bytes: exact_units::Bytes::new(4 << 10),
                ..IndexPolicy::default()
            },
            ..WriterOptions::default()
        },
    };
    let mut chain = ww_tape_fs::chain::create(&dir, owner, 0, &[], options)?;
    for k in 0..120 {
        let (head, starts) = chunk(0, k);
        chain.write_chunk(defs, &head, &column(&starts))?; // cuts first when the piece is full
    }
    let pieces = chain.close()?;
    // --8<-- [end:create]
    assert!(pieces.len() >= 3, "{} pieces", pieces.len());

    // --8<-- [start:delete]
    std::fs::remove_file(dir.join(ww_tape_fs::chain::piece_name(0)))?; // retention: the oldest piece goes
    let mut summaries = Vec::new();
    for seq in 1..pieces.len() as u64 {
        let bytes = std::fs::read(dir.join(ww_tape_fs::chain::piece_name(seq)))?;
        let reader = TapeReader::open(&bytes[..])?; // each piece reads alone
        summaries.extend(reader.pieces().iter().filter_map(|p| p.summary()));
    }
    let report = check_chain(&summaries);
    assert!(report.head_cut); // the start was deleted, not damaged
    assert!(report.links.iter().all(|l| *l == ChainLink::Linked));
    // --8<-- [end:delete]
    assert_eq!(report.links.len(), pieces.len() - 2);
    std::fs::remove_dir_all(&dir)?;
    Ok(())
}

/// Pieces renamed and mixed up still form their chain: each piece's head and close say which chain it belongs to,
/// where in it it lies and which items it holds, so a scan of the files' ends orders them and shows what is where.
#[test]
fn renamed_pieces_still_form_their_chain() -> Result<(), Box<dyn Error>> {
    let dir = scratch("chains_renamed");
    let owner = OwnerId([0xd8; 32]);
    let defs = base_defs();
    let options = ChainOptions {
        cut: CutPolicy {
            bytes: exact_units::Bytes::new(16 << 10),
            ..CutPolicy::default()
        },
        ..ChainOptions::default()
    };
    let mut chain = ww_tape_fs::chain::create(&dir, owner, 0, &[], options)?;
    for k in 0..120 {
        let (head, starts) = chunk(0, k);
        chain.write_chunk(defs, &head, &column(&starts))?;
    }
    let pieces = chain.close()?.len() as u64;
    assert_eq!(pieces, 4);

    // --8<-- [start:renamed]
    // Copied by hand under names that sort in the wrong order: piece 0 becomes d.wwt, piece 3 a.wwt.
    let mixed = scratch("chains_mixed");
    for seq in 0..pieces {
        let name = ["d.wwt", "c.wwt", "b.wwt", "a.wwt"][seq as usize];
        std::fs::copy(
            dir.join(ww_tape_fs::chain::piece_name(seq)),
            mixed.join(name),
        )?;
    }

    // A scan opens each file from its end and asks the piece where it belongs and what it holds.
    let mut found = Vec::new(); // (file, piece number, the items of the track)
    let mut summaries = Vec::new();
    for file in std::fs::read_dir(&mixed)? {
        let path = file?.path();
        let reader = TapeReader::open(ww_tape_fs::FileSource::open(&path)?)?;
        let piece = &reader.pieces()[0];
        let summary = piece.summary().ok_or("a closed piece")?;
        let (_, ranges) = piece.close.as_ref().ok_or("a close")?;
        let items = ranges[0].first_item.get()..ranges[0].end_item().get();
        found.push((
            path.file_name().ok_or("a name")?.to_owned(),
            summary.ident.seq,
            items,
        ));
        summaries.push(summary);
    }

    // The heads put the pieces in order, whatever their names, and every link holds.
    let report = check_chain(&summaries);
    assert!(report.is_whole());
    found.sort_by_key(|(_, seq, _)| *seq);
    assert_eq!(
        found,
        [
            ("d.wwt".into(), 0, 0..1600),
            ("c.wwt".into(), 1, 1600..3150),
            ("b.wwt".into(), 2, 3150..4700),
            ("a.wwt".into(), 3, 4700..6000),
        ]
    );
    // --8<-- [end:renamed]
    std::fs::remove_dir_all(&dir)?;
    std::fs::remove_dir_all(&mixed)?;
    Ok(())
}
