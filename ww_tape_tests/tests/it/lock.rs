//! One writer per file across processes: a file sink's lock on the open file holds against another process, goes
//! with a crash, stays out of a forked child, and readers of the file in the writer's process don't let it go.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use ww_tape::SyncPolicy;
use ww_tape_fs::FileSink;

use crate::pieces::scratch;

/// The file [lock_child] opens, when the test binary runs as one.
const CHILD_ENV: &str = "WW_TAPE_LOCK_CHILD";
/// How [lock_child] opens it: `reopen` (the file there), `append`, or `hold` (create it, write, report, wait).
const CHILD_OP_ENV: &str = "WW_TAPE_LOCK_OP";

/// Not a test of its own: the body of the process [try_in_child] and [hold_in_child] start. It opens the file named
/// in [CHILD_ENV] as [CHILD_OP_ENV] says and prints whether its lock was free; `hold` writes four blocks, syncs,
/// prints `held` and waits to be killed.
#[test]
#[expect(
    clippy::print_stdout,
    reason = "the child reports its verdict to the test that started it"
)]
fn lock_child() {
    let Some(path) = std::env::var_os(CHILD_ENV) else {
        return;
    };
    let path = Path::new(&path);
    let opened = match std::env::var(CHILD_OP_ENV).as_deref() {
        Ok("hold") => {
            let mut w = ww_tape_fs::writer::create(path, SyncPolicy::None).unwrap();
            for n in 0..4 {
                let block = crate::pieces::records(w.next_id(), n);
                w.write_block(crate::definitions::base_defs(), &block)
                    .unwrap();
            }
            w.sync().unwrap();
            println!("lock-child: held");
            loop {
                std::thread::park();
            }
        }
        Ok("append") => FileSink::append(path).map(Some),
        _ => FileSink::reopen(path),
    };
    let verdict = match opened {
        Ok(Some(_)) => "free",
        Ok(None) => "missing",
        Err(ww_tape_fs::FsError::Locked(_)) => "locked",
        Err(e) => panic!("{e}"),
    };
    println!("lock-child: {verdict}");
}

/// The command that runs [lock_child] on `path` as `op`.
fn child(path: &Path, op: &str) -> Command {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["lock::lock_child", "--exact", "--nocapture"])
        .env_clear()
        .env(CHILD_ENV, path)
        .env(CHILD_OP_ENV, op)
        .stdin(Stdio::null());
    cmd
}

/// Whether another process finds the lock of the file `path` free, held or the file missing.
fn try_in_child(path: &Path) -> String {
    try_op_in_child(path, "reopen")
}

/// Whether another process that opens `path` by `op` (`reopen` or `append`) finds its lock free or held.
fn try_op_in_child(path: &Path, op: &str) -> String {
    let out = child(path, op).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
        .lines()
        .find_map(|l| l.strip_prefix("lock-child: "))
        .unwrap_or_else(|| panic!("no verdict from the child: {stdout}"))
        .to_owned()
}

/// A process writing a new file at `path`, which holds its lock until killed.
fn hold_in_child(path: &Path) -> std::process::Child {
    let mut held = child(path, "hold").stdout(Stdio::piped()).spawn().unwrap();
    let mut out = BufReader::new(held.stdout.take().unwrap());
    let mut line = String::new();
    while !line.contains("lock-child: held") {
        line.clear();
        assert_ne!(out.read_line(&mut line).unwrap(), 0, "the child ended");
    }
    held
}

/// The names in `dir`, sorted.
fn names_in(dir: &Path) -> Vec<PathBuf> {
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| PathBuf::from(e.unwrap().file_name()))
        .collect();
    names.sort();
    names
}

