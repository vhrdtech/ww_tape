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
