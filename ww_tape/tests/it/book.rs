//! The code shown on the book pages (`docs/index.md`, `docs/writing.md`), run as written there.

use std::path::Path;

use ww_tape::{
    BlockKind, BlockWriter, ClockTime, Definitions, MemSink, SyncPolicy, TapeWriter, WriteError,
};

use crate::definitions::{CLOCK, clock, state, track};

const MODE: u32 = 5;

/// The lines of "What the application does with its definitions", between the setup and the checks.
fn example(writer: &mut TapeWriter<MemSink>) -> Result<(), WriteError> {
    let (clock, temperature_track, mode_track) = (clock(CLOCK), track(1, CLOCK), state(MODE));
    let ticks = 1_000;
    let mode_bytes = b"mode: run".to_vec();
    let mut buf = [0u8; 128];
    let mut b = BlockWriter::new(&mut buf, BlockKind::Records, writer.next_id(), &[CLOCK]).unwrap();
    b.push(1, ClockTime::new(CLOCK, ticks), &[&21i16.to_le_bytes()])
        .unwrap();
    let n = b.finish();
    let readings = buf[..n].to_vec();

    // as on the pages
    let mut defs = Definitions::new();
    defs.define_all(&[clock, temperature_track, mode_track])?;
    writer.put(&defs, MODE, ticks, &mode_bytes)?; // a state item, at its use
    writer.write_block(&defs, &readings)?; // the definitions it needs go first
    Ok(())
}

#[test]
fn book_definitions_example() {
    let mut writer = TapeWriter::create_on(MemSink::new(Path::new("book")), SyncPolicy::None);
    example(&mut writer).unwrap();
    let tape = writer.sink().tape();
    writer.finish().unwrap();
    let bytes = tape.to_vec().unwrap();
    let r = ww_tape::TapeReader::open(&bytes[..]).unwrap();
    let defs = r.definitions_at(0, u64::MAX).unwrap();
    assert!(defs.is_whole(), "every definition reads: {defs:?}");
}

/// The module docs of every source file under `dir`, `//!` stripped, one after another.
fn module_docs(dir: &std::path::Path, out: &mut String) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            module_docs(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            for line in std::fs::read_to_string(&path).unwrap().lines() {
                if let Some(doc) = line.strip_prefix("//!") {
                    out.push_str(doc.strip_prefix(' ').unwrap_or(doc));
                    out.push('\n');
                }
            }
        }
    }
}

/// Every byte layout `docs/format.md` shows (its `text` blocks) is a layout of the module docs, word for word.
#[test]
fn format_page_layouts_are_the_module_docs() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut docs = String::new();
    module_docs(&root.join("src"), &mut docs);
    let page = std::fs::read_to_string(root.join("../docs/format.md")).unwrap();
    let mut blocks = 0;
    for part in page.split("```text\n").skip(1) {
        let block = part.split("```").next().unwrap();
        assert!(
            docs.contains(block),
            "docs/format.md shows a layout the module docs don't have:\n{block}"
        );
        blocks += 1;
    }
    assert!(blocks >= 8, "the layouts of every block kind are on the page");
}
