//! A ww_tape chain of bytes track values whose parts, seals and index entries lie behind good CRCs
//! (ww_tape_fuzz::hostile::bytes): a value reads whole only as written, a torn one never.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| ww_tape_fuzz::targets::bytes_hostile(data));
