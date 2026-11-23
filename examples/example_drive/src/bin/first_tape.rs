//! `first_tape [DIR]`: write the drive's first minute into `DIR/drive.wwt`, close it, open it and print its tracks: the
//! program of the book's chapter "A first tape". `DIR` is `ww_tape_first_tape` in the system's temporary directory by
//! default; a `drive.wwt` already there is replaced.
#![allow(clippy::print_stdout, reason = "the binary prints the tracks")]

use std::path::PathBuf;

use example_drive::Drive;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = std::env::args_os().nth(1).map_or_else(
        || std::env::temp_dir().join("ww_tape_first_tape"),
        PathBuf::from,
    );
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("drive.wwt");
    // the writer refuses to create a file that is there
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    let report = example_drive::first_tape::run(&path, &Drive::full().minutes(1))?;
    println!("wrote {}", path.display());
    print!("{report}");
    Ok(())
}