/// A writer's lock holds against another process while the writer's process opens and closes the file it writes,
/// and is free once the writer is dropped. Nothing but the file is ever beside it.
#[test]
fn a_writer_holds_its_file_against_another_process() {
    let dir = scratch("lock_process");
    let path = dir.join("a.wwt");
    let w = ww_tape_fs::writer::create(&path, SyncPolicy::None).unwrap();
    assert_eq!(names_in(&dir), [PathBuf::from("a.wwt")]);
    // readers in the writer's process open and close the file: a record lock on it would go with the first close
    for _ in 0..3 {
        drop(std::fs::File::open(w.sink().path()).unwrap());
    }
    assert_eq!(try_in_child(&path), "locked");
    assert_eq!(try_op_in_child(&path, "append"), "locked");
    drop(w);
    assert_eq!(try_in_child(&path), "free");
    assert_eq!(names_in(&dir), [PathBuf::from("a.wwt")]);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A writer in another process holds the file against this one; once that process crashes (killed while it holds
/// the lock), the lock is gone with it, nothing is left beside the file, and a resume here goes on writing it.
#[test]
fn a_crash_while_locked_leaves_nothing_behind() {
    let dir = scratch("lock_crash");
    let path = dir.join("a.wwt");
    let mut held = hold_in_child(&path);
    let r = ww_tape_fs::writer::resume(&path, ww_tape::WriterOptions::default());
    assert!(
        matches!(
            &r,
            Err(ww_tape::WriteError::Sink {
                error: ww_tape_fs::FsError::Locked(_),
                ..
            })
        ),
        "{:?}",
        r.err()
    );
    let r = FileSink::create(&path);
    assert!(
        matches!(&r, Err(ww_tape_fs::FsError::Exists(_))),
        "{:?}",
        r.err()
    );
    // SIGKILL: the crash, no core dump
    held.kill().unwrap();
    held.wait().unwrap();
    assert_eq!(names_in(&dir), [PathBuf::from("a.wwt")]);
    let (w, resumed) =
        ww_tape_fs::writer::resume(&path, ww_tape::WriterOptions::default()).unwrap();
    assert!(resumed.kept_bytes > 0, "{resumed:?}");
    w.close().unwrap();
    assert_eq!(names_in(&dir), [PathBuf::from("a.wwt")]);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// One file has one lock whatever name it is reached by: a link to it or a hard link, in this process and in
/// another.
#[cfg(unix)]
#[test]
fn one_file_has_one_lock_whatever_its_name() {
    let dir = scratch("lock_names");
    let x = dir.join("x.wwt");
    let link = dir.join("link.wwt");
    let hard = dir.join("hard.wwt");
    let held = FileSink::append(&x).unwrap();
    std::os::unix::fs::symlink(&x, &link).unwrap();
    std::fs::hard_link(&x, &hard).unwrap();
    for name in [&link, &hard] {
        let r = FileSink::append(name);
        assert!(
            matches!(r, Err(ww_tape_fs::FsError::Locked(_))),
            "{}: {:?}",
            name.display(),
            r.err()
        );
        assert_eq!(try_op_in_child(name, "append"), "locked");
    }
    drop(held);
    assert_eq!(try_op_in_child(&hard, "append"), "free");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A FIFO under the file's name is refused at once, by every way a sink opens a file, never waited on.
#[cfg(unix)]
#[test]
fn a_fifo_is_refused_without_waiting() {
    let dir = scratch("lock_fifo");
    let path = dir.join("a.wwt");
    let made = Command::new("mkfifo")
        .arg(&path)
        .env_clear()
        .status()
        .expect("mkfifo, a POSIX utility, makes the FIFO");
    assert!(made.success());
    let refused = |r: Result<(), ww_tape_fs::FsError>| {
        assert!(
            matches!(&r, Err(ww_tape_fs::FsError::NotAFile(p)) if *p == path),
            "{:?}",
            r.err()
        );
    };
    refused(FileSink::append(&path).map(drop));
    refused(FileSink::open_whole(&path).map(drop));
    refused(FileSink::reopen(&path).map(drop));
    refused(FileSink::create_whole(&path).map(drop));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A file whose name takes the whole 255 bytes a name may have is locked as any other: nothing is named after it.
#[test]
fn a_name_of_the_longest_length_is_locked() {
    let dir = scratch("lock_long");
    let path = dir.join(format!("{}.wwt", "a".repeat(251)));
    let held = FileSink::append(&path).unwrap();
    let r = FileSink::append(&path);
    assert!(
        matches!(r, Err(ww_tape_fs::FsError::Locked(_))),
        "{:?}",
        r.err()
    );
    assert_eq!(try_op_in_child(&path, "append"), "locked");
    drop(held);
    assert_eq!(names_in(&dir).len(), 1);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A child forked while a writer holds its file, and held before it runs `exec`, holds no lock: the writer's resume
/// right after the writer is dropped takes the file. Without the fork handler the child would share the `flock`
/// until its `exec`.
#[cfg(unix)]
#[test]
fn a_child_held_between_fork_and_exec_holds_no_lock() {
    use std::io::{Read, Write};
    use std::os::unix::process::CommandExt;
    let dir = scratch("lock_fork");
    let path = dir.join("a.wwt");
    let w = ww_tape_fs::writer::create(&path, SyncPolicy::None).unwrap();
    let (mut release_rx, mut release_tx) = std::io::pipe().unwrap();
    let (mut forked_rx, mut forked_tx) = std::io::pipe().unwrap();
    let mut cmd = Command::new("true");
    cmd.env_clear().stdin(Stdio::null());
    // SAFETY: the closure runs in the forked child before exec and makes only two system calls on descriptors it
    // owns (write, then read), both async-signal-safe; it allocates and locks nothing.
    unsafe {
        cmd.pre_exec(move || {
            forked_tx.write_all(b"f")?;
            let mut b = [0u8; 1];
            release_rx.read_exact(&mut b)?;
            Ok(())
        });
    }
    // spawn waits for the exec, which waits for the release
    let spawned = std::thread::spawn(move || cmd.spawn());
    let mut b = [0u8; 1];
    forked_rx.read_exact(&mut b).unwrap();
    // the child is forked and holds a copy of every descriptor the writer has
    drop(w);
    let again = ww_tape_fs::FileSink::reopen(&path);
    release_tx.write_all(b"r").unwrap();
    let mut child = spawned.join().unwrap().unwrap();
    assert!(child.wait().unwrap().success());
    assert!(matches!(again, Ok(Some(_))), "{:?}", again.err());
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A closed piece is all there is: a copy of it reads and is written on as the original.
#[test]
fn a_copy_of_a_piece_is_the_piece() {
    let dir = scratch("lock_copy");
    let path = dir.join("p.wwt");
    let mut w = ww_tape_fs::writer::create(&path, SyncPolicy::None).unwrap();
    for n in 0..4 {
        let block = crate::pieces::records(w.next_id(), n);
        w.write_block(crate::definitions::base_defs(), &block)
            .unwrap();
    }
    w.close().unwrap();
    assert_eq!(names_in(&dir), [PathBuf::from("p.wwt")]);
    let bytes = std::fs::read(&path).unwrap();
    let read = |p: &Path| {
        let src = ww_tape_fs::FileSource::open(p).unwrap();
        let r = ww_tape::TapeReader::open(src).unwrap();
        (r.pieces().to_vec(), r.verify(0).map(|v| v.holds).unwrap())
    };
    let want = read(&path);
    assert!(want.1);
    let copy = dir.join("copy.wwt");
    std::fs::copy(&path, &copy).unwrap();
    assert_eq!(read(&copy), want);
    let (w, _) = ww_tape_fs::writer::resume(&copy, ww_tape::WriterOptions::default()).unwrap();
    w.close().unwrap();
    assert_eq!(std::fs::read(&copy).unwrap()[..bytes.len()], bytes[..]);
    std::fs::remove_dir_all(&dir).unwrap();
}
