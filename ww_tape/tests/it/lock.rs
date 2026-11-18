//! One writer per file across processes: a file sink's lock holds against another process, and readers of the file
//! in the writer's process don't let it go.

use std::path::Path;
use std::process::{Command, Stdio};

use ww_tape::sink::lock_name;
use ww_tape::{FileSink, SyncPolicy, TapeWriter};

use crate::pieces::scratch;

/// The file [lock_child] tries to lock, when the test binary runs as one.
const CHILD_ENV: &str = "WW_TAPE_LOCK_CHILD";

/// Not a test of its own: the body of the process [try_in_child] starts. It reopens the unfinished file named in
/// [CHILD_ENV] and prints whether its lock was free.
#[test]
#[expect(
    clippy::print_stdout,
    reason = "the child reports its verdict to the test that started it"
)]
fn lock_child() {
    let Some(path) = std::env::var_os(CHILD_ENV) else {
        return;
    };
    let verdict = match FileSink::reopen(Path::new(&path)) {
        Ok(Some(_)) => "free",
        Ok(None) => "missing",
        Err(ww_tape::FsError::Locked(_)) => "locked",
        Err(e) => panic!("{e}"),
    };
    println!("lock-child: {verdict}");
}

/// Whether another process finds the lock of the unfinished file `path` free, held or the file missing.
fn try_in_child(path: &Path) -> String {
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["lock::lock_child", "--exact", "--nocapture"])
        .env_clear()
        .env(CHILD_ENV, path)
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
