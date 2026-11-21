//! The bench binaries through their command line: a short run of `starts` with `--json` read back as a report,
//! `--version`, shell completion and a `--json` file whose directory is missing.
#![allow(
    clippy::unwrap_used,
    reason = "tests fail by panicking, their helpers too"
)]

use std::path::PathBuf;
use std::process::{Command, Output};
use ww_tape_bench::Report;

/// `bin` with `args`, without the `CARGO_*` variables of the test run (`CARGO_BUILD_JOBS` among them).
fn run(bin: &str, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut c = Command::new(bin);
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("CARGO") {
            c.env_remove(k);
        }
    }
    c.args(args).envs(env.iter().copied()).output().unwrap()
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ww_tape_bench_cli-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn starts_writes_a_report_that_reads_back() {
    let dir = scratch("starts");
    let json = dir.join("starts.json");
    let out = run(
        env!("CARGO_BIN_EXE_starts"),
        &[
            "--items",
            "4096",
            "--rounds",
            "1",
            "--color",
            "never",
            "--json",
            json.to_str().unwrap(),
        ],
        &[("CARGO_BUILD_JOBS", "3")],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("| `EventTimes::read`, intact |"),
        "{stdout}"
    );
    let r = Report::read(&json).unwrap();
    assert_eq!(r.bench, "starts");
    assert_eq!(r.setting("items"), Some("4096"));
    assert_eq!(r.build.jobs, Some(3));
    assert!(!r.machine.host.is_empty(), "the host is named");
    let t = r.get("starts").unwrap();
    assert_eq!(t.rows.len(), 4);
    assert!(
        t.cell("`Starts::get` each item, salvaged", "ns per item")
            .and_then(|c| c.value())
            .is_some()
    );
    if cfg!(debug_assertions) {
        assert_eq!(r.build.profile, "debug");
        assert!(r.measured().is_err(), "a debug build's times are refused");
        assert!(stdout.contains("its times mean nothing"), "{stdout}");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_missing_json_directory_fails_before_measuring() {
    let out = run(
        env!("CARGO_BIN_EXE_starts"),
        &["--json", "/nonexistent/ww_tape/x.json"],
        &[],
    );
    assert!(!out.status.success());
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(
        err.contains("--json /nonexistent/ww_tape/x.json: no directory"),
        "{err}"
    );
    assert!(out.stdout.is_empty(), "nothing measured");
}

#[test]
fn version_and_completion() {
    let out = run(env!("CARGO_BIN_EXE_drive"), &["--version"], &[]);
    let v = String::from_utf8(out.stdout).unwrap();
    assert!(v.starts_with("drive 0."), "{v}");
    assert!(v.contains(", commit "), "{v}");
    let out = run(
        env!("CARGO_BIN_EXE_event_times"),
        &[],
        &[("COMPLETE", "bash")],
    );
    assert!(out.status.success());
    assert!(!out.stdout.is_empty(), "a completion script");
}
