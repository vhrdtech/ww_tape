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
    // the top line says what the writer and the file did and the kind; the system's error comes next, once
    let texts = chain(&first.unwrap_err());
    assert_eq!(texts.len(), 2, "{texts:?}");
    assert_eq!(
        texts[0],
        "writing a block: /dev/full: writing the file: no storage space"
    );
    assert!(!texts[0].contains(&texts[1]), "{texts:?}");
    assert!(matches!(
        w.write_block(defs, &records(w.next_id(), 2)),
        Err(WriteError::Failed)
    ));
    assert!(matches!(w.sync(), Err(WriteError::Failed)));
    assert!(matches!(w.finish(), Err(WriteError::Failed)));
}

/// The texts of an error and its sources, top first.
fn chain(e: &dyn std::error::Error) -> Vec<String> {
    let mut texts = vec![e.to_string()];
    let mut source = e.source();
    while let Some(s) = source {
        texts.push(s.to_string());
        source = s.source();
    }
    texts
}

/// A file a second writer opens: its top line names what was done, the file and the refusal, once, with nothing
/// below it; a log line of the top line alone says it all.
#[test]
fn a_refused_writer_says_the_file_and_why_in_one_line() {
    let dir = scratch("files_texts");
    let path = dir.join("x.wwt");
    let first = ww_tape_fs::writer::create(&path, SyncPolicy::None).unwrap();
    let second = ww_tape_fs::writer::create(&path, SyncPolicy::None)
        .err()
        .unwrap();
    assert_eq!(
        chain(&second),
        [format!(
            "creating a writer: {}: locked by another writer",
            path.display()
        )]
    );
    first.finish().unwrap();
    let again = ww_tape_fs::writer::append(&dir, SyncPolicy::None)
        .err()
        .unwrap();
    assert_eq!(
        chain(&again),
        [format!(
            "opening a writer to append: {}: not a regular file",
            dir.display()
        )]
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A resume with nothing to resume, the file or its directory missing, says so typed, naming the file: a caller tells
/// a missing piece from a failure.
#[test]
fn a_resume_of_no_file_names_the_file() {
    let dir = scratch("files_no_piece");
    for path in [dir.join("x.wwt"), dir.join("gone").join("x.wwt")] {
        let e = ww_tape_fs::writer::resume(&path, ww_tape::WriterOptions::default())
            .err()
            .unwrap();
        assert!(
            matches!(&e, WriteError::Sink { error: FsError::NoPiece(p), .. } if *p == path),
            "{e}"
        );
        assert_eq!(
            chain(&e),
            [format!(
                "opening a writer to resume: {}: no piece to resume",
                path.display()
            )]
        );
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A chain resumed where there is none, its directory missing or empty, or its one file missing, says so typed,
/// naming the directory or file, apart from a failure.
#[test]
fn a_resume_of_no_chain_names_the_directory() {
    let dir = scratch("files_no_chain");
    let owner = ww_tape::OwnerId([3; 32]);
    let each = ww_tape::ChainOptions::default();
    let one = ww_tape::ChainOptions {
        tapes: ww_tape::PieceTapes::One,
        ..Default::default()
    };
    let empty = dir.join("empty");
    std::fs::create_dir(&empty).unwrap();
    for (at, options) in [
        (dir.join("gone"), each.clone()),
        (empty, each),
        (dir.join("one.wwt"), one),
    ] {
        let e = ww_tape_fs::chain::resume(&at, owner, 0, options)
            .err()
            .unwrap();
        assert!(
            matches!(&e, WriteError::Sink { error: FsError::NoPiece(p), .. } if *p == at),
            "{e}"
        );
        assert_eq!(
            chain(&e),
            [format!(
                "opening a chain to resume: {}: no piece to resume",
                at.display()
            )]
        );
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// An append takes any bytes before it, checked for nothing: bytes that aren't blocks, a cut tape, a finished one. Its
/// session is a piece of its own after them, found by its tail; the reader shows the bytes before as an unfinished
/// piece, its good blocks up to the cut, or as the finished piece they are.
#[test]
fn an_append_follows_any_bytes_as_a_piece_of_its_own() {
    use ww_tape::TapeReader;
    let dir = scratch("files_append_any");
    let finished = dir.join("done.wwt");
    let mut w = ww_tape_fs::writer::create(&finished, SyncPolicy::None).unwrap();
    w.write_block(crate::definitions::base_defs(), &records(w.next_id(), 3))
        .unwrap();
    w.finish().unwrap();
    let done = std::fs::read(&finished).unwrap();
    let cut = done[..done.len() / 2].to_vec();
    for (name, prefix) in [
        ("garbage", vec![0x5au8; 1000]),
        ("cut", cut),
        ("finished", done.clone()),
    ] {
        let path = dir.join(format!("{name}.wwt"));
        std::fs::write(&path, &prefix).unwrap();
        let mut w = ww_tape_fs::writer::append(&path, SyncPolicy::None).unwrap();
        w.write_block(crate::definitions::base_defs(), &records(w.next_id(), 2))
            .unwrap();
        w.finish().unwrap();
        let r = TapeReader::open(ww_tape_fs::FileSource::open(&path).unwrap()).unwrap();
        let [before, appended] = r.pieces() else {
            panic!("{name}: {:?}", r.pieces());
        };
        let len = prefix.len() as u64;
        assert_eq!(appended.start, len, "{name}");
        assert!(appended.tail.is_some() && appended.head.is_none(), "{name}");
        assert_eq!(before.start, 0, "{name}");
        match name {
            "garbage" => assert_eq!((before.end, before.cut), (0, Some(0))),
            "cut" => {
                assert!(before.tail.is_none() && before.finish.is_none());
                assert!(
                    matches!(before.cut, Some(c) if c > 0 && c < len),
                    "{before:?}"
                );
            }
            _ => assert!(before.tail.is_some() && before.end == len, "{before:?}"),
        }
    }
    std::fs::remove_dir_all(&dir).unwrap();
}
