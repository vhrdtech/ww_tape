//! A ww_tape grid track of hostile axes and grid chunks of hostile tile coordinates
//! (ww_tape_fuzz::hostile::grid), read as blocks and as a tape.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| ww_tape_fuzz::targets::grid_hostile(data));
