//! A ww_tape series track and series chunks of hostile first samples, ticks, grids and scales
//! (ww_tape_fuzz::hostile::series), built into a timeline and queried.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| ww_tape_fuzz::targets::series_hostile(data));
