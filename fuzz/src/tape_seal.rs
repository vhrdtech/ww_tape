//! A closed piece or chain of a level below Full, mutated: a seal never panics and gives Full's bytes or refuses.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| ww_tape_fuzz::targets::seal(data));
