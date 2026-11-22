//! A ww_tape series track and a series chunk of hostile flag runs (ww_tape_fuzz::hostile::flags), read, salvaged and
//! summarised.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| ww_tape_fuzz::targets::flags_hostile(data));
