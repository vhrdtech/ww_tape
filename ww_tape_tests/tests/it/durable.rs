//! The code of the book page "Files and syncing" (`docs/files.md`), which includes the marked sections of this file:
//! a recorder writes a file under a lock, a second writer is refused, a quiet input still reaches the disk through
//! `tick`, the file takes its name at the close, and a later session appends a piece to it.

use std::error::Error;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use ww_tape::{Clock, ClockRef, SyncPolicy, TapeReader, WriteError};
use ww_tape_fs::FsError;

use crate::definitions::base_defs;
use crate::pieces::{records, scratch};

/// The host's clock, moved by the test.
#[derive(Clone, Default)]
struct HostClock(Arc<AtomicU64>);

impl Clock for HostClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

#[test]
fn a_file_written_durably_under_its_lock() -> Result<(), Box<dyn Error>> {
    let dir = scratch("durable");
    let path = dir.join("drive.wwt");
    let defs = base_defs();
    let clock = HostClock::default();

    // --8<-- [start:create]
    let mut writer =
        ww_tape_fs::writer::create(&path, SyncPolicy::Interval(Duration::from_secs(1)))?;
    writer.set_clock(ClockRef::new(clock.clone())); // the system's clock unless set
    // --8<-- [end:create]
    writer.set_session(crate::TEST_SESSION)?;
    writer.write_block(defs, &records(writer.next_id(), 0))?;

    // --8<-- [start:locked]
    let options = writer.options();
    let second = ww_tape_fs::writer::resume(&path, options);
    assert!(matches!(
        second,
        Err(WriteError::Sink {
            error: FsError::Locked(_),
            ..
        })
    ));
    // --8<-- [end:locked]

    // a log line, then nothing for two seconds: the line waits unsynced until a tick finds the interval up (the
    // test `a_tick_syncs_a_quiet_input_once_the_interval_is_up` counts the syncs)
    clock.0.fetch_add(400, Ordering::SeqCst);
    writer.write_block(defs, &records(writer.next_id(), 1))?;
    // --8<-- [start:tick]
    clock.0.fetch_add(2_000, Ordering::SeqCst); // the input is quiet
    writer.tick()?; // the interval is up: the waiting blocks are synced
    // --8<-- [end:tick]

    // --8<-- [start:close]
    let closed = writer.close()?; // final index, close, tail, two syncs, then the name
    assert!(path.exists());
    // --8<-- [end:close]

    // --8<-- [start:append]
    let mut writer = ww_tape_fs::writer::append(&path, SyncPolicy::default())?;
    writer.write_block(defs, &records(writer.next_id(), 2))?;
    let again = writer.close()?; // a second piece in the same file
    // --8<-- [end:append]

    let bytes = std::fs::read(&path)?;
    let r = TapeReader::open(&bytes[..])?;
    assert_eq!(r.pieces().len(), 2, "the append is a piece of its own");
    assert_eq!(r.closes(0)?[0].1.hash, closed.hash);
    assert_eq!(r.closes(1)?[0].1.hash, again.hash);
    std::fs::remove_dir_all(&dir)?;
    Ok(())
}
