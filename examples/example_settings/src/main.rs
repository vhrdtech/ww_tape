//! The tuning session, headless: `cargo run -p example_settings` writes it, reads it back and prints what the book's
//! chapters on tracks and state show.
#![allow(
    clippy::print_stdout,
    reason = "the binary prints the session's report"
)]

fn main() -> Result<(), Box<dyn std::error::Error>> {
    print!("{}", example_settings::run()?);
    Ok(())
}
