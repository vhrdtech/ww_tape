//! The program of the chapter "A first tape" prints what the page shows (`first_tape.out`, included there), and the
//! file it writes is a closed piece that verifies.
#![allow(clippy::expect_used, reason = "tests fail by panicking")]

use std::path::Path;

use ww_tape::TapeReader;
use ww_tape_example::Drive;
use ww_tape_example::first_tape::{FirstTapeError, run};

#[test]
fn the_first_tape_prints_what_the_page_shows() -> Result<(), FirstTapeError> {
    let dir = std::env::temp_dir().join(format!("ww_tape_example-first-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(FirstTapeError::Io)?;
    let path = dir.join("drive.wwt");
    _ = std::fs::remove_file(&path);
    let report = run(&path, &Drive::full().minutes(1))?;
    let want =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/first_tape.out"))
            .map_err(FirstTapeError::Io)?;
    assert_eq!(
        report, want,
        "tests/first_tape.out is what the program prints: copy it from `cargo run -p ww_tape_example --bin first_tape`"
    );

    let bytes = std::fs::read(&path).map_err(FirstTapeError::Io)?;
    let reader = TapeReader::open(&bytes[..])?;
    assert_eq!(reader.pieces().len(), 1, "one piece");
    assert!(reader.verify(0)?.holds, "the piece holds its content hash");
    assert_eq!(bytes.len() % 4096, 0, "a closed piece ends on 4 KiB");
    std::fs::remove_dir_all(&dir).map_err(FirstTapeError::Io)?;
    Ok(())
}
