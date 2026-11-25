//! The program of the chapter "A first tape" prints what the page shows (`docs/generated/first_tape.txt`, included
//! there), and the file it writes is a closed piece that verifies.
#![allow(clippy::expect_used, reason = "tests fail by panicking")]

use example_drive::Drive;
use example_drive::first_tape::{FirstTapeError, run};
use ww_tape::TapeReader;

#[test]
fn book_output_first_tape() -> Result<(), FirstTapeError> {
    let dir = std::env::temp_dir().join(format!("example_drive-first-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(FirstTapeError::Io)?;
    let path = dir.join("drive.wwt");
    _ = std::fs::remove_file(&path);
    let report = run(&path, &Drive::full().minutes(1))?;
    ww_tape_book_outputs::check("first_tape.txt", &report);

    let bytes = std::fs::read(&path).map_err(FirstTapeError::Io)?;
    let reader = TapeReader::open(&bytes[..])?;
    assert_eq!(reader.pieces().len(), 1, "one piece");
    assert!(reader.verify(0)?.holds, "the piece holds its content hash");
    assert_eq!(bytes.len() % 4096, 0, "a closed piece ends on 4 KiB");
    std::fs::remove_dir_all(&dir).map_err(FirstTapeError::Io)?;
    Ok(())
}
