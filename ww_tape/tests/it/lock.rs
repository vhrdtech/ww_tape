//! One writer per file across processes: a file sink's lock holds against another process, and readers of the file
//! in the writer's process don't let it go.

use std::path::Path;
use std::process::{Command, Stdio};

use ww_tape::sink::lock_name;
use ww_tape::{FileSink, SyncPolicy, TapeWriter};

use crate::pieces::scratch;

/// The file [lock_child] tries to lock, when the test binary runs as one.
const CHILD_ENV: &str = "WW_TAPE_LOCK_CHILD";
/// How [lock_child] opens it: `reopen` (the unfinished file) or `append`.
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
        Err(ww_tape::FsError::Locked(_)) => "locked",
        Err(e) => panic!("{e}"),
    };
    println!("lock-child: {verdict}");
}

/// Whether another process finds the lock of the unfinished file `path` free, held or the file missing.
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
    let w = TapeWriter::create(&path, SyncPolicy::None).unwrap();
    // readers in the writer's process open and close the file: a record lock on it would go with the first close
    for _ in 0..3 {
        drop(std::fs::File::open(w.current_path()).unwrap());
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
            matches!(r, Err(ww_tape::FsError::Locked(_))),
            "{}: {:?}",
            name.display(),
            r.err()
        );
    }
    assert_eq!(try_op_in_child(&link, "append"), "locked");
    drop(held);

    let y = dir.join("y.wwt");
    let creating = FileSink::create(&y).unwrap();
    let partial = ww_tape::sink::partial_name(&y);
    let r = FileSink::append(&partial);
    assert!(
        matches!(r, Err(ww_tape::FsError::Locked(_))),
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
        matches!(&r, Err(ww_tape::FsError::NotAFile(p)) if *p == lock_name(&path)),
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
        matches!(&r, Err(ww_tape::FsError::NotAFile(_))),
        "{:?}",
        r.err()
    );
    assert!(
        !target.exists(),
        "a dangling lock file link creates nothing"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
