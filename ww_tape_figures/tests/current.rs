//! Every figure in `docs/plots/` is the one `ww_tape_figures` draws from the format as it is.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::unreachable,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::print_stderr,
    clippy::print_stdout,
    reason = "tests fail by panicking, compute expected values freely and print what they saw"
)]

use std::path::Path;

#[test]
fn figures_in_docs_are_current() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/plots");
    let mut stale = Vec::new();
    for (name, svg) in ww_tape_figures::figures().unwrap() {
        let path = dir.join(&name);
        if std::fs::read_to_string(&path).ok().as_deref() != Some(svg.as_str()) {
            stale.push(name);
        }
    }
    assert!(
        stale.is_empty(),
        "docs/plots/{stale:?} differ from what ww_tape_figures draws: run `just book-figures` and check them"
    );
}

#[test]
fn prose_numbers_come_from_reports() {
    let page =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/sizes.md"))
            .unwrap();
    let problems = ww_tape_figures::bench_says().unwrap().check(&page);
    assert!(
        problems.is_empty(),
        "docs/sizes.md writes numbers the bench reports don't hold:\n{}",
        problems.join("\n")
    );
}

#[test]
fn writing_thread_numbers_come_from_reports() {
    let page = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/writing_thread.md"),
    )
    .unwrap();
    let problems = ww_tape_figures::handles_missing(&page).unwrap();
    assert!(
        problems.is_empty(),
        "docs/writing_thread.md writes numbers the handles report doesn't hold:\n{}",
        problems.join("\n")
    );
}

#[test]
fn damage_numbers_come_from_reports() {
    let page =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/damage.md"))
            .unwrap();
    let problems = ww_tape_figures::damage_missing(&page).unwrap();
    assert!(
        problems.is_empty(),
        "docs/damage.md writes numbers the read_check report doesn't hold:\n{}",
        problems.join("\n")
    );
}

#[test]
fn integrity_numbers_come_from_reports() {
    let page =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/integrity.md"))
            .unwrap();
    let problems = ww_tape_figures::integrity_missing(&page).unwrap();
    assert!(
        problems.is_empty(),
        "docs/integrity.md writes numbers the levels report doesn't hold:\n{}",
        problems.join("\n")
    );
}
