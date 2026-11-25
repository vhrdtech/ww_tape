//! The book's frame (`book_checks.toml`) through `vz check`, the book kit's check: the book as it is passes, a copy
//! with a page stepping down a level or a bare `rust` fence fails.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The repository root, above this crate.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

/// The kit the justfile pins (`kit := "..."`), so the test checks with the book's own version.
fn kit() -> String {
    let justfile = std::fs::read_to_string(root().join("justfile")).unwrap();
    let line = justfile
        .lines()
        .find_map(|l| l.strip_prefix("kit := "))
        .expect("the justfile pins the kit as `kit := \"...\"`");
    line.trim_matches('"').to_owned()
}

/// `vz check --root book` through uv. A missing uv fails the test: a check that cannot run never passes.
fn vz_check(book: &Path) -> Output {
    let run = Command::new("uv")
        .args([
            "run",
            "-q",
            "--with",
            &kit(),
            "vz",
            "check",
            "--color",
            "never",
            "--root",
        ])
        .arg(book)
        .env_remove("VIRTUAL_ENV")
        .output();
    match run {
        Ok(out) => out,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            panic!(
                "skipped: vz check of the book's frame, uv missing: install uv (https://docs.astral.sh/uv/)"
            )
        }
        Err(e) => panic!("uv: {e}"),
    }
}

/// A copy of what `vz check` reads: zensical.toml, book_checks.toml and the pages of docs/.
fn copy_book(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("ww_tape_book_frame_{name}_{}", std::process::id()));
    _ = std::fs::remove_dir_all(&dir);
    for file in ["zensical.toml", "book_checks.toml"] {
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(root().join(file), dir.join(file)).unwrap();
    }
    copy_dir(&root().join("docs"), &dir.join("docs"));
    dir
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&path, &to.join(entry.file_name()));
        } else {
            std::fs::copy(&path, to.join(entry.file_name())).unwrap();
        }
    }
}

/// The report of a check that must fail, with its failure asserted.
fn failing(book: &Path) -> String {
    let out = vz_check(book);
    let report = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "vz check passed a planted fault:\n{report}"
    );
    report
}

#[test]
fn book_frame_holds() {
    let out = vz_check(&root());
    assert!(
        out.status.success(),
        "vz check:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn book_frame_fails_a_step_down() {
    let book = copy_book("step_down");
    // anatomy.md closes "How a tape is built" in the Basic part: as an Advanced page it steps down to writing.md.
    let page = book.join("docs/anatomy.md");
    let text = std::fs::read_to_string(&page).unwrap();
    let planted = text.replacen("level: basic", "level: advanced", 1);
    assert_ne!(planted, text, "anatomy.md has `level: basic`");
    std::fs::write(&page, planted).unwrap();
    let report = failing(&book);
    assert!(
        report.contains("anatomy.md"),
        "the report names the page:\n{report}"
    );
    std::fs::remove_dir_all(&book).unwrap();
}

#[test]
fn book_frame_fails_a_bare_fence() {
    let book = copy_book("bare_fence");
    // tape.md holds no waived fence: one written into the page, not included from a compiled source, fails.
    let page = book.join("docs/tape.md");
    let mut text = std::fs::read_to_string(&page).unwrap();
    text.push_str("\n```rust\nfn main() {}\n```\n");
    std::fs::write(&page, text).unwrap();
    let report = failing(&book);
    assert!(
        report.contains("tape.md"),
        "the report names the page:\n{report}"
    );
    std::fs::remove_dir_all(&book).unwrap();
}
