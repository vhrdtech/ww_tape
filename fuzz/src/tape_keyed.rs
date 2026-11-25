//! ww_tape keyed state on any bytes: keyed items, key ids, keyed blocks and keyed index entries
//! (ww_tape_fuzz::targets::keyed).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| ww_tape_fuzz::targets::keyed(data));
