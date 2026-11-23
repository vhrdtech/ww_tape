//! ww_tape grids on any bytes: grid definitions, tile layouts, tile numbers and boxes (ww_tape_fuzz::targets::grid).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| ww_tape_fuzz::targets::grid(data));
