//! A piece closed more than once, its last append cut and mutated by the input: the reader takes the newest whole
//! close, and a resume keeps every close before the append and closes whole.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| ww_tape_fuzz::targets::reclose(data));
