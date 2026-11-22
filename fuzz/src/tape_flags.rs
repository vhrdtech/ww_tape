//! A ww_tape series chunk's flag runs on any bytes, and flag runs built from pushes (ww_tape_fuzz::targets::flags).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| ww_tape_fuzz::targets::flags(data));
