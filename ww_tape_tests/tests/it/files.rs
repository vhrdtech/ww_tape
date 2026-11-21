//! Files written by path: a new file is created under its final name, an append goes on in place, and a failed write
//! ends the file.

use ww_tape::{FileBlocks, Recovery, SyncPolicy, WriteError};
use ww_tape_fs::FsError;

use crate::pieces::{records, scratch};

/// A new file has its final name from the start and keeps it at the close; it reads as unclosed until then. A
/// second create of the name is refused, open or closed, and an append adds a session the prefix walk reads after
/// the first.
#[test]
fn a_new_file_has_its_name_from_the_start() {
    let dir = scratch("files_named");
    let path = dir.join("cap.wwtape");
    let mut w = ww_tape_fs::writer::create(&path, SyncPolicy::EveryBlocks(2)).unwrap();
    assert_eq!(w.sink().path(), path);
    w.write_block(crate::definitions::base_defs(), &records(w.next_id(), 1))
        .unwrap();
    w.sync().unwrap();
    let unclosed = |path: &std::path::Path| {
        let r = ww_tape::TapeReader::open(ww_tape_fs::FileSource::open(path).unwrap()).unwrap();
        !r.pieces().iter().all(|p| p.is_closed())
    };
    assert!(unclosed(&path), "open until its close");
    assert!(matches!(
        ww_tape_fs::writer::create(&path, SyncPolicy::None),
        Err(WriteError::Sink {
            error: FsError::Exists(_),
            ..
        })
    ));
    w.close().unwrap();
    assert!(!unclosed(&path), "closed by its bytes");
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        1,
        "nothing beside it"
    );
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
    w.close().unwrap();
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
    assert!(matches!(w.close(), Err(WriteError::Failed)));
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
    let second = ww_tape_fs::writer::append(&path, SyncPolicy::None)
        .err()
        .unwrap();
    assert_eq!(
        chain(&second),
        [format!(
            "opening a writer to append: {}: locked by another writer",
            path.display()
        )]
    );
    let third = ww_tape_fs::writer::create(&path, SyncPolicy::None)
        .err()
        .unwrap();
    assert_eq!(
        chain(&third),
        [format!(
            "creating a writer: {}: already exists",
            path.display()
        )]
    );
    first.close().unwrap();
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

/// An append takes any bytes before it, checked for nothing: bytes that aren't blocks, a cut tape, a closed one. Its
/// session is a piece of its own after them, found by its tail; the reader shows the bytes before as an unclosed
/// piece, its good blocks up to the cut, or as the closed piece they are.
#[test]
fn an_append_follows_any_bytes_as_a_piece_of_its_own() {
    use ww_tape::TapeReader;
    let dir = scratch("files_append_any");
    let closed = dir.join("done.wwt");
    let mut w = ww_tape_fs::writer::create(&closed, SyncPolicy::None).unwrap();
    w.write_block(crate::definitions::base_defs(), &records(w.next_id(), 3))
        .unwrap();
    w.close().unwrap();
    let done = std::fs::read(&closed).unwrap();
    let cut = done[..done.len() / 2].to_vec();
    for (name, prefix) in [
        ("garbage", vec![0x5au8; 1000]),
        ("cut", cut),
        ("closed", done.clone()),
    ] {
        let path = dir.join(format!("{name}.wwt"));
        std::fs::write(&path, &prefix).unwrap();
        let mut w = ww_tape_fs::writer::append(&path, SyncPolicy::None).unwrap();
        w.write_block(crate::definitions::base_defs(), &records(w.next_id(), 2))
            .unwrap();
        w.close().unwrap();
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
                assert!(before.tail.is_none() && before.close.is_none());
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

/// A file replaced on disk by a rename under an open [FileSource](ww_tape_fs::FileSource): the open one keeps its
/// file's metadata (its length; on unix its device and inode), a new open of the path has the new file's.
#[test]
fn a_file_source_keeps_the_metadata_of_the_file_it_opened() {
    use ww_tape::ByteSource;
    let dir = scratch("files_identity");
    let path = dir.join("p.wwt");
    std::fs::write(&path, b"old").unwrap();
    let held = ww_tape_fs::FileSource::open(&path).unwrap();
    let before = held.metadata().unwrap();
    let new = dir.join("p.wwt.new");
    std::fs::write(&new, b"newer").unwrap();
    std::fs::rename(&new, &path).unwrap();
    let reopened = ww_tape_fs::FileSource::open(&path).unwrap();
    let (kept, now) = (held.metadata().unwrap(), reopened.metadata().unwrap());
    assert_eq!((kept.len(), now.len()), (3, 5));
    assert_eq!(held.size().unwrap(), 3);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!((kept.dev(), kept.ino()), (before.dev(), before.ino()));
        assert_ne!((kept.dev(), kept.ino()), (now.dev(), now.ino()));
    }
    #[cfg(not(unix))]
    assert_eq!(kept.len(), before.len());
    drop((held, reopened));
    std::fs::remove_dir_all(&dir).unwrap();
}
