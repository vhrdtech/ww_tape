//! The tuning session prints what the book shows (`docs/generated/settings_*.txt`), and what it reads back is what it
//! wrote: the keys carried into the last piece, the history of one key and the firmware image whole.
#![allow(clippy::expect_used, reason = "tests fail by panicking")]

use example_settings::{FIRMWARE_LEN, firmware_image, run};

#[test]
fn book_output_settings() {
    let report = run().expect("the session runs");
    ww_tape_book_outputs::check("settings_puts.txt", &report.puts_text().expect("text"));
    ww_tape_book_outputs::check(
        "settings_firmware.txt",
        &report.firmware_text().expect("text"),
    );
    ww_tape_book_outputs::check("settings_blocks.txt", &report.blocks_text().expect("text"));
    ww_tape_book_outputs::check("settings_keys.txt", &report.keys_text().expect("text"));
}

#[test]
fn the_last_piece_alone_holds_every_live_key() {
    let report = run().expect("the session runs");
    let params: Vec<(&str, &str)> = report
        .params
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    assert_eq!(
        params,
        [
            ("limit/current_ma", "15000"),
            ("pid/ki", "0.09"),
            ("pid/kp", "1.20")
        ]
    );
    assert!(report.params_whole);
    assert_eq!(report.profile.as_deref(), Some("fast"));
    assert_eq!(report.pieces, 2);
}

#[test]
fn a_repeated_value_writes_nothing_and_a_group_writes_whole() {
    let report = run().expect("the session runs");
    let written: Vec<bool> = report.puts.iter().map(|p| p.written).collect();
    assert_eq!(
        written,
        [true, true, true, true, true, false, true, true, true]
    );
    let kp: Vec<_> = report
        .kp_history
        .iter()
        .map(|c| (c.ms, c.value.as_deref()))
        .collect();
    assert_eq!(kp, [(3_000, Some("1.20")), (0, Some("0.80"))]);
}

#[test]
fn the_firmware_reads_back_whole() {
    let report = run().expect("the session runs");
    assert_eq!(report.firmware.len, FIRMWARE_LEN);
    // 200 000 bytes in parts of 64 KiB: three whole parts, a last of 3 392 bytes, and the seal
    assert_eq!(report.firmware.parts, 5);
    assert_eq!(report.firmware_read, firmware_image());
}

#[test]
fn the_same_session_gives_the_same_report() {
    assert_eq!(run().expect("runs"), run().expect("runs"));
}
