//! The code of the book page "When the power fails" (`docs/crashes.md`), which includes the marked sections of this
//! file: a recorder writes a file, the power fails in the middle of a block, a reader opens what is left, and the
//! recorder resumes the file and gives its lost blocks again. The test checks that the file then holds the bytes of
//! one written without the crash.

use std::error::Error;
use std::path::{Path, PathBuf};

use ww_tape::{Definitions, FileBlocks, Recovery, SyncPolicy, TapeReader, TapeWriter};
use ww_tape_fs::FileSink;

use crate::definitions::base_defs;
use crate::pieces::{records, scratch};

/// The recorder of the page: it keeps every block it gave the writer, with the block's seq, so that it can give a
/// block again after a crash.
struct Recorder {
    defs: Definitions,
    sent: Vec<(u64, Vec<u8>)>,
}

impl Recorder {
    fn new() -> Self {
        Self {
            defs: base_defs().clone(),
            sent: Vec::new(),
        }
    }

    /// Write log line `n` as a records block.
    fn log(&mut self, writer: &mut TapeWriter<FileSink>, n: i64) -> Result<(), Box<dyn Error>> {
        let block = records(writer.next_id(), n);
        self.sent.push((writer.next_seq(), block.clone()));
        writer.write_block(&self.defs, &block)?;
        Ok(())
    }
}

fn partial(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(".partial");
    PathBuf::from(p)
}

/// 400 log lines into a new file at `path`, left unclosed as a crash leaves it; the recorder that wrote them.
fn record(path: &Path) -> Result<Recorder, Box<dyn Error>> {
    let mut recorder = Recorder::new();
    // --8<-- [start:create]
    let mut writer = ww_tape_fs::writer::create(path, SyncPolicy::default())?;
    let options = writer.options(); // kept: a resume needs the same options
    // --8<-- [end:create]
    writer.set_session(crate::TEST_SESSION)?;
    for n in 0..400 {
        recorder.log(&mut writer, n)?;
    }
    writer.sync()?;
    assert_eq!(options, writer.options());
    drop(writer);
    Ok(recorder)
}

/// The file written whole, for the comparison.
fn whole(path: &Path) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut recorder = Recorder::new();
    let mut writer = ww_tape_fs::writer::create(path, SyncPolicy::default())?;
    writer.set_session(crate::TEST_SESSION)?;
    for n in 0..400 {
        recorder.log(&mut writer, n)?;
    }
    writer.close()?;
    Ok(std::fs::read(path)?)
}

#[test]
fn power_fails_and_the_recorder_resumes() -> Result<(), Box<dyn Error>> {
    let dir = scratch("crashes");
    let want = whole(&dir.join("whole.wwt"))?;
    let path = dir.join("capture.wwt");
    let mut recorder = record(&path)?;
    let options = ww_tape::WriterOptions::default();

    // the power fails in the middle of the last block: cut the file there
    let bytes = std::fs::read(partial(&path))?;
    let mut walk = FileBlocks::with_recovery(&bytes[..], Recovery::Prefix);
    let mut last = 0;
    while let Some(Ok(_)) = walk.next_block() {
        last = walk.offset();
    }
    let torn_at = last;
    std::fs::OpenOptions::new()
        .write(true)
        .open(partial(&path))?
        .set_len(torn_at + 20)?;

    // --8<-- [start:read]
    let bytes = std::fs::read(dir.join("capture.wwt.partial"))?;
    let reader = TapeReader::open(&bytes[..])?;
    let piece = &reader.pieces()[0];
    assert!(piece.tail.is_none()); // never closed
    let end = piece.cut.expect("where the good blocks end"); // the torn block's offset
    // --8<-- [end:read]
    assert_eq!(end, torn_at);

    // --8<-- [start:resume]
    let (mut writer, resumed) = ww_tape_fs::writer::resume(&path, options)?;
    assert_eq!(resumed.kept_bytes, end); // the torn block is cut off
    let lost = writer.next_seq(); // the first block the file lacks
    for (_, block) in recorder.sent.iter().filter(|(seq, _)| *seq >= lost) {
        writer.write_block(&recorder.defs, block)?;
    }
    writer.close()?; // capture.wwt.partial becomes capture.wwt
    // --8<-- [end:resume]

    assert_eq!(
        std::fs::read(&path)?,
        want,
        "the same bytes as without the crash"
    );
    recorder.sent.clear();
    Ok(())
}
