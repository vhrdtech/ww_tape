//! Groups of puts a ww_tape writer writes, cut at any byte (ww_tape_fuzz::targets::runs): a scan, a follower and a
//! resume take each group whole or not at all.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| ww_tape_fuzz::targets::runs(data));
