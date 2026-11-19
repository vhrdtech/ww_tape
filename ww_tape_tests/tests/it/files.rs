//! Files written by path: a new file is `NAME.partial` until its finish, an append goes on in place, and a failed
//! write ends the file.

use ww_tape::{FileBlocks, Recovery, SyncPolicy, WriteError};
use ww_tape_fs::FsError;

use crate::pieces::{records, scratch};

/// A new file is written as `NAME.partial` and takes its name at the finish; a second create of the name is refused,
/// and an append adds a session the prefix walk reads after the first.
#[test]
fn a_new_file_is_partial_until_its_finish() {
    let dir = scratch("files_partial");
    let path = dir.join("cap.wwtape");
    let partial = ww_tape_fs::partial_name(&path);
    let mut w = ww_tape_fs::writer::create(&path, SyncPolicy::EveryBlocks(2)).unwrap();
    assert_eq!(w.sink().current_path(), partial);
    w.write_block(crate::definitions::base_defs(), &records(w.next_id(), 1))
        .unwrap();
    assert!(!path.exists(), "named only when finished");
    assert!(partial.exists());
    w.finish().unwrap();
    assert!(path.exists(), "named when finished");
    assert!(!partial.exists());
    assert!(matches!(
        ww_tape_fs::writer::create(&path, SyncPolicy::None),
        Err(WriteError::Sink {
            error: FsError::Exists(_),
            ..
        })
    ));
    let first_len = std::fs::metadata(&path).unwrap().len();
    let mut w = ww_tape_fs::writer::append(&path, SyncPolicy::default()).unwrap();
    w.set_producer("second");
    w.tick().unwrap();
    w.finish().unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert!(bytes.len() as u64 > first_len, "appended in place");
    let mut rd = FileBlocks::with_recovery(&bytes[..], Recovery::Prefix);
    let mut blocks = 0;
    while let Some(b) = rd.next_block() {
        b.unwrap();
        blocks += 1;
    }
    assert!(blocks > 0);
    assert_eq!(
        rd.offset(),
        bytes.len() as u64,
        "both sessions read to the end"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A failed write is the end of the file: later writes and syncs are refused, never retried.
#[cfg(target_os = "linux")]
#[test]
fn a_writer_stops_after_a_failure() {
    let mut w =
        ww_tape_fs::writer::append(std::path::Path::new("/dev/full"), SyncPolicy::None).unwrap();
    let defs = crate::definitions::base_defs();
    // /dev/full takes nothing: the write fails with ENOSPC
    let first = w.write_block(defs, &records(w.next_id(), 1));
    assert!(
        matches!(
            first,
            Err(WriteError::Sink {
                error: FsError::Io { .. },
                ..
            })
        ),
        "{first:?}"
    );
    // each level of the error says its own part once: what the writer did, what the file did, the system's error
    let e = first.unwrap_err();
    let mut texts = vec![e.to_string()];
    let mut source = std::error::Error::source(&e);
    while let Some(s) = source {
        texts.push(s.to_string());
        source = s.source();
    }
    assert_eq!(texts.len(), 3, "{texts:?}");
    for pair in texts.windows(2) {
        assert!(!pair[0].contains(&pair[1]), "{texts:?}");
    }
    assert!(matches!(
        w.write_block(defs, &records(w.next_id(), 2)),
        Err(WriteError::Failed)
    ));
    assert!(matches!(w.sync(), Err(WriteError::Failed)));
    assert!(matches!(w.finish(), Err(WriteError::Failed)));
}
