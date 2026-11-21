//! The code of the book page "Pieces and chains" (`docs/pieces.md`), which includes the marked sections of this file:
//! a chain of files in a directory cut by size, its oldest piece deleted, and the pieces left read and checked.

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
            bytes: 16 << 10, // a new piece once one holds 16 KiB
            ..CutPolicy::default()
        },
        tapes: PieceTapes::Each, // a file per piece: 00000000.wwt, 00000001.wwt, ...
        writer: WriterOptions {
            index: IndexPolicy {
                bytes: 4 << 10,
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
