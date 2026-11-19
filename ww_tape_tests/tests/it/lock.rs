//! One writer per file across processes: a file sink's lock holds against another process, and readers of the file
//! in the writer's process don't let it go.

use std::path::Path;
use std::process::{Command, Stdio};

use ww_tape::SyncPolicy;
use ww_tape_fs::FileSink;
use ww_tape_fs::lock_name;

use crate::pieces::scratch;

/// The file [lock_child] tries to lock, when the test binary runs as one.
const CHILD_ENV: &str = "WW_TAPE_LOCK_CHILD";
/// How [lock_child] opens it: `reopen` (the unclosed file) or `append`.
const CHILD_OP_ENV: &str = "WW_TAPE_LOCK_OP";

/// Not a test of its own: the body of the process [try_in_child] starts. It opens the file named in [CHILD_ENV] as
/// [CHILD_OP_ENV] says and prints whether its lock was free.
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

/// Whether another process finds the lock of the unclosed file `path` free, held or the file missing.
fn try_in_child(path: &Path) -> String {
    try_op_in_child(path, "reopen")
}

/// Whether another process that opens `path` by `op` (`reopen` or `append`) finds its lock free or held.
fn try_op_in_child(path: &Path, op: &str) -> String {
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["lock::lock_child", "--exact", "--nocapture"])
        .env_clear()
        .env(CHILD_ENV, path)
        .env(CHILD_OP_ENV, op)
        .stdin(Stdio::null())
        .output()
        .unwrap();
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

/// A writer's lock holds against another process while the writer's process opens and closes the file it writes,
/// and is free once the writer is dropped; the lock file goes with it.
#[test]
fn a_writer_holds_its_file_against_another_process() {
    let dir = scratch("lock_process");
    let path = dir.join("a.wwt");
    let w = ww_tape_fs::writer::create(&path, SyncPolicy::None).unwrap();
    // readers in the writer's process open and close the file: a record lock on it would go with the first close
    for _ in 0..3 {
        drop(std::fs::File::open(w.sink().current_path()).unwrap());
    }
    assert_eq!(try_in_child(&path), "locked");
    drop(w);
    assert!(
        !lock_name(&path).exists(),
        "the lock file goes with its lock"
    );
    assert_eq!(try_in_child(&path), "free");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// One file has one lock whatever name it is reached by: a link to it, a hard link (in this process) or the
/// `.partial` name of a file being created, in this process and in another.
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
    }
    assert_eq!(try_op_in_child(&link, "append"), "locked");
    drop(held);

    let y = dir.join("y.wwt");
    let creating = FileSink::create(&y).unwrap();
    let partial = ww_tape_fs::partial_name(&y);
    let r = FileSink::append(&partial);
    assert!(
        matches!(r, Err(ww_tape_fs::FsError::Locked(_))),
        "{:?}",
        r.err()
    );
    assert_eq!(try_op_in_child(&partial, "append"), "locked");
    drop(creating);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A lock file that is a link is refused, never followed: a link to another file doesn't lock it, and a dangling one
/// doesn't create its target.
#[cfg(unix)]
#[test]
fn a_lock_file_that_is_a_link_is_refused() {
    let dir = scratch("lock_link");
    let path = dir.join("a.wwt");
    let other = dir.join("other.wwt");
    std::fs::write(&other, b"").unwrap();
    std::os::unix::fs::symlink(&other, lock_name(&path)).unwrap();
    let r = FileSink::append(&path);
    assert!(
        matches!(&r, Err(ww_tape_fs::FsError::NotAFile(p)) if *p == lock_name(&path)),
        "{:?}",
        r.err()
    );
    // the other file's lock is free
    drop(FileSink::append(&other).unwrap());

    std::fs::remove_file(lock_name(&path)).unwrap();
    let target = dir.join("target");
    std::os::unix::fs::symlink(&target, lock_name(&path)).unwrap();
    let r = FileSink::append(&path);
    assert!(
        matches!(&r, Err(ww_tape_fs::FsError::NotAFile(_))),
        "{:?}",
        r.err()
    );
    assert!(
        !target.exists(),
        "a dangling lock file link creates nothing"
    );
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
    refused(FileSink::create_whole(&path).map(drop));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A file whose name leaves no room for `.lock` (up to the 255-byte limit of a name) is locked under a hashed
/// name in its directory, in this process and in another.
#[test]
fn a_name_too_long_for_its_lock_file_is_locked_under_a_hash() {
    let dir = scratch("lock_long");
    let path = dir.join(format!("{}.wwt", "a".repeat(251)));
    let lock = lock_name(&path);
    assert!(lock.file_name().unwrap().len() <= 255, "{}", lock.display());
    assert_eq!(lock.parent(), Some(dir.as_path()));
    let held = FileSink::append(&path).unwrap();
    assert!(lock.exists());
    let r = FileSink::append(&path);
    assert!(
        matches!(r, Err(ww_tape_fs::FsError::Locked(_))),
        "{:?}",
        r.err()
    );
    assert_eq!(try_op_in_child(&path, "append"), "locked");
    drop(held);
    assert!(!lock.exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A child forked while a writer holds its file, and held before it runs `exec`, holds no lock: the writer's resume
/// right after the writer is dropped takes the file. An `flock`, which the child shares, refuses it.
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

/// The lock file holds nothing anyone needs: a closed piece leaves none behind on unix, a copy of the piece without it
/// reads and is written on as the original, and a stale lock file, whatever it holds, blocks no writer and changes
/// nothing a reader sees.
#[test]
fn the_lock_file_holds_nothing_anyone_needs() {
    let dir = scratch("lock_holds_nothing");
    let path = dir.join("p.wwt");
    let mut w = ww_tape_fs::writer::create(&path, SyncPolicy::None).unwrap();
    for n in 0..4 {
        let block = crate::pieces::records(w.next_id(), n);
        w.write_block(crate::definitions::base_defs(), &block)
            .unwrap();
    }
    w.close().unwrap();
    if cfg!(unix) {
        assert!(!lock_name(&path).exists(), "removed at the close");
    }
    let bytes = std::fs::read(&path).unwrap();
    let read = |p: &Path| {
        let src = ww_tape_fs::FileSource::open(p).unwrap();
        let r = ww_tape::TapeReader::open(src).unwrap();
        (r.pieces().to_vec(), r.verify(0).unwrap())
    };
    let want = read(&path);
    assert!(want.1);
    // a copy without its lock file
    let copy = dir.join("copy.wwt");
    std::fs::copy(&path, &copy).unwrap();
    assert_eq!(read(&copy), want);
    let (w, _) = ww_tape_fs::writer::resume(&copy, ww_tape::WriterOptions::default()).unwrap();
    w.close().unwrap();
    assert_eq!(std::fs::read(&copy).unwrap()[..bytes.len()], bytes[..]);
    // a stale lock file with bytes in it: no writer holds it
    std::fs::write(lock_name(&path), b"stale").unwrap();
    assert_eq!(read(&path), want, "a reader never opens the lock file");
    let (w, _) = ww_tape_fs::writer::resume(&path, ww_tape::WriterOptions::default())
        .expect("a stale lock file blocks no writer");
    w.close().unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}
