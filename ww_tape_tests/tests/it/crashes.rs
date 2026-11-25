//! The code of the book pages "When the power fails" (`docs/crashes.md`) and "Beyond a torn block"
//! (`docs/resume_cases.md`), which include the marked sections of this file: a recorder writes a file, the power
//! fails in the middle of a block, a reader opens what is left, and the recorder resumes the file and gives its lost
//! blocks again. The tests check that the file then holds the bytes of one written without the crash, also when the
//! crash tears a group of puts, and at every byte of a small piece.

use std::error::Error;
use std::path::Path;

use ww_tape::{
    Definitions, Describe, FileBlocks, GroupItem, GroupPut, MemSink, Recovery, SyncPolicy,
    TapeReader, TapeSink, TapeWriter, TrackNum,
};
use ww_tape_fs::FileSink;

use crate::definitions::{base_defs, state};
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
    let bytes = std::fs::read(&path)?;
    let mut walk = FileBlocks::with_recovery(&bytes[..], Recovery::Prefix);
    let mut last = 0;
    while let Some(Ok(_)) = walk.next_block() {
        last = walk.offset();
    }
    let torn_at = last;
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)?
        .set_len(torn_at + 20)?;

    // --8<-- [start:read]
    let bytes = std::fs::read(dir.join("capture.wwt"))?;
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
    writer.close()?; // capture.wwt now ends in a close and a tail
    // --8<-- [end:resume]

    assert_eq!(
        std::fs::read(&path)?,
        want,
        "the same bytes as without the crash"
    );
    recorder.sent.clear();
    Ok(())
}

/// The state tracks of a sensor's calibration: the tables of its two axes and their version.
fn calibration() -> Result<(Definitions, [TrackNum; 3]), Box<dyn Error>> {
    let mut defs = base_defs().clone();
    let mut tracks = Vec::new();
    for n in 10..13 {
        let Describe::Track(def) = state(n) else {
            unreachable!("state() gives a track")
        };
        tracks.push(defs.define_track(def.spec())?);
    }
    let tracks = tracks.try_into().expect("three tracks");
    Ok((defs, tracks))
}

/// A writer of `defs` that has put the first calibration group: the writer, its tape and where the next group starts.
fn calibrated(
    defs: &Definitions,
    group: &[GroupPut<'_>],
) -> Result<(TapeWriter<MemSink>, ww_tape::MemPiece, u64), Box<dyn Error>> {
    let mut writer = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    writer.set_session(crate::TEST_SESSION)?;
    let tape = writer.sink().tape();
    let first = [
        GroupPut {
            item: GroupItem::State(&[1; 64]),
            ..group[0]
        },
        GroupPut {
            item: GroupItem::State(&[2; 64]),
            ..group[1]
        },
        GroupPut {
            item: GroupItem::State(b"v1"),
            ..group[2]
        },
    ];
    writer.put_group(defs, &first)?;
    let start = tape.to_vec()?.len() as u64;
    Ok((writer, tape, start))
}

/// A crash inside a group of puts: the resume drops the blocks of the group that reached the disk and says so, the
/// application gives the group again, and the piece then holds the bytes of one written without the crash.
#[test]
fn a_torn_group_is_dropped_and_given_again() -> Result<(), Box<dyn Error>> {
    let (defs, [x_track, y_track, version_track]) = calibration()?;
    let (x_table, y_table) = (vec![7u8; 40 << 10], vec![8u8; 40 << 10]);
    // --8<-- [start:torn_group]
    // New tables of both axes and their version, as one group: a reader sees all three or none.
    let at = |track, item| GroupPut {
        track,
        ticks: 1_000,
        item: GroupItem::State(item),
    };
    let group = [
        at(x_track, &x_table),
        at(y_track, &y_table),
        at(version_track, b"v2"),
    ];
    // --8<-- [end:torn_group]
    crash_in_group(&defs, &group)?;
    Ok(())
}

/// [a_torn_group_is_dropped_and_given_again] past its shown lines: `group` torn by a crash, the resume, the group
/// given again.
fn crash_in_group(defs: &Definitions, group: &[GroupPut<'_>]) -> Result<(), Box<dyn Error>> {
    let (mut whole, whole_tape, start) = calibrated(defs, group)?;
    whole.put_group(defs, group)?;
    let want = whole_tape.to_vec()?;
    let mut blocks: Vec<u64> = TapeReader::open(&want[..])?
        .entries(0)?
        .iter()
        .map(|e| e.offset)
        .filter(|at| *at >= start)
        .collect();
    blocks.dedup();
    let blocks = blocks.len();
    assert!(blocks >= 2, "the group takes {blocks} blocks");

    let (mut writer, tape, _) = calibrated(defs, group)?;
    writer.put_group(defs, group)?;
    let options = writer.options();
    let bytes = tape.to_vec()?;
    drop(writer);
    // the power fails in the group's last block
    let mut sink = MemSink::new();
    sink.write_all(&bytes[..bytes.len() - 100])?;
    let tape = sink.tape();
    // --8<-- [start:torn_group_resume]
    let (mut writer, resumed) = TapeWriter::resume_on(sink, options)?;
    let dropped = resumed
        .dropped_run
        .expect("the group's blocks before the torn one");
    assert_eq!(dropped.start, resumed.kept_bytes); // dropped from where the kept bytes end
    writer.put_group(defs, group)?; // given again, whole
    // --8<-- [end:torn_group_resume]
    assert_eq!(
        resumed.kept_bytes, start,
        "the resume keeps the first group only"
    );
    assert_eq!(tape.to_vec()?, want, "the same bytes as without the crash");
    Ok(())
}

/// A crash rehearsed at every byte of a small piece: each prefix resumes, the lost calls given again, into the
/// bytes of the piece written whole.
#[test]
fn a_crash_at_every_byte_resumes_into_the_same_piece() -> Result<(), Box<dyn Error>> {
    let defs = base_defs();
    let write = |writer: &mut TapeWriter<MemSink>, from: u64| -> Result<(), Box<dyn Error>> {
        for n in from..20 {
            let n = i64::try_from(n)?;
            writer.write_block(defs, &records(writer.next_id(), n))?;
        }
        Ok(())
    };
    let mut writer = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    writer.set_session(crate::TEST_SESSION)?;
    let tape = writer.sink().tape();
    writer.write_block(defs, &records(writer.next_id(), 0))?;
    // from the end of the first block of data on, the file names its session
    let first = tape.to_vec()?.len();
    write(&mut writer, 1)?;
    let options = writer.options();
    writer.close()?;
    let whole = tape.to_vec()?;
    let mut resumed = 0;
    // --8<-- [start:every_byte]
    for cut in first..whole.len() {
        let mut sink = MemSink::new();
        sink.write_all(&whole[..cut])?; // what the crash left
        let tape = sink.tape();
        let (mut writer, _) = TapeWriter::resume_on(sink, options.clone())?;
        let lost = writer.next_seq(); // the first block the prefix lacks
        write(&mut writer, lost)?; // the calls given again
        writer.close()?;
        assert_eq!(tape.to_vec()?, whole, "a crash at byte {cut}");
        resumed += 1;
    }
    // --8<-- [end:every_byte]
    assert_eq!(resumed, whole.len() - first);
    Ok(())
}
